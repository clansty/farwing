mod actions;
mod headless;
// Fixed negotiated video-track pool; captures are attached/detached independently.
use super::{Published, media};
use crate::features::host::{
    Lease, VideoConfig, capture, format::Negotiated, lock, track::VideoTrack, transport::Transport,
};
use crate::platform::display::topology::{Target, Topology};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use webrtc::{
    peer_connection::RTCPeerConnection, rtp_transceiver::rtp_sender::RTCRtpSender,
    track::track_local::TrackLocal,
};

pub(crate) const TRACK_COUNT: usize = 5;

/// 没被主控看着、也没有被显式要过的那几路，用低帧持续发送。
///
/// 官方 macOS 主控把一块屏拖成独立窗口时不会告诉被控端，切页签也只换本地渲染
/// 轨道（见 `prefill` 的注释）；靠「切了再拉起来」会黑一下，靠「全都满帧」又要
/// 多花一份编码。低帧常发是那条中间线：编码量和码率都按帧率比例掉。
const AWAY_FPS: u32 = 10;

pub(super) struct Inventory {
    targets: Vec<Target>,
    screens: Vec<capture::Screen>,
    // Restoration bookkeeping needs a complete, identified layout. A transient
    // failure there must not discard an otherwise valid live screen inventory.
    layout: Result<crate::platform::display::topology::SavedTopology>,
}
impl Inventory {
    pub fn read() -> Result<Self> {
        let topology = Topology::query(false)?;
        let targets = topology.targets()?;
        // Linux：零输出是合法状态，兜底屏要处理的正是它。枚举不到屏时按空清单
        // 继续；连 X11 都连不上时上面的 query 已经报过错了。
        #[cfg(windows)]
        let screens = capture::screens()?;
        #[cfg(not(windows))]
        let screens = match capture::screens() {
            Ok(screens) => screens,
            Err(_) if targets.iter().all(|t| !t.active) => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            targets,
            screens,
            layout: Topology::query(true).and_then(|topology| topology.snapshot()),
        })
    }
    pub async fn collect(cancel: &CancellationToken) -> Result<Option<Self>> {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let mut task = tokio::task::spawn_blocking(Self::read);
        tokio::select! {
            biased;
            _=cancel.cancelled()=>{let _=task.await;Ok(None)},
            result=&mut task=>result.context("显示器库存读取任务中断").and_then(|r|r).map(Some),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenInfo {
    pub screen: capture::Screen,
    pub initial: capture::Screen,
    pub target: Option<Target>,
    pub kind: i32,
    pub resolution_type: i32,
}
pub(super) struct Reports {
    pub media_ready: Arc<AtomicBool>,
    pub capabilities: Mutex<Option<Vec<crate::features::host::format::Capability>>>,
    pub catalog: Mutex<Vec<ScreenInfo>>,
    pub current: AtomicI32,
    pub sequence: Arc<std::sync::atomic::AtomicI64>,
    pub publications: Vec<Mutex<Option<watch::Receiver<Published>>>>,
    pub pointers: Vec<Arc<Mutex<Option<crate::platform::cursor_shape::Snapshot>>>>,
    pub changed: watch::Sender<u64>,
}
impl Reports {
    pub fn notify(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn media(&self) -> Vec<(usize, Published)> {
        self.publications
            .iter()
            .enumerate()
            .filter_map(|(index, rx)| lock(rx).as_ref().map(|rx| (index, rx.borrow().clone())))
            .collect()
    }
}

pub(crate) struct Slot {
    pub screen: Option<capture::Screen>,
    pub suspended: Option<capture::Screen>,
    pub config: Arc<Mutex<VideoConfig>>,
    pub negotiated: Arc<Negotiated>,
    pub transport: Transport,
    pub sender: Arc<RTCRtpSender>,
    track: Arc<VideoTrack>,
    clock: Arc<media::Timeline>,
    worker: Option<media::Worker>,
    observer: Option<tokio::task::JoinHandle<()>>,
    lease: Option<Lease>,
    awaiting_source: bool,
    /// 主控显式要过这一路（SimpleAction 8）。它不受「鼠标在哪块屏」分档影响：
    /// 多窗口观看时每个窗口都要满帧。
    pinned: bool,
}
pub(crate) struct Screens {
    inventory_revision: u64,
    layout_error: Option<String>,
    pub displays: Arc<crate::features::host::displays::Session>,
    pub slots: Vec<Slot>,
    pub registered: Vec<usize>,
    pub(super) reports: Arc<Reports>,
    connection: std::sync::Weak<RTCPeerConnection>,
    lease: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    base: VideoConfig,
    originals: BTreeMap<i32, capture::Screen>,
    source_generation: u16,
    initial: Option<capture::Screen>,
    deferred: Option<Deferred>,
    before_super: Option<actions::Running>,
    headless: headless::State,
    /// Shells opened by the controller, shared with the frame handler on the
    /// BINARY channel. One table serves every connection so a reconnect keeps
    /// the running shells.
    terminal: Arc<Mutex<crate::features::host::terminal::Sessions>>,
    /// 主控是官方 macOS 客户端。它切屏完全在本地换视频轨道、一个请求都不发
    /// （逆向与实测都如此），所以要把每块屏都挂到轨道上等它切——官方被控端
    /// 也是这么做的。Windows / iOS / Android 和本项目的客户端都会自己发采集
    /// 启停请求，替它们预铺只是白占编码线程。
    prefill_tracks: bool,
}
pub(crate) struct Deferred {
    pub options: crate::features::stream_control::publisher::ConnectOptions,
    pub remote: crate::protocol::capability::DeviceCapability,
    pub encoding: crate::features::host::EncodingSettings,
}
impl Screens {
    pub(crate) fn update_media_intent(&self) -> VideoConfig {
        self.base
    }
    pub(crate) async fn resume_capture(&mut self) -> Result<()> {
        if let Some(id) = self.prepare_video().await? {
            return self.start(id).await;
        }
        self.refresh()?;
        let ids: Vec<_> = self
            .slots
            .iter()
            .filter_map(|s| s.screen.as_ref().or(s.suspended.as_ref()))
            .filter(|s| {
                self.info(s.id)
                    .is_ok_and(|v| v.screen.identity == s.identity)
            })
            .map(|s| s.id)
            .collect();
        if !ids.is_empty() {
            for id in ids {
                self.start(id).await?;
            }
            return Ok(());
        }
        let current = {
            let catalog = lock(&self.reports.catalog);
            catalog
                .iter()
                .find(|s| s.screen.primary)
                .or(catalog.first())
                .map(|s| s.screen.id)
        };
        if let Some(id) = current {
            return self.start(id).await;
        }
        let index = *self.registered.first().context("没有可用的视频轨道")?;
        self.slots[index].suspended = self.initial.clone();
        self.slots[index].awaiting_source = true;
        self.maintain_headless().await?;
        ensure!(
            self.slots.iter().any(|s| s.screen.is_some()),
            "没有可用显示器，画面尚未恢复"
        );
        Ok(())
    }
    pub(crate) async fn prepare_video(&mut self) -> Result<Option<i32>> {
        let Some(deferred) = self.deferred.as_ref() else {
            return Ok(None);
        };
        let preparation = async {
            let screen = self.displays.prepare(deferred.options.clone()).await?;
            let media = self
                .lease
                .prepare_media(screen.clone(), deferred.encoding.gpu_compatibility)
                .await?;
            let capabilities = deferred.encoding.select(&media.codecs)?;
            let prepared = crate::features::host::desktop::Prepared::new(
                &deferred.options,
                screen,
                &capabilities,
                &deferred.remote,
                deferred.encoding,
            )?;
            Ok::<_, anyhow::Error>((prepared, capabilities))
        }
        .await;
        let (prepared, capabilities) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Err(restore) = self.displays.rollback_preparation().await {
                    tracing::error!(%restore,"failed video preparation layout recovery");
                }
                return Err(error);
            }
        };
        self.base = prepared.config;
        self.initial = Some(prepared.screen.clone());
        for slot in &mut self.slots {
            let negotiated = Arc::new(prepared.negotiated.for_track());
            slot.track.configure(negotiated.clone());
            slot.negotiated = negotiated;
            *lock(&slot.config) = prepared.config;
        }
        *lock(&self.reports.capabilities) = Some(capabilities);
        self.reports
            .current
            .store(prepared.screen.id, Ordering::Release);
        self.deferred = None;
        self.refresh()?;
        self.reports.notify();
        Ok(Some(prepared.screen.id))
    }
    pub(crate) fn set_fps_limit(&mut self, limit: u32) {
        self.base.fps_limit = limit;
        self.base.fps = self
            .base
            .requested_fps
            .min(limit)
            .min(self.base.maximum_fps);
        for slot in &self.slots {
            let mut config = lock(&slot.config);
            config.fps_limit = limit;
            config.fps = config.requested_fps.min(limit).min(config.maximum_fps);
            config.revision = config.revision.wrapping_add(1);
        }
        self.reports.notify();
    }
    pub(crate) fn set_media_default(&mut self, config: VideoConfig) {
        self.base = config;
    }
    pub(crate) fn handshake_source(&self) -> Option<capture::Screen> {
        self.initial.clone()
    }
    pub(crate) fn authorization(&self) -> Lease {
        self.lease.clone()
    }
    /// The controller's shells, for the terminal RPCs and frame handler.
    pub(crate) fn terminal(&self) -> Arc<Mutex<crate::features::host::terminal::Sessions>> {
        Arc::clone(&self.terminal)
    }
    pub(super) async fn new(
        connection: Arc<RTCPeerConnection>,
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
        initial: VideoConfig,
        negotiated: Arc<Negotiated>,
        network: Transport,
        selected: Option<capture::Screen>,
        displays: Arc<crate::features::host::displays::Session>,
        deferred: Option<Deferred>,
        prefill_tracks: bool,
    ) -> Result<Self> {
        let reports = Arc::new(Reports {
            media_ready: Arc::new(AtomicBool::new(false)),
            capabilities: Mutex::new(None),
            catalog: Mutex::default(),
            current: AtomicI32::new(selected.as_ref().map_or(-1, |s| s.id)),
            sequence: Arc::new(std::sync::atomic::AtomicI64::new(1)),
            publications: (0..TRACK_COUNT).map(|_| Mutex::new(None)).collect(),
            pointers: (0..TRACK_COUNT).map(|_| Arc::default()).collect(),
            changed: watch::channel(0).0,
        });
        let mut slots = Vec::new();
        for index in 0..TRACK_COUNT {
            let negotiated = Arc::new(negotiated.for_track());
            let transport = if index == 0 {
                network.clone()
            } else {
                network.fork(index)
            };
            let track = Arc::new(VideoTrack::new(
                index,
                initial.format.codec,
                negotiated.clone(),
            ));
            let sender = connection
                .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
                .await?;
            let parameters = sender.get_parameters().await;
            for encoding in parameters.encodings {
                transport.register_ssrc(encoding.ssrc);
            }
            slots.push(Slot {
                screen: None,
                suspended: None,
                config: Arc::new(Mutex::new(initial)),
                negotiated,
                transport,
                sender,
                track,
                clock: media::Timeline::new(),
                worker: None,
                observer: None,
                lease: None,
                awaiting_source: false,
                pinned: false,
            });
        }
        let mut state = Self {
            inventory_revision: 0,
            layout_error: None,
            displays,
            slots,
            registered: vec![0],
            reports,
            connection: Arc::downgrade(&connection),
            lease,
            cancel,
            connected,
            base: initial,
            originals: BTreeMap::new(),
            source_generation: 0,
            initial: selected.clone(),
            deferred,
            before_super: None,
            headless: Default::default(),
            terminal: crate::features::host::terminal::shared(),
            prefill_tracks,
        };
        state.refresh()?;
        if let Some(selected) = selected {
            state.start_at(0, selected, initial).await?;
        }
        Ok(state)
    }
    pub(crate) fn refresh(&mut self) -> Result<()> {
        self.apply_inventory(Inventory::read()?)
    }
    pub(super) fn inventory_revision(&self) -> u64 {
        self.inventory_revision
    }
    fn apply_inventory(&mut self, inventory: Inventory) -> Result<()> {
        let Inventory {
            targets,
            screens,
            layout,
        } = inventory;
        match layout {
            Ok(layout) => {
                self.displays.observe_layout(&layout)?;
                if self.layout_error.take().is_some() {
                    tracing::info!("host restoration layout observation recovered");
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.layout_error.as_ref() != Some(&message) {
                    tracing::warn!(error = %message,
                        "host restoration layout unavailable; preserving ownership and continuing live display recovery");
                    self.layout_error = Some(message);
                }
                // Do not substitute an empty or partial layout: that would
                // wrongly relinquish restoration ownership for existing targets.
            }
        }
        let mut catalog = Vec::new();
        for mut screen in screens {
            let mut target = targets
                .iter()
                .find(|target| {
                    screen
                        .identity
                        .as_ref()
                        .map_or(target.source == screen.device_name, |identity| {
                            *identity == target.identity
                        })
                })
                .cloned();
            let (kind, resolution_type) = self.displays.annotate(&mut screen, &mut target)?;
            let initial = self
                .originals
                .entry(screen.id)
                .or_insert_with(|| self.displays.initial_screen(&screen))
                .clone();
            catalog.push(ScreenInfo {
                screen,
                initial,
                target,
                kind,
                resolution_type,
            });
        }
        self.inventory_revision = self.inventory_revision.wrapping_add(1);
        let mut old = lock(&self.reports.catalog);
        if *old != catalog {
            *old = catalog;
            drop(old);
            self.reports.notify();
        }
        Ok(())
    }
    pub(crate) fn info(&self, id: i32) -> Result<ScreenInfo> {
        lock(&self.reports.catalog)
            .iter()
            .find(|s| s.screen.id == id)
            .cloned()
            .context("显示器已不可用")
    }
    pub(crate) fn active_slot(&self, id: i32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.screen.as_ref().is_some_and(|s| s.id == id))
    }
    pub(crate) fn available_slot(&self) -> Option<usize> {
        self.registered
            .iter()
            .copied()
            .find(|&index| self.slots[index].screen.is_none())
    }
    pub(crate) async fn register(&mut self, indices: &[i32]) -> Result<()> {
        ensure!(
            indices.len() <= TRACK_COUNT
                && indices.iter().all(|i| (0..TRACK_COUNT as i32).contains(i)),
            "无效视频轨道池"
        );
        let mut registered: Vec<_> = indices.iter().map(|i| *i as usize).collect();
        registered.sort_unstable();
        registered.dedup();
        self.registered = registered;
        for (index, slot) in self.slots.iter().enumerate() {
            if !self.registered.contains(&index) {
                lock(&slot.config).sending = false;
                slot.transport.pause();
            }
        }
        self.reports.notify();
        // 主控登记轨道池等于说「这些轨道我都会看」。官方主控切屏完全在本地换渲染
        // 轨道、一个请求都不发，所以每块屏都得先挂在某一路轨道上、并在 `Screens`
        // 里报出真实轨道号，否则它切过去就是黑屏。
        if self.prefill_tracks {
            self.prefill().await;
        }
        Ok(())
    }
    /// 把每块屏铺到一路已登记的轨道上，最后把「主控正在看的那块」放进唯一的
    /// 满帧档（`tune` 会把其余几路压到 `AWAY_FPS`）。
    ///
    /// 铺完每块屏都是 `sending = true`：主控切过去的那一刻它已经在收画面，不会
    /// 先黑一下再等输入事件把采集拉起来。
    async fn prefill(&mut self) {
        let current = self.reports.current.load(Ordering::Acquire);
        let screens: Vec<i32> = lock(&self.reports.catalog)
            .iter()
            .map(|info| info.screen.id)
            .collect();
        for id in screens {
            if self.available_slot().is_none() {
                break;
            }
            if self.active_slot(id).is_some() {
                continue;
            }
            if let Err(error) = self.start(id).await {
                tracing::warn!(screen_id = id, %error, "host could not put screen on a track");
            }
        }
        if let Err(error) = self.watch(current) {
            tracing::warn!(screen_id = current, %error, "host could not keep the watched track");
        }
    }
    /// 主控现在看的是 `id`：这一路满帧，其余在轨道上的路退回低帧继续发。
    ///
    /// 不把别的路关掉，是因为主控可以把屏拖成独立窗口，也可能在动鼠标之前就要
    /// 看到新的一块（macOS 切页签不发任何请求，只有输入帧里有信号）。
    pub(crate) fn watch(&self, id: i32) -> Result<()> {
        let target = self
            .active_slot(id)
            .with_context(|| format!("显示器 {id} 尚未挂到轨道上"))?;
        self.tune(Some(target));
        self.reports.current.store(id, Ordering::Release);
        self.reports.notify();
        Ok(())
    }
    /// 主控有没有显式要过某一路画面（`SimpleAction 8`）。要过就说明它自己管理
    /// 窗口，被控端不该再按输入去猜它在看哪块屏。
    pub(crate) fn pinned_any(&self) -> bool {
        self.slots.iter().any(|slot| slot.pinned)
    }
    /// 主控显式要了这一路（`SimpleAction 8`）：它得满帧，且不再随鼠标来回切档。
    pub(crate) fn pin(&mut self, id: i32) {
        if let Some(index) = self.active_slot(id) {
            self.slots[index].pinned = true;
            self.tune(Some(index));
        }
    }
    /// `full` 那一路用会话的满帧，其余在采的路用 `AWAY_FPS`。
    fn tune(&self, full: Option<usize>) {
        for (index, slot) in self.slots.iter().enumerate() {
            let limit = if Some(index) == full || slot.pinned {
                self.base.fps_limit
            } else {
                AWAY_FPS
            };
            let mut config = lock(&slot.config);
            let sending = self.registered.contains(&index) && slot.screen.is_some();
            if config.fps_limit != limit || config.sending != sending {
                config.fps_limit = limit;
                config.fps = config.requested_fps.min(limit).min(config.maximum_fps);
                config.sending = sending;
                config.revision = config.revision.wrapping_add(1);
            }
        }
    }
    pub(crate) async fn start(&mut self, id: i32) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        let id = self.prepare_video().await?.unwrap_or(id);
        self.refresh()?;
        let screen = self.info(id)?.screen;
        self.headless.choose(id);
        if let Some(index) = self.active_slot(id) {
            if self.slots[index]
                .worker
                .as_ref()
                .is_some_and(|w| !w.ended())
            {
                let mut config = lock(&self.slots[index].config);
                config.capturing = true;
                config.sending = self.registered.contains(&index);
                drop(config);
                self.reports.current.store(id, Ordering::Release);
                self.reports.notify();
                return Ok(());
            }
            let config = *lock(&self.slots[index].config);
            return self.start_at(index, screen, config).await;
        }
        let index = self
            .slots
            .iter()
            .enumerate()
            .find(|(index, s)| {
                self.registered.contains(index) && s.suspended.as_ref().is_some_and(|s| s.id == id)
            })
            .map(|(index, _)| index)
            .or_else(|| self.available_slot())
            .or_else(|| (self.registered.len() == 1).then(|| self.registered[0]))
            .context("没有空闲的已登记视频轨道")?;
        let config = if self.slots[index]
            .suspended
            .as_ref()
            .is_some_and(|s| s.id == id)
        {
            *lock(&self.slots[index].config)
        } else {
            self.base
        };
        self.start_at(index, screen, config).await
    }
    pub(crate) async fn start_configured(
        &mut self,
        screen: capture::Screen,
        config: VideoConfig,
    ) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        ensure!(
            self.registered.len() == 1 && self.deferred.is_none(),
            "指定屏幕切换需要已准备的单视频轨道"
        );
        let index = self.registered[0];
        ensure!(self.slots[index].screen.is_some(), "当前视频采集已停止");
        self.headless.choose(screen.id);
        self.start_at(index, screen, config).await
    }
    /// 把当前唯一在采的那块屏换成另一块：还是同一路轨道、同一套编码参数。
    ///
    /// 主控单窗口切屏时只换输入里的屏幕标识（macOS 实测），画面得由被控端搬过去；
    /// 不能走 `start`——它优先挑空闲轨道，会变成多采一路，主控看的那一路并没有换。
    pub(crate) async fn switch_source(&mut self, id: i32) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        let mut capturing = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.screen.is_some());
        let (Some((index, _)), None) = (capturing.next(), capturing.next()) else {
            anyhow::bail!("只有一路画面时才能换源");
        };
        self.refresh()?;
        let screen = self.info(id)?.screen;
        let config = *lock(&self.slots[index].config);
        self.headless.choose(screen.id);
        self.start_at(index, screen, config).await
    }
    async fn start_at(
        &mut self,
        index: usize,
        screen: capture::Screen,
        mut config: VideoConfig,
    ) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        let requested_format = config.format;
        self.slots[index]
            .negotiated
            .source_gpu(screen.render_adapter.unwrap_or(screen.adapter));
        let requested_hdr = config.color.is_hdr();
        self.slots[index].negotiated.apply(
            &mut config,
            None,
            requested_format.chroma,
            requested_hdr,
            (screen.width, screen.height),
        )?;
        self.stop_at(index).await;
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        let slot = &mut self.slots[index];
        slot.suspended = None;
        slot.awaiting_source = false;
        config.capturing = true;
        config.sending = self.registered.contains(&index);
        *lock(&slot.config) = config;
        self.source_generation = self.source_generation.wrapping_add(1);
        let (tx, mut rx) = watch::channel(Published {
            screen: screen.clone(),
            // The slot already owns this source. Native preparation is not a
            // stopped stream: publishing track=-1/CaptureChange=99 here races
            // the controller's initial setting restoration.
            capturing: true,
            visible: false,
            quality: 0,
            fps: config.fps,
            format: config.format,
            budget_size: (screen.width, screen.height),
            encoder: None,
            capture: String::new(),
        });
        let lease = self.lease.begin_stream(index)?;
        let cancel = self.cancel.child_token();
        let worker = media::Worker::spawn(
            index,
            self.source_generation,
            slot.clock.clone(),
            self.connection.upgrade().context("媒体连接已关闭")?,
            slot.track.clone(),
            slot.sender.clone(),
            screen.clone(),
            lease.clone(),
            cancel.clone(),
            self.connected.clone(),
            self.reports.media_ready.clone(),
            slot.config.clone(),
            slot.negotiated.clone(),
            slot.transport.clone(),
            tx,
            self.reports.pointers[index].clone(),
        )?;
        *lock(&self.reports.publications[index]) = Some(rx.clone());
        let reports = self.reports.clone();
        slot.observer = Some(tokio::spawn(async move {
            loop {
                tokio::select! { _=cancel.cancelled()=>break, result=rx.changed()=>{ if result.is_err() { break; } } }
                reports.notify();
            }
            reports.notify();
        }));
        slot.lease = Some(lease);
        slot.worker = Some(worker);
        slot.screen = Some(screen.clone());
        self.reports.current.store(screen.id, Ordering::Release);
        self.reports.notify();
        Ok(())
    }
    async fn stop_at(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        if let Some(lease) = slot.lease.take() {
            lease.clear_stream();
        }
        if let Some(worker) = slot.worker.take() {
            worker.close().await;
        }
        if let Some(observer) = slot.observer.take() {
            let _ = observer.await;
        }
        slot.transport.pause();
        slot.screen = None;
        slot.pinned = false;
        *lock(&self.reports.publications[index]) = None;
        *lock(&self.reports.pointers[index]) = None;
        self.reports.notify();
    }
    pub(crate) async fn stop(&mut self, screen: i32) -> Result<()> {
        ensure!(self.lease.requested(), "被控许可已失效");
        self.headless.stop(screen);
        self.stop_media(screen).await
    }
    async fn stop_media(&mut self, screen: i32) -> Result<()> {
        if screen == -1 {
            for slot in &mut self.slots {
                slot.awaiting_source = false;
            }
            for index in 0..self.slots.len() {
                if self.slots[index].screen.is_some() {
                    self.slots[index].suspended = self.slots[index].screen.clone();
                }
                self.stop_at(index).await;
            }
        } else if let Some(index) = self.active_slot(screen) {
            self.slots[index].awaiting_source = false;
            self.slots[index].suspended = self.slots[index].screen.clone();
            self.stop_at(index).await;
        } else {
            let mut known = self.headless.knows_screen(screen);
            for slot in &mut self.slots {
                if slot.suspended.as_ref().is_some_and(|s| s.id == screen) {
                    known = true;
                    slot.awaiting_source = false;
                }
            }
            if !known {
                self.info(screen)?;
            }
        }
        Ok(())
    }
    pub(super) async fn maintain(&mut self, revision: u64, inventory: Inventory) -> Result<()> {
        // A user-requested layout operation may have completed during the read.
        // Its newer catalog wins over the late periodic result.
        if revision != self.inventory_revision {
            return Ok(());
        }
        self.apply_inventory(inventory)?;
        let selected = self.reports.current.load(Ordering::Acquire);
        let mut fatal = false;
        for index in 0..self.slots.len() {
            let disappeared = self.slots[index].screen.as_ref().is_some_and(|screen| {
                !lock(&self.reports.catalog).iter().any(|info| {
                    info.screen.id == screen.id
                        && (screen.identity.is_none() || info.screen.identity == screen.identity)
                })
            });
            if disappeared {
                self.slots[index].awaiting_source = true;
                self.slots[index].suspended = self.slots[index].screen.clone();
                self.stop_at(index).await;
            }
            if self.slots[index]
                .worker
                .as_ref()
                .is_some_and(|worker| worker.ended())
            {
                if self.slots[index]
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.source_missing())
                {
                    self.slots[index].awaiting_source = true;
                    self.slots[index].suspended = self.slots[index].screen.clone();
                } else {
                    fatal |= !self.headless.owns_slot(index);
                }
                self.stop_at(index).await;
            }
            if self.slots[index].awaiting_source && !self.headless.owns_slot(index) {
                if let Some(old) = self.slots[index].suspended.clone() {
                    if let Ok(info) = self.info(old.id) {
                        // A returning target must have the same monitor identity; never
                        // attach an unrelated monitor which inherited DISPLAYn.
                        if info.screen.identity == old.identity && old.identity.is_some() {
                            let config = *lock(&self.slots[index].config);
                            self.start_at(index, info.screen, config).await?;
                        }
                    }
                }
            }
        }
        if self.active_slot(selected).is_some() {
            self.reports.current.store(selected, Ordering::Release);
        }
        if let Err(error) = self.maintain_headless().await {
            tracing::warn!(error=%format!("{error:#}"), "headless display recovery failed");
            self.lease.fail(format!("显示器恢复失败：{error:#}"));
        }
        if fatal
            && self.headless.recovery.is_none()
            && self
                .slots
                .iter()
                .all(|s| s.worker.is_none() && !s.awaiting_source)
        {
            self.cancel.cancel();
        }
        Ok(())
    }
    pub(super) async fn close(&mut self) {
        self.headless.close().await;
        for index in 0..self.slots.len() {
            self.stop_at(index).await;
        }
        if let Err(error) = self.displays.close().await {
            self.lease
                .recovery_failed(format!("显示恢复未完成：{error:#}"));
            tracing::error!(%error,"display cleanup failed");
        }
    }
}
impl Drop for Screens {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Some(lease) = slot.lease.take() {
                lease.clear_stream();
            }
            slot.worker.take();
            if let Some(observer) = slot.observer.take() {
                observer.abort();
            }
            slot.transport.pause();
        }
    }
}
