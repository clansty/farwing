//! Read-only pointer shape sampling through XFixes, independent of desktop
//! pixel capture.
use anyhow::{Context, Result, ensure};
use std::sync::Mutex;
use x11rb::protocol::xfixes::{ConnectionExt as _, GetCursorImageAndNameReply};
use x11rb::rust_connection::RustConnection;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Pointer {
    /// The XFixes cursor serial: changes whenever the shape does.
    pub handle: usize,
    pub showing: bool,
    /// Where the pointer is in the desktop, when this session can say.
    pub position: Option<(i32, i32)>,
}
pub(crate) struct Shape {
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub rgba: Vec<u8>,
    pub kind: i32,
}

/// A pointer shape as the host reports it: PNG, base64-encoded.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Image {
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub kind: i32,
    pub png: String,
}
/// Where the pointer is and what it looks like, sampled with each frame.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Snapshot {
    pub showing: bool,
    /// `None` when this session has no way to know: see `reported_position`.
    pub position: Option<(i32, i32)>,
    pub image: Option<Image>,
}
/// Samples the pointer for the capture, re-encoding its shape only when the
/// cursor changes (or at most every 250 ms), as the Windows sampler does.
#[derive(Default)]
pub(crate) struct Sampler {
    handle: usize,
    image: Option<Image>,
    sampled: Option<std::time::Instant>,
}
impl Sampler {
    pub fn sample(&mut self) -> Result<Snapshot> {
        let pointer = pointer()?;
        if pointer.showing
            && (self.handle != pointer.handle
                || self.image.is_none()
                || self
                    .sampled
                    .is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(250)))
        {
            // 形状这一次读不到，不能把整次采样判成失败：调用方把失败当成「没有光标」，
            // 于是「形状没读到」会变成一次「光标不见了」，下一帧它又出现了——主控那边的
            // smart mouse 就照着这个来回切换把本地光标往采样位置挪。上一次的形状留着用，
            // 指针的样子又不会因为我们读失败而变。隔一会儿再试一次，别再连着重试。
            self.sampled = Some(std::time::Instant::now());
            match self.encode(pointer.handle) {
                Ok(image) => {
                    self.image = Some(image);
                    self.handle = pointer.handle;
                }
                Err(error) => tracing::debug!(%error, "读取光标形状失败"),
            }
        }
        Ok(Snapshot {
            showing: pointer.showing,
            position: pointer.position,
            image: if pointer.showing {
                self.image.clone()
            } else {
                None
            },
        })
    }

    /// 取一次形状并编码成 PNG。
    fn encode(&self, handle: usize) -> Result<Image> {
        use base64::Engine as _;
        use image::ImageEncoder as _;
        let shape = shape(handle)?;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(
            &shape.rgba,
            shape.width,
            shape.height,
            image::ExtendedColorType::Rgba8,
        )?;
        ensure!(png.len() <= 4 * 1024 * 1024, "光标图片过大");
        Ok(Image {
            width: shape.width,
            height: shape.height,
            hotspot: shape.hotspot,
            kind: shape.kind,
            png: base64::engine::general_purpose::STANDARD.encode(png),
        })
    }
}

struct Connection {
    connection: RustConnection,
    last: Option<GetCursorImageAndNameReply>,
}
/// One connection for the session's pointer polling; reopened after an error.
static SAMPLER: Mutex<Option<Connection>> = Mutex::new(None);

fn sample<T>(read: impl FnOnce(&GetCursorImageAndNameReply) -> T) -> Result<T> {
    let mut sampler = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    if sampler.is_none() {
        let (connection, _) = x11rb::connect(None).context("连接 X11 显示失败")?;
        connection
            .xfixes_query_version(4, 0)?
            .reply()
            .context("X 服务器不支持 XFixes")?;
        *sampler = Some(Connection {
            connection,
            last: None,
        });
    }
    let active = sampler.as_mut().unwrap();
    let reply = active
        .connection
        .xfixes_get_cursor_image_and_name()
        .map_err(anyhow::Error::from)
        .and_then(|cookie| cookie.reply().map_err(anyhow::Error::from));
    match reply {
        Ok(reply) => {
            let result = read(&reply);
            active.last = Some(reply);
            Ok(result)
        }
        Err(error) => {
            *sampler = None;
            Err(error.context("读取光标失败"))
        }
    }
}

pub(crate) fn pointer() -> Result<Pointer> {
    sample(|reply| Pointer {
        handle: reply.cursor_serial as usize,
        // XFixes reports a hidden pointer as an empty image.
        showing: reply.width > 0
            && reply.height > 0
            && reply.cursor_image.iter().any(|pixel| pixel >> 24 != 0),
        position: reported_position(reply),
    })
}

/// 指针在桌面上的位置，只有这台机器的 X 服务器**就是**桌面时才敢报。
///
/// Xorg 会话里两者是一回事。Wayland 会话里不是：XWayland 只有指针落在自己的窗口上
/// 才更新这份坐标，报出来的也是那个窗口自己的坐标系（niri 不告诉 X 窗口它在屏幕
/// 上的位置）。实机验证过：用合成器的绝对指针把光标挪到六处相距上千像素的地方，
/// 读回来的一直是同一个值。所以 Wayland 会话上位置是**不知道**——报一个错的比不报
/// 更糟，主控拿到它会照着把本地光标挪过去，正好落在窗口左上角那个偏移上。
fn reported_position(reply: &GetCursorImageAndNameReply) -> Option<(i32, i32)> {
    (!crate::platform::capture::wayland_session())
        .then(|| (i32::from(reply.x), i32::from(reply.y)))
}

pub(crate) fn shape(handle: usize) -> Result<Shape> {
    let cached = {
        let sampler = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
        sampler
            .as_ref()
            .and_then(|s| s.last.as_ref())
            .filter(|last| last.cursor_serial as usize == handle)
            .map(convert)
    };
    match cached {
        Some(shape) => shape,
        None => sample(convert)?,
    }
}

fn convert(reply: &GetCursorImageAndNameReply) -> Result<Shape> {
    let (width, height) = (u32::from(reply.width), u32::from(reply.height));
    ensure!(width > 0 && height > 0, "光标不可见");
    ensure!(
        reply.cursor_image.len() >= (width * height) as usize,
        "光标图像不完整"
    );
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for &argb in &reply.cursor_image[..(width * height) as usize] {
        // XFixes hands out premultiplied ARGB; PNG wants straight alpha.
        let alpha = argb >> 24;
        let straight = |shift: u32| {
            let channel = (argb >> shift) & 0xff;
            if alpha == 0 {
                0
            } else {
                ((channel * 255 + alpha / 2) / alpha).min(255) as u8
            }
        };
        rgba.extend_from_slice(&[straight(16), straight(8), straight(0), alpha as u8]);
    }
    Ok(Shape {
        width,
        height,
        hotspot: [u32::from(reply.xhot), u32::from(reply.yhot)],
        rgba,
        kind: system_kind(&String::from_utf8_lossy(&reply.name)),
    })
}

/// The Windows system cursor a themed X cursor stands for, by its name in the
/// cursor theme (both the X11 core and the CSS names are in use), so the
/// controller can show its own native pointer for common shapes.
fn system_kind(name: &str) -> i32 {
    match name {
        "left_ptr" | "default" | "arrow" | "top_left_arrow" => 32512,
        "xterm" | "text" | "ibeam" => 32513,
        "watch" | "wait" => 32514,
        "crosshair" | "cross" | "tcross" => 32515,
        "sb_up_arrow" | "up-arrow" => 32516,
        "top_left_corner"
        | "bottom_right_corner"
        | "nwse-resize"
        | "nw-resize"
        | "se-resize"
        | "size_fdiag" => 32642,
        "top_right_corner" | "bottom_left_corner" | "nesw-resize" | "ne-resize" | "sw-resize"
        | "size_bdiag" => 32643,
        "sb_h_double_arrow" | "ew-resize" | "col-resize" | "left_side" | "right_side"
        | "e-resize" | "w-resize" | "size_hor" => 32644,
        "sb_v_double_arrow" | "ns-resize" | "row-resize" | "top_side" | "bottom_side"
        | "n-resize" | "s-resize" | "size_ver" => 32645,
        "fleur" | "move" | "all-scroll" | "size_all" => 32646,
        "not-allowed" | "crossed_circle" | "no-drop" | "forbidden" => 32648,
        "hand1" | "hand2" | "pointer" | "pointing_hand" => 32649,
        "left_ptr_watch" | "progress" => 32650,
        "question_arrow" | "help" | "whats_this" => 32651,
        _ => 0,
    }
}
