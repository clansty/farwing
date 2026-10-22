//! Bounded wallpaper memory/disk cache; downloads never use account credentials.
mod disk;
use anyhow::{Context, Result, bail};
use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    path::Path,
    sync::mpsc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Source {
    pub device_id: String,
    pub url: String,
}
impl Source {
    pub fn new(device_id: &str, url: &str) -> Self {
        Self {
            device_id: device_id.into(),
            url: url.into(),
        }
    }
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallpaperSource").finish_non_exhaustive()
    }
}

/// 低分辨率亮度表。详情页要把文字直接压在壁纸上，深浅只能按图判断，
/// 而取景区（uv）是绘制时才知道的，所以这里只留一张粗网格供视图自己查。
#[derive(Default)]
struct Luma {
    size: [usize; 2],
    cells: Vec<f32>,
}

impl Luma {
    const COLS: usize = 6;
    const ROWS: usize = 6;

    fn new(image: &egui::ColorImage) -> Self {
        let [width, height] = image.size;
        let mut cells = vec![0.0; Self::COLS * Self::ROWS];
        if width == 0 || height == 0 {
            return Self::default();
        }
        let mut counts = vec![0u32; cells.len()];
        for y in 0..height {
            let row = y * Self::ROWS / height;
            for x in 0..width {
                let pixel = image.pixels[y * width + x];
                let cell = row * Self::COLS + x * Self::COLS / width;
                // Rec.601 亮度：判断「白底还是黑底」够用，也比线性化便宜。
                cells[cell] += (0.299 * f32::from(pixel.r())
                    + 0.587 * f32::from(pixel.g())
                    + 0.114 * f32::from(pixel.b()))
                    / 255.0;
                counts[cell] += 1;
            }
        }
        for (cell, count) in cells.iter_mut().zip(counts) {
            if count > 0 {
                *cell /= count as f32;
            }
        }
        Self {
            size: [width, height],
            cells,
        }
    }

    /// uv 覆盖范围（0..1 归一化）内的平均亮度；没有交集时返回 None。
    fn mean(&self, uv: egui::Rect) -> Option<f32> {
        let [width, height] = self.size;
        if width == 0 || height == 0 {
            return None;
        }
        let mut sum = 0.0;
        let mut weight = 0.0;
        for row in 0..Self::ROWS {
            for col in 0..Self::COLS {
                let cell = egui::Rect::from_min_max(
                    egui::pos2(
                        col as f32 / Self::COLS as f32,
                        row as f32 / Self::ROWS as f32,
                    ),
                    egui::pos2(
                        (col + 1) as f32 / Self::COLS as f32,
                        (row + 1) as f32 / Self::ROWS as f32,
                    ),
                );
                let overlap = cell.intersect(uv);
                let area = overlap.width().max(0.0) * overlap.height().max(0.0);
                if area > 0.0 {
                    sum += self.cells[row * Self::COLS + col] * area;
                    weight += area;
                }
            }
        }
        (weight > 0.0).then(|| sum / weight)
    }
}

struct Entry {
    url: String,
    ticket: u64,
    touched: u64,
    texture: Option<egui::TextureHandle>,
    luma: Option<Luma>,
    failed: bool,
}

struct Loaded {
    id: String,
    ticket: u64,
    pixels: Option<egui::ColorImage>,
    complete: bool,
}

pub(crate) struct Wallpapers {
    entries: HashMap<String, Entry>,
    tasks: HashMap<u64, CancellationToken>,
    sender: mpsc::Sender<Loaded>,
    receiver: mpsc::Receiver<Loaded>,
    refresh: HashSet<String>,
    next: u64,
    clock: u64,
}

impl Default for Wallpapers {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            entries: HashMap::new(),
            tasks: HashMap::new(),
            sender,
            receiver,
            refresh: HashSet::new(),
            next: 0,
            clock: 0,
        }
    }
}

impl Wallpapers {
    pub fn clear(&mut self) {
        for cancel in self.tasks.values() {
            cancel.cancel();
        }
        self.entries.clear();
        self.refresh.clear();
    }

    pub fn invalidate(&mut self, id: &str) {
        if let Some(entry) = self.entries.remove(id)
            && let Some(cancel) = self.tasks.get(&entry.ticket)
        {
            cancel.cancel();
        }
    }

    pub fn refresh(&mut self, id: &str) {
        self.invalidate(id);
        self.refresh.insert(id.into());
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(Loaded {
            id,
            ticket,
            pixels,
            complete,
        }) = self.receiver.try_recv()
        {
            if complete {
                self.tasks.remove(&ticket);
            }
            if let Some(entry) = self.entries.get_mut(&id)
                && entry.ticket == ticket
            {
                if let Some(pixels) = pixels {
                    entry.luma = Some(Luma::new(&pixels));
                    entry.texture = Some(ctx.load_texture(
                        format!("device-wallpaper-{ticket}"),
                        pixels,
                        egui::TextureOptions::LINEAR,
                    ));
                }
                entry.failed = complete && entry.texture.is_none();
            }
        }
    }

    pub fn failed(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.failed)
    }

    /// 壁纸在给定 uv 区域（0..1）上的平均亮度，用来判断压在上面的文字该用什么颜色。
    pub fn mean_luma(&self, id: &str, uv: egui::Rect) -> Option<f32> {
        self.entries.get(id)?.luma.as_ref()?.mean(uv)
    }

    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        id: &str,
        url: &str,
    ) -> Option<egui::TextureHandle> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(id)
            && entry.url == url
        {
            entry.touched = self.clock;
            return entry.texture.clone();
        }
        self.invalidate(id);
        if url.is_empty() || self.tasks.len() >= 3 {
            return None;
        }
        if self.entries.len() >= 24
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(id, _)| id.clone())
        {
            self.invalidate(&oldest);
        }
        self.next = self.next.wrapping_add(1);
        let ticket = self.next;
        let cancel = CancellationToken::new();
        self.entries.insert(
            id.into(),
            Entry {
                url: url.into(),
                ticket,
                touched: self.clock,
                texture: None,
                luma: None,
                failed: false,
            },
        );
        self.tasks.insert(ticket, cancel.clone());
        let sender = self.sender.clone();
        let key = id.to_owned();
        let id = key.clone();
        let address = url.to_owned();
        let disk_path = disk::path(&id, url);
        let force_refresh = self.refresh.remove(&id);
        let repaint = ctx.clone();
        let result = std::thread::Builder::new()
            .name("wallpaper".into())
            .spawn(move || {
                let pixels = load(
                    &address,
                    disk_path.as_deref(),
                    force_refresh,
                    &cancel,
                    |pixels| {
                        let _ = sender.send(Loaded {
                            id: id.clone(),
                            ticket,
                            pixels: Some(pixels),
                            complete: false,
                        });
                        repaint.request_repaint();
                    },
                )
                .ok();
                let _ = sender.send(Loaded {
                    id,
                    ticket,
                    pixels,
                    complete: true,
                });
                repaint.request_repaint();
            });
        if result.is_err() {
            self.tasks.remove(&ticket);
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.failed = true;
            }
        }
        None
    }
}

impl Drop for Wallpapers {
    fn drop(&mut self) {
        self.clear();
    }
}

fn decode(bytes: Vec<u8>) -> Result<image::RgbaImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    Ok(if image.width() > 960 || image.height() > 540 {
        image.thumbnail(960, 540)
    } else {
        image
    }
    .into_rgba8())
}

fn color_image(pixels: &image::RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [pixels.width() as usize, pixels.height() as usize],
        pixels.as_raw(),
    )
}

fn load(
    address: &str,
    path: Option<&Path>,
    force_refresh: bool,
    cancel: &CancellationToken,
    preview: impl FnOnce(egui::ColorImage),
) -> Result<egui::ColorImage> {
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    if let Some((pixels, fresh)) = path
        .and_then(|path| disk::read(path).ok())
        .and_then(|(bytes, fresh)| decode(bytes).ok().map(|pixels| (pixels, fresh)))
    {
        if fresh && !force_refresh {
            return Ok(color_image(&pixels));
        }
        // Keep the old image visible if the background refresh fails.
        preview(color_image(&pixels));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let bytes = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("cancelled"),
            result = download(address) => result,
        }
    })?;
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    let pixels = decode(bytes)?;
    if let Some(path) = path {
        let mut png = Cursor::new(Vec::new());
        if pixels.write_to(&mut png, image::ImageFormat::Png).is_ok() {
            let _ = disk::write(path, png.get_ref(), cancel);
        }
    }
    Ok(color_image(&pixels))
}

async fn download(address: &str) -> Result<Vec<u8>> {
    let url = reqwest::Url::parse(address)?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        bail!("unsupported wallpaper URL");
    }
    let client = crate::transport::http_client()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(3))
        .timeout(Duration::from_secs(12))
        .build()?;
    let mut response = crate::transport::http::send(
        crate::transport::http::Operation::WallpaperDownload,
        client.get(url),
    )
    .await?
    .error_for_status()?;
    const LIMIT: usize = 8 * 1024 * 1024;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        bail!("wallpaper too large");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes
            .len()
            .checked_add(chunk.len())
            .context("wallpaper size")?
            > LIMIT
        {
            bail!("wallpaper too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn left_white_right_black() -> egui::ColorImage {
        let size = [128, 128];
        egui::ColorImage::new(
            size,
            (0..size[0] * size[1])
                .map(|i| {
                    if i % size[0] < size[0] / 2 {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::BLACK
                    }
                })
                .collect(),
        )
    }

    /// 亮度表要按查询的区域给值，不能整张图抹成平均。
    #[test]
    fn mean_follows_the_queried_region() {
        let luma = Luma::new(&left_white_right_black());
        let left = luma
            .mean(egui::Rect::from_min_max(
                egui::pos2(0.0, 0.0),
                egui::pos2(0.4, 0.2),
            ))
            .unwrap();
        let right = luma
            .mean(egui::Rect::from_min_max(
                egui::pos2(0.6, 0.0),
                egui::pos2(1.0, 0.2),
            ))
            .unwrap();
        assert!(left > 0.99, "left = {left}");
        assert!(right < 0.01, "right = {right}");
    }

    #[test]
    fn empty_image_has_no_luma() {
        let luma = Luma::new(&egui::ColorImage::filled([0, 0], egui::Color32::WHITE));
        assert!(
            luma.mean(egui::Rect::from_min_max(
                egui::Pos2::ZERO,
                egui::pos2(1.0, 1.0)
            ))
            .is_none()
        );
    }
}
