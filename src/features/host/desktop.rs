//! Desktop media preparation within an already authorized controlled connection.
use super::{VideoConfig, capture, encoder, format};
use crate::features::stream_control::publisher;
use crate::protocol::capability::DeviceCapability;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Capabilities {
    pub screen: capture::Screen,
    pub codecs: Vec<format::Capability>,
    pub adapters: Vec<capture::EncodingAdapter>,
}

#[derive(Default)]
pub(super) struct Cache(tokio::sync::Mutex<Slots>);

/// 一次探测的适用条件：同一块屏、同样的尺寸与 HDR 状态、同一组显卡，探出来的能力
/// 就一样。摆放位置、标签、协议 ID、DPI 与刷新率都不进这个键——它们不影响编码能力。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ProbeKey {
    identity: Option<String>,
    device_name: String,
    adapter: u64,
    render_adapter: Option<u64>,
    width: u32,
    height: u32,
    hdr: bool,
    /// 显卡列表变了（插拔显卡、换驱动）也得重探，所以把每张卡的标识也带上。
    adapters: Vec<(Option<crate::media::selection::GpuId>, u64, u32, String)>,
}

impl ProbeKey {
    fn new(screen: &capture::Screen, adapters: &[capture::EncodingAdapter]) -> Self {
        Self {
            identity: screen.identity.clone(),
            device_name: screen.device_name.clone(),
            adapter: screen.adapter,
            render_adapter: screen.render_adapter,
            width: screen.width,
            height: screen.height,
            hdr: screen.hdr,
            adapters: adapters
                .iter()
                .map(|adapter| (adapter.id, adapter.luid, adapter.vendor, adapter.name.clone()))
                .collect(),
        }
    }
}

/// 一块屏一条。以前只有一格，客户端在两块屏之间来回切就每次都得重探（后一次把前一次
/// 顶掉）；探测本身不碰 GPU 了，但抓第一帧之类的代价还在，没必要白花。
/// 条数按「屏 × HDR 状态」算，正常机器个位数；真超了整体清空，不值得为它写 LRU。
const MAX_ENTRIES: usize = 8;

#[derive(Default)]
struct Slots {
    entries: std::collections::HashMap<ProbeKey, Arc<Capabilities>>,
    /// 最近准备出来的那份：`snapshot()` 用它回答「现在能提供什么能力」。
    latest: Option<Arc<Capabilities>>,
}

impl Slots {
    fn remember(&mut self, key: ProbeKey, capabilities: Arc<Capabilities>) {
        if self.entries.len() >= MAX_ENTRIES && !self.entries.contains_key(&key) {
            self.entries.clear();
        }
        self.entries.insert(key, Arc::clone(&capabilities));
        self.latest = Some(capabilities);
    }
}

impl Cache {
    pub fn snapshot(&self) -> Option<Arc<Capabilities>> {
        self.0.try_lock().ok()?.latest.clone()
    }
    pub async fn prepare(
        &self,
        screen: capture::Screen,
        gpu_compatibility: bool,
        active: impl Fn() -> bool + Send + 'static,
    ) -> Result<Arc<Capabilities>> {
        let mut slots = self.0.lock().await;
        ensure!(active(), "媒体准备已取消");
        let adapters = tokio::task::spawn_blocking(capture::encoding_adapters).await??;
        ensure!(active(), "媒体准备已取消");
        let key = ProbeKey::new(&screen, &adapters);
        // 先把要复用的 codec 列表拷出来，免得抱着 entries 的借用去调 remember。
        let reused = slots.entries.get(&key).map(|cached| cached.codecs.clone());
        if let Some(codecs) = reused {
            let capabilities = Arc::new(Capabilities {
                screen,
                codecs,
                adapters,
            });
            slots.remember(key, Arc::clone(&capabilities));
            tracing::info!("host media capabilities reused");
            return Ok(capabilities);
        }
        let capabilities = Arc::new(probe(screen, adapters, gpu_compatibility, active).await?);
        slots.remember(key, Arc::clone(&capabilities));
        Ok(capabilities)
    }
    pub async fn replace(&self, value: Option<Capabilities>) {
        let mut slots = self.0.lock().await;
        match value {
            Some(capabilities) => {
                let key = ProbeKey::new(&capabilities.screen, &capabilities.adapters);
                slots.remember(key, Arc::new(capabilities));
            }
            None => *slots = Slots::default(),
        }
    }
    pub async fn invalidate(&self) {
        *self.0.lock().await = Slots::default();
    }
}

async fn probe(
    screen: capture::Screen,
    adapters: Vec<capture::EncodingAdapter>,
    gpu_compatibility: bool,
    active: impl Fn() -> bool + Send + 'static,
) -> Result<Capabilities> {
    crate::lifecycle::blocking(move |cancel| {
        let active = || active() && !cancel.is_cancelled();
        let started = std::time::Instant::now();
        ensure!(active(), "媒体准备已取消");
        let _runtime = encoder::Runtime::with_gpu_compatibility(gpu_compatibility)?;
        // 探针只解码一两帧看被控能力，传 false：绝不让它去协商、mmap 合成器的
        // dma-buf——不可映射的 dma-buf 会让整个能力检查失败，会话根本起不来。
        let mut desktop = capture::Desktop::open_selected(&screen)?;
        let codecs = encoder::probe(&mut desktop, &active)?;
        ensure!(active(), "媒体准备已取消");
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            screen_id = screen.id,
            capabilities = codecs.len(),
            "host media capabilities prepared"
        );
        Ok(Capabilities {
            screen,
            codecs,
            adapters,
        })
    })
    .await
    .context("被控能力检查任务中断")?
}

pub(crate) struct Prepared {
    pub screen: capture::Screen,
    pub negotiated: Arc<format::Negotiated>,
    pub config: VideoConfig,
}
impl Prepared {
    pub fn new(
        options: &publisher::ConnectOptions,
        screen: capture::Screen,
        capabilities: &[format::Capability],
        remote: &DeviceCapability,
        settings: super::EncodingSettings,
    ) -> Result<Self> {
        let negotiated = Arc::new(format::Negotiated::new(
            capabilities,
            remote,
            &options.decoders,
            settings,
            screen.render_adapter.unwrap_or(screen.adapter),
        )?);
        let mut config = publisher::config(options.params.as_ref());
        let chroma = options
            .params
            .as_ref()
            .map_or(1, |p| if p.chroma == 3 { 3 } else { 1 });
        let hdr = options.params.as_ref().is_some_and(|p| p.hdr);
        negotiated.apply(
            &mut config,
            None,
            chroma,
            hdr,
            (screen.width, screen.height),
        )?;
        tracing::info!(
            parameters = ?options.params,
            requested_fps = config.requested_fps,
            fps_limit = config.fps_limit,
            maximum_fps = config.maximum_fps,
            effective_fps = config.fps,
            "host initial capture settings negotiated"
        );
        Ok(Self {
            screen,
            negotiated,
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::encoding::{Backend, Capability, Codec, Format};

    fn screen(id: i32, width: u32, height: u32) -> capture::Screen {
        capture::Screen {
            id,
            device_name: "card1".into(),
            display_name: "DP".into(),
            width,
            height,
            left: 0,
            top: 0,
            primary: id == 0,
            fps: 60,
            dpi_scale: None,
            hdr: false,
            adapter: 0,
            render_adapter: None,
            identity: Some(format!("DP-{id}")),
        }
    }

    fn capabilities(screen: capture::Screen) -> Capabilities {
        Capabilities {
            screen,
            codecs: vec![Capability {
                adapter: 0,
                backend: Backend::Amd,
                format: Format {
                    codec: Codec::H264,
                    chroma: 1,
                    depth: 8,
                },
                maximum: (4096, 4096),
            }],
            adapters: Vec::new(),
        }
    }

    /// 两块屏各探一次之后，切回前一块要能直接复用。单槽时代这里会缓存未命中、
    /// 于是去开采集（测试环境里打不开桌面必然失败），所以拿到 `Ok` 就说明多槽生效。
    /// 摆放位置 / 刷新率变了不该重探；尺寸变了才该当成另一块屏。
    #[tokio::test]
    async fn keeps_a_probe_for_each_screen() {
        let cache = Cache::default();
        let first = screen(0, 2560, 1600);
        let second = screen(1, 5120, 2160);
        cache.replace(Some(capabilities(first.clone()))).await;
        cache.replace(Some(capabilities(second.clone()))).await;
        let reused = cache
            .prepare(first.clone(), false, || true)
            .await
            .expect("第一块屏的能力应该还在缓存里");
        assert_eq!(reused.screen.id, first.id);

        let mut moved = first.clone();
        moved.left = 1920;
        moved.fps = 144;
        assert_eq!(
            ProbeKey::new(&moved, &[]),
            ProbeKey::new(&first, &[]),
            "摆放位置与刷新率不该让缓存失效"
        );
        assert_ne!(
            ProbeKey::new(&screen(0, 1920, 1080), &[]),
            ProbeKey::new(&first, &[]),
            "尺寸变了就是另一块屏"
        );
    }

    /// 超过上限整体清空，别让它无限长。
    #[test]
    fn drops_everything_when_the_table_is_full() {
        let mut slots = Slots::default();
        for id in 0..MAX_ENTRIES as i32 {
            let screen = screen(id, 1920, 1080);
            slots.remember(ProbeKey::new(&screen, &[]), Arc::new(capabilities(screen)));
        }
        assert_eq!(slots.entries.len(), MAX_ENTRIES);
        let extra = screen(99, 1920, 1080);
        slots.remember(ProbeKey::new(&extra, &[]), Arc::new(capabilities(extra)));
        assert_eq!(slots.entries.len(), 1);
    }
}
