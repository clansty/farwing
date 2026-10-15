//! Screen capture for the host role, with backends tried in order of
//! preference the way Sunshine picks its capture method.
//!
//! - NVIDIA NvFBC into CUDA (`nvfbc`): Xorg on NVIDIA, only while NVENC works,
//!   so frames go from the display to the encoder without leaving the GPU.
//! - X11 MIT-SHM (`x11`): Xorg sessions; no setup, no consent prompt.
//! - wlroots screencopy (`wlr`): Wayland sessions on wlroots, niri, Sway and
//!   their relatives; frames come through the compositor itself, and nobody is
//!   asked for consent.
//! - XDG ScreenCast portal + PipeWire (`portal`): Wayland sessions, and any
//!   desktop whose portal offers it; asks the local user once.
//!
//! Sunshine puts KMS first; on this driver stack it finds no scanout buffer
//! under Xorg (the NVIDIA X driver bypasses KMS), so it is not in the list
//! yet. `OPENUUYC_CAPTURE` (for example `portal` or `nvfbc,x11`) overrides the
//! order.
//!
//! Frames are BGRA, in system memory or, from NvFBC, in CUDA device memory.
pub(crate) mod dmabuf;
mod nvfbc;
mod pipewire;
mod portal;
pub(crate) mod wlr_screencopy;
mod x11;

use anyhow::{Result, bail, ensure};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) use crate::media::capture::{Screen, SourceGone};

/// An X11 capture has no GPU device. The one value stands for the CPU path
/// that capture and the software encoder share, so the loop's "same device,
/// no transfer" test always holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device;

/// Nothing on the CPU path can be removed from under the capture.
pub(crate) fn device_lost(_device: &Device) -> bool {
    false
}

/// Linux captures on the CPU, so there is no adapter for a second device to be
/// opened on. The host only asks for one when a hardware encoder sits on a
/// different adapter than the capture, which cannot happen here.
pub(crate) fn create_device(_adapter: u64) -> Result<(Device, ())> {
    Ok((Device, ()))
}

/// GPU adapters a hardware encoder could be pinned to. None are listed: NVENC
/// runs on the GPU driving the display and VA-API on the driver's device, so
/// there is nothing to choose between and the preference stays automatic.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EncodingAdapter {
    #[serde(default)]
    pub id: Option<crate::media::selection::GpuId>,
    pub luid: u64,
    pub vendor: u32,
    pub name: String,
}

pub(crate) fn encoding_adapters() -> Result<Vec<EncodingAdapter>> {
    Ok(Vec::new())
}

/// Whether the desktop session is locked. X11 has no portable answer: a
/// screen locker is just another client drawing over the root window, and the
/// capture sees exactly what it draws.
pub(crate) fn session_locked() -> Option<bool> {
    None
}

/// A captured picture's BGRA pixels, wherever the capture left them.
#[derive(Clone)]
pub(crate) enum Image {
    /// Tightly packed, `width * height * 4` bytes.
    Cpu {
        pixels: Arc<Vec<u8>>,
        width: u32,
        height: u32,
    },
    /// A pitched CUDA allocation on the GPU driving the display.
    Cuda(Arc<super::cuda::Buffer>),
    /// 合成器直接画进去的显存缓冲：内容不在系统内存里，编码器自己导入 GPU。
    /// `width`/`height` 是这块屏「看到」的尺寸，缓冲自己的排布（旋转过的屏上是
    /// 另一个方向）由 `transform` 描述。
    DmaBuf {
        buffer: Arc<dmabuf::Buffer>,
        width: u32,
        height: u32,
        transform: wayland_client::protocol::wl_output::Transform,
    },
}
impl Image {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Cpu { width, height, .. } => (*width, *height),
            Self::Cuda(buffer) => (buffer.width, buffer.height),
            Self::DmaBuf { width, height, .. } => (*width, *height),
        }
    }
    /// The pixels in system memory, read back from the GPU if need be.
    ///
    /// 显存帧的读回很慢（一帧几十毫秒），它只是给「零拷贝那条路走不通了」兜底
    /// 用的后备，正常路径下编码器直接吃显存。
    pub fn pixels(&self) -> Result<std::borrow::Cow<'_, [u8]>> {
        Ok(match self {
            Self::Cpu { pixels, .. } => std::borrow::Cow::Borrowed(pixels.as_slice()),
            Self::Cuda(buffer) => std::borrow::Cow::Owned(buffer.download()?),
            Self::DmaBuf {
                buffer, transform, ..
            } => {
                let raw = buffer.read_back()?;
                let row = buffer.width as usize * 4;
                let mut tight = Vec::with_capacity(row * buffer.height as usize);
                for line in raw
                    .chunks_exact(buffer.stride as usize)
                    .take(buffer.height as usize)
                {
                    tight.extend_from_slice(&line[..row]);
                }
                let (pixels, _, _) =
                    wlr_screencopy::orient(tight, buffer.width, buffer.height, *transform);
                std::borrow::Cow::Owned(pixels)
            }
        })
    }
    /// 这一帧是不是合成器画的显存缓冲。
    pub fn dmabuf(&self) -> Option<&Arc<dmabuf::Buffer>> {
        match self {
            Self::DmaBuf { buffer, .. } => Some(buffer),
            _ => None,
        }
    }
}

/// 输出自己的旋转里能交给 GPU 还原的那几种。`None` 表示这种旋转得回 CPU 做
/// （180° 和带翻转的变换没有现成的 VA-API 滤镜），这条路就干脆别开。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rotation {
    None,
    Clockwise,
    CounterClockwise,
}

pub(crate) fn rotation_of(
    transform: wayland_client::protocol::wl_output::Transform,
) -> Option<Rotation> {
    use wayland_client::protocol::wl_output::Transform;
    match transform {
        Transform::Normal => Some(Rotation::None),
        Transform::_90 => Some(Rotation::Clockwise),
        Transform::_270 => Some(Rotation::CounterClockwise),
        _ => None,
    }
}

/// A backend's picture before it is scaled: tightly packed BGRA rows.
pub(super) struct Picture {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// One captured picture, already scaled to the size the encoder was asked for.
#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub image: Image,
    pub captured: Instant,
    pub is_new: bool,
    pub hdr_metadata: Option<crate::media::video_color::HdrMetadata>,
}
impl Frame {
    /// 这一帧的像素是不是留在显存里。编码器照着它决定用哪条路——它是帧自己的
    /// 属性，所以走「上一帧原样再发一次」那条路时也不会说错。
    pub fn hardware_image(&self) -> bool {
        matches!(self.image, Image::DmaBuf { .. })
    }

    /// 能不能留一份在手上当「上一帧」重发。门户那种从合成器池子借来的显存帧不行
    /// ——攥着不放会把浅池子抽干、逼门户整条流重来；wlr 的显存缓冲是我们自己分配
    /// 的，留一份没关系，静态屏上还得靠它发保活帧。
    pub fn cacheable(&self) -> bool {
        !matches!(&self.image, Image::DmaBuf { buffer, .. } if buffer.imported())
    }
}

/// The active RandR outputs, in the protocol's terms. The identity is the
/// one the display topology uses, so screens and targets pair up exactly.
pub(crate) fn screens() -> Result<Vec<Screen>> {
    // Monitors come from RandR, which XWayland also provides on Wayland.
    ensure!(
        std::env::var_os("DISPLAY").is_some(),
        "没有 X11 显示（DISPLAY 未设置），无法枚举显示器"
    );
    let monitors = super::display::topology::Topology::query(true)?.monitors();
    ensure!(!monitors.is_empty(), "没有检测到显示器");
    monitors
        .into_iter()
        .map(|monitor| {
            Ok(Screen {
                id: super::display::source_id(&monitor.identity)?,
                device_name: monitor.name.clone(),
                display_name: monitor.name,
                width: monitor.width,
                height: monitor.height,
                left: monitor.left,
                top: monitor.top,
                primary: monitor.primary,
                fps: monitor.hz.filter(|hz| *hz > 1).unwrap_or(60),
                // X11 has no per-output scale to report.
                dpi_scale: None,
                hdr: false,
                adapter: 0,
                render_adapter: None,
                identity: Some(monitor.identity),
            })
        })
        .collect()
}

/// Whether this is a Wayland session, where X11 sees only XWayland clients.
pub(crate) fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland")
}

/// The same physical output as `selected`, as it is now.
pub(crate) fn refresh(selected: &Screen) -> Result<Screen> {
    let mut found = screens()?
        .into_iter()
        .find(|screen| screen.identity == selected.identity)
        .ok_or(SourceGone)?;
    found.id = selected.id;
    Ok(found)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    NvFbc,
    X11,
    Wlr,
    Portal,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::NvFbc => "NvFBC",
            Self::X11 => "X11",
            Self::Wlr => "wlr-screencopy",
            Self::Portal => "XDG 门户",
        }
    }
}

/// The capture methods to try, best first.
fn candidates() -> Result<Vec<Kind>> {
    if let Ok(order) = std::env::var("OPENUUYC_CAPTURE") {
        let kinds = order
            .split(',')
            .map(|name| match name.trim() {
                "nvfbc" => Ok(Kind::NvFbc),
                "x11" => Ok(Kind::X11),
                "wlr" => Ok(Kind::Wlr),
                "portal" => Ok(Kind::Portal),
                other => bail!("OPENUUYC_CAPTURE 中有未知的采集方式：{other}"),
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(!kinds.is_empty(), "OPENUUYC_CAPTURE 为空");
        return Ok(kinds);
    }
    Ok(if wayland_session() {
        // The compositor's own protocol first: it is the only one that copies
        // without asking, and the portal stays as the fallback for compositors
        // that do not serve it.
        vec![Kind::Wlr, Kind::Portal]
    } else {
        vec![Kind::NvFbc, Kind::X11, Kind::Portal]
    })
}

/// 这份候选表里真正开得起来的那些，顺序不变。合成器不提供协议时把 `Wlr` 摘掉，
/// 挡在它后面的东西（Mutter 和 KWin 上就是门户）才能被看见。
fn usable(kinds: &[Kind], wlr_available: bool) -> Vec<Kind> {
    kinds
        .iter()
        .copied()
        .filter(|kind| *kind != Kind::Wlr || wlr_available)
        .collect()
}

/// 现在开采集会不会要求本机用户共享屏幕：门户排在最前、又没有记住的授权。
/// 开不起来的 `Wlr` 会把门户顶到第一位，所以这道判断必须问真正会用上的那份
/// 候选表，而不是 `candidates()` 的原样。
pub(crate) fn needs_consent() -> bool {
    candidates().is_ok_and(|kinds| {
        // 只有 `Wlr` 打头时它才可能把门户挡在后面，而探测要连一次合成器，
        // 其它顺序就省掉这次连接。
        let wlr = kinds.first() == Some(&Kind::Wlr) && wlr_screencopy::available();
        consent_needed(&usable(&kinds, wlr), portal::remembered())
    })
}

fn consent_needed(kinds: &[Kind], remembered: bool) -> bool {
    kinds.first() == Some(&Kind::Portal) && !remembered
}

enum Backend {
    NvFbc(nvfbc::Grab),
    X11(x11::Grab),
    Wlr(wlr_screencopy::Grab),
    Portal(portal::Grab),
}
impl Backend {
    /// `size` is the frame size for backends that scale while capturing.
    fn open(kind: Kind, screen: &Screen, cursor: bool, size: (u32, u32)) -> Result<Self> {
        Ok(match kind {
            Kind::NvFbc => Self::NvFbc(nvfbc::Grab::open(screen, cursor, size)?),
            Kind::X11 => Self::X11(x11::Grab::open(screen)?),
            Kind::Wlr => Self::Wlr(wlr_screencopy::Grab::open(screen, cursor)?),
            Kind::Portal => Self::Portal(portal::Grab::open(screen, cursor)?),
        })
    }
    fn kind(&self) -> Kind {
        match self {
            Self::NvFbc(_) => Kind::NvFbc,
            Self::X11(_) => Kind::X11,
            Self::Wlr(_) => Kind::Wlr,
            Self::Portal(_) => Kind::Portal,
        }
    }
    /// Open the first capture method that works for `screen`.
    fn select(screen: &Screen, cursor: bool, size: (u32, u32)) -> Result<Self> {
        let mut failures = Vec::new();
        for kind in candidates()? {
            match Self::open(kind, screen, cursor, size) {
                Ok(backend) => {
                    tracing::info!(
                        backend = backend.name(),
                        screen = screen.id,
                        "host capture backend selected"
                    );
                    return Ok(backend);
                }
                Err(error) => {
                    tracing::info!(backend = kind.label(), error = %format!("{error:#}"), "host capture backend unavailable");
                    failures.push(format!("{}：{error:#}", kind.label()));
                }
            }
        }
        bail!("没有可用的屏幕采集方式（{}）", failures.join("；"))
    }
    fn name(&self) -> &'static str {
        match self {
            Self::NvFbc(grab) => grab.name(),
            Self::X11(grab) => grab.name(),
            Self::Wlr(grab) => grab.name(),
            Self::Portal(grab) => grab.name(),
        }
    }
}

/// A capture of one monitor, kept open between frames.
pub(crate) struct Desktop {
    pub device: Device,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    /// The pointer as of the latest `next`, for the cursor channel.
    pub cursor: Option<super::cursor_shape::Snapshot>,
    sampler: super::cursor_shape::Sampler,
    backend: Backend,
    /// The last system-memory picture, which X11 compares against.
    last: Option<Arc<Vec<u8>>>,
    last_size: (u32, u32),
    refreshed: Instant,
    /// 调用方想让画面留在显存里。后端换了（换屏、换光标设置）要按它再切回来：
    /// 编码器是照着显存帧建起来的，采集这边不能悄悄换回系统内存。
    hardware: bool,
    /// 采集期间点亮屏幕、挡住自动息屏与空闲休眠。放最后，让它比采集资源活得久，
    /// 与 Windows 侧一致。
    _power: Option<super::power_request::Request>,
}

impl Desktop {
    /// 打开一路采集。门户那边要线性 dma-buf 还是共享内存由合成器挑（见
    /// `pipewire::format_object`），拿到显存帧也只在上层要求时才当显存用。
    pub fn open_selected(selected: &Screen) -> Result<Self> {
        // 桌面不提供抑制接口时不拦住采集：会话照常，只是屏幕可能按空闲策略熄灭。
        let power = super::power_request::Request::capture()
            .inspect_err(|error| {
                tracing::warn!(error = %format!("{error:#}"), "熄灭屏幕的唤醒与息屏抑制不可用")
            })
            .ok();
        let screen = refresh(selected)?;
        let size = (screen.width & !1, screen.height & !1);
        let backend = Backend::select(&screen, false, size)?;
        Ok(Self {
            device: Device,
            screen,
            generation: 0,
            available: true,
            cursor: None,
            sampler: Default::default(),
            backend,
            last: None,
            last_size: (0, 0),
            refreshed: Instant::now(),
            hardware: false,
            _power: power,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Neither backend captures HDR.
    pub fn hdr_available(&self) -> bool {
        false
    }

    /// 零拷贝：让采集把画面留在显存里，缩放与色彩转换都交给 GPU。
    ///
    /// 返回是否真的切了过去；切不过去（不是 wlr-screencopy、合成器没有
    /// `zwp_linux_dmabuf_v1`、这块屏的旋转 GPU 还原不了，或是门户没谈成
    /// dma-buf）调用方照旧走系统内存。切换之后 `next` 给的是 [`Image::DmaBuf`]，
    /// 尺寸是这块屏看到的尺寸。
    pub fn hardware_images(&mut self, enabled: bool) -> bool {
        self.hardware = enabled;
        self.apply_hardware()
    }

    /// 采集现在是不是真的把画面留在显存里。切不过去、或者后端重建之后没切回来，
    /// 都会是 `false`——调用方得照这个来决定编码器怎么建。
    pub fn hardware_active(&self) -> bool {
        match &self.backend {
            Backend::Wlr(grab) => grab.hardware(),
            Backend::Portal(grab) => grab.hardware(),
            _ => false,
        }
    }

    fn apply_hardware(&mut self) -> bool {
        let active = self.hardware
            && match &mut self.backend {
                Backend::Wlr(grab) => grab.set_hardware(true),
                // 门户的协商连上就定了，切不了；它只是把「现在是不是显存」报回来。
                Backend::Portal(grab) => grab.set_hardware(true),
                _ => false,
            };
        if !active {
            self.hardware = false;
        }
        active
    }

    /// The next picture, or `None` when nothing changed before `timeout` ms.
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        _hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        if self.refreshed.elapsed() >= Duration::from_secs(1) {
            let current = refresh(&self.screen)?;
            self.refreshed = Instant::now();
            self.screen.display_name.clone_from(&current.display_name);
            if current != self.screen {
                let size = output(current.width, current.height, quality, maximum);
                let backend = Backend::open(self.backend.kind(), &current, cursor, size)
                    .or_else(|_| Backend::select(&current, cursor, size))?;
                self.backend = backend;
                self.screen = current;
                self.generation = self.generation.wrapping_add(1);
                self.last = None;
                self.apply_hardware();
            }
        }
        // The portal and wlr-screencopy draw the cursor or leave it out for the
        // whole session; a change of request moves to a session with the other
        // mode.
        if let Backend::Portal(grab) = &self.backend
            && grab.cursor() != cursor
        {
            self.backend =
                Backend::Portal(portal::Grab::open(&self.screen, cursor)?);
            self.last = None;
            // 新 Grab 的硬件开关回到默认（关），按桌面记着的意愿补回来。
            self.apply_hardware();
        }
        if let Backend::Wlr(grab) = &self.backend
            && grab.cursor() != cursor
        {
            self.backend = Backend::Wlr(wlr_screencopy::Grab::open(&self.screen, cursor)?);
            self.last = None;
            self.apply_hardware();
        }
        // NvFBC scales and draws the cursor as it captures; both are fixed
        // per session, so a different request opens a new one.
        let wanted = output(self.screen.width, self.screen.height, quality, maximum);
        if let Backend::NvFbc(grab) = &self.backend
            && !grab.matches(cursor, wanted)
        {
            self.backend = Backend::NvFbc(nvfbc::Grab::open(&self.screen, cursor, wanted)?);
        }
        // 采样失败只是这一次读不到，不是「光标不见了」：当成隐藏会让主控的光标一闪
        // 一闪，而 smart mouse 会照着每一次「又出现了」去挪它的本地光标。上一次的
        // 样子留着，下一次采样（几十毫秒后）会补上。
        match self.sampler.sample() {
            Ok(pointer) => self.cursor = Some(pointer),
            Err(error) => tracing::debug!(%error, "host cursor sampling failed"),
        }
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout));
        // 零拷贝那条路：不缩放、也不在 CPU 上解旋转，画面原样留在显存里，
        // 剩下的都在 GPU 上做（见 `ffmpeg::drm`）。
        // 两道都要：`self.hardware` 是上层记着的意愿（探针这类从没开过的就为假），
        // `grab.hardware()` 是后端当下真谈成了、且没被关掉。缺了前者，没开零拷贝
        // 的地方也会拿到显存帧；缺了后者，谈崩了还照走。
        if self.hardware
            && match &self.backend {
                Backend::Wlr(grab) => grab.hardware(),
                Backend::Portal(grab) => grab.hardware(),
                _ => false,
            }
        {
            let waiting = deadline.saturating_duration_since(Instant::now());
            let result = match &mut self.backend {
                Backend::Wlr(grab) => grab.next_hardware(waiting).map(|ready| ready.map(|done| done.buffer)),
                Backend::Portal(grab) => grab.next_hardware(waiting),
                _ => unreachable!(),
            };
            // 门户给的画面已经是摆正后的（Monitor 采集按显示器所见方向交付），
            // 所以它的旋转恒为 `Normal`；wlr 那边 `transform` 说的是缓冲自己的
            // 排布，是另一回事，照旧报它自己那个。
            let (transform, displayed) = match &self.backend {
                Backend::Wlr(grab) => (grab.transform(), grab.displayed_size()),
                Backend::Portal(grab) => (
                    wayland_client::protocol::wl_output::Transform::Normal,
                    grab.displayed_size(),
                ),
                _ => unreachable!(),
            };
            return match result {
                Ok(Some(hardware)) => {
                    self.available = true;
                    Ok(Some(Frame {
                        width: displayed.0,
                        height: displayed.1,
                        image: Image::DmaBuf {
                            buffer: hardware,
                            width: displayed.0,
                            height: displayed.1,
                            transform,
                        },
                        captured: Instant::now(),
                        is_new: true,
                        hdr_metadata: None,
                    }))
                }
                Ok(None) => {
                    self.available = true;
                    Ok(None)
                }
                Err(error) => {
                    self.available = false;
                    Err(error)
                }
            };
        }
        let result = match &mut self.backend {
            // The driver waits for a new frame and delivers it on the GPU.
            Backend::NvFbc(grab) => {
                return match grab.next(timeout) {
                    Ok(frame) => {
                        self.available = true;
                        Ok(frame.map(|buffer| Frame {
                            width: buffer.width,
                            height: buffer.height,
                            image: Image::Cuda(buffer),
                            captured: Instant::now(),
                            is_new: true,
                            hdr_metadata: None,
                        }))
                    }
                    Err(error) => {
                        self.available = false;
                        Err(error)
                    }
                };
            }
            Backend::X11(grab) => loop {
                if let Err(error) = grab.grab(&self.screen, cursor) {
                    break Err(error);
                }
                let size = output(self.screen.width, self.screen.height, quality, maximum);
                let image = scale(&grab.source, (self.screen.width, self.screen.height), size);
                // X11 cannot say whether anything changed; compare.
                if self.last_size != size
                    || self
                        .last
                        .as_deref()
                        .is_none_or(|last| last.as_slice() != image.as_slice())
                {
                    break Ok(Some((size, image)));
                }
                let now = Instant::now();
                if now >= deadline {
                    break Ok(None);
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(8)));
            },
            // The compositor answers a copy request only when it next draws,
            // so a still desktop reports no picture.
            Backend::Wlr(grab) => grab
                .next(deadline.saturating_duration_since(Instant::now()))
                .map(|picture| {
                    picture.map(|picture| {
                        let size = output(picture.width, picture.height, quality, maximum);
                        // 尺寸已经对上了就别再缩放一次：同尺寸的 scale() 也会
                        // 老老实实分配并整帧拷一遍（2880x1800 约 20MiB），而这
                        // 张已是 owned、紧密打包的 BGRA，直接交出去就行。
                        let image = if size == (picture.width, picture.height) {
                            picture.pixels
                        } else {
                            scale(&picture.pixels, (picture.width, picture.height), size)
                        };
                        (size, image)
                    })
                }),
            // PipeWire only delivers a picture when the monitor changed.
            Backend::Portal(grab) => grab
                .next(deadline.saturating_duration_since(Instant::now()))
                .map(|picture| {
                    picture.map(|picture| {
                        let size = output(picture.width, picture.height, quality, maximum);
                        // 同上：PipeWire 给的已经是 owned 且紧密打包的 BGRA，
                        // 原生尺寸下不必再分配、再拷一遍。
                        let image = if size == (picture.width, picture.height) {
                            picture.pixels
                        } else {
                            scale(&picture.pixels, (picture.width, picture.height), size)
                        };
                        (size, image)
                    })
                }),
        };
        match result {
            Ok(Some((size, image))) => {
                self.available = true;
                let image = Arc::new(image);
                self.last = Some(image.clone());
                self.last_size = size;
                Ok(Some(Frame {
                    width: size.0,
                    height: size.1,
                    image: Image::Cpu {
                        pixels: image,
                        width: size.0,
                        height: size.1,
                    },
                    captured: Instant::now(),
                    is_new: true,
                    hdr_metadata: None,
                }))
            }
            Ok(None) => {
                self.available = true;
                Ok(None)
            }
            Err(error) => {
                self.available = false;
                Err(error)
            }
        }
    }
}


/// The encoded size for a source of `width`x`height` at `quality`.
fn output(width: u32, height: u32, quality: i32, maximum: (u32, u32)) -> (u32, u32) {
    let size = crate::media::geometry::output_size(width, height, quality);
    crate::media::geometry::fit_size(size.0, size.1, maximum)
}

/// One output column: the two source pixels it reads and how much of the right
/// one it takes. The mapping is the same for every row, so it is worked out
/// once per frame rather than once per pixel.
#[derive(Clone, Copy)]
struct Column {
    /// First source pixel, as an index into the row's channels.
    left: usize,
    right: usize,
    /// 14-bit weight of the right pixel; the left one takes the rest.
    weight: u32,
}

/// Bilinear BGRA scaling, or a crop to even dimensions when the size already
/// fits. The encoder takes exactly the size it was opened with.
///
/// Each output row is built in two passes instead of one per pixel: the source
/// rows are blended vertically into a 16-bit row (one weight for the whole row,
/// read and written in order, so it stays in the cache), and that row is then
/// blended horizontally. A single pass over the source would read four pixels
/// per output pixel, each from a different place — on a 5K screen that is 44 MB
/// read at random while the encoder waits, and it was the longest stage of the
/// capture. Weights are fourteen-bit fixed point in both passes, which is
/// finer than the eight-bit result can show.
fn scale(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (source_width, source_height) = (from.0 as usize, from.1 as usize);
    let (width, height) = (to.0 as usize, to.1 as usize);
    let mut out = vec![0u8; width * height * 4];
    if width <= source_width
        && height <= source_height
        && (source_width - width) <= 1
        && (source_height - height) <= 1
    {
        for y in 0..height {
            let start = y * source_width * 4;
            out[y * width * 4..(y + 1) * width * 4]
                .copy_from_slice(&source[start..start + width * 4]);
        }
        return out;
    }
    let step_x = ((source_width as u64) << 16) / width as u64;
    let step_y = ((source_height as u64) << 16) / height as u64;
    let mut columns = Vec::with_capacity(width);
    for x in 0..width {
        // 16.16 fixed point, sampling pixel centres.
        let sx = ((x as u64 * step_x) + (step_x >> 1)).saturating_sub(1 << 15);
        let left = ((sx >> 16) as usize).min(source_width - 1);
        columns.push(Column {
            left: left * 4,
            right: (left + 1).min(source_width - 1) * 4,
            weight: ((sx & 0xffff) as u32) >> 2,
        });
    }
    let mut row = vec![0u16; source_width * 4];
    for y in 0..height {
        let sy = ((y as u64 * step_y) + (step_y >> 1)).saturating_sub(1 << 15);
        let top = ((sy >> 16) as usize).min(source_height - 1);
        let bottom = (top + 1).min(source_height - 1);
        blend_rows(
            &source[top * source_width * 4..(top + 1) * source_width * 4],
            &source[bottom * source_width * 4..(bottom + 1) * source_width * 4],
            ((sy & 0xffff) as u32) >> 2,
            &mut row,
        );
        blend_columns(&row, &columns, &mut out[y * width * 4..(y + 1) * width * 4]);
    }
    out
}

/// One channel of the vertical blend, seven fractional bits in the result: the
/// horizontal pass expects the fraction to still be there.
fn blend_pair(top: u8, bottom: u8, weight: u32) -> u16 {
    (u32::from(top) * (16384 - weight) + u32::from(bottom) * weight + 64).wrapping_shr(7) as u16
}

/// Two source rows into one, four channels each. The weight of the bottom row
/// is 0..=16384, and `blend_pair` keeps the fraction of what it does not move.
fn blend_rows(top: &[u8], bottom: &[u8], weight: u32, row: &mut [u16]) {
    let weight = weight.min(16384);
    #[cfg(target_arch = "x86_64")]
    {
        blend_rows_simd(top, bottom, weight, row);
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    for (index, slot) in row.iter_mut().enumerate() {
        *slot = blend_pair(top[index], bottom[index], weight);
    }
}

/// The same eight lanes at a time: two pixels of the row per iteration.
///
/// The blend is written as the top row plus the weighted difference to the
/// bottom one, because every weight can then be seven bits wide: the driver
/// multiplies pairs of sixteen-bit lanes, whose high half is the product of the
/// two seven-bit halves. A weight kept as one sixteen-bit number would need its
/// own multiplication and a wider lane.
#[cfg(target_arch = "x86_64")]
fn blend_rows_simd(top: &[u8], bottom: &[u8], weight: u32, row: &mut [u16]) {
    use std::arch::x86_64::*;
    let high = (weight >> 7) as i16;
    let low = (weight & 0x7f) as i16;
    // Safe because the three slices are the same length, which the loops keep
    // within, and both sources are four-channel rows.
    unsafe {
        let zero = _mm_setzero_si128();
        let high = _mm_set1_epi16(high);
        let low = _mm_set1_epi16(low);
        let rounding = _mm_set1_epi16(64);
        let mut index = 0;
        while index + 8 <= row.len() {
            let top_bytes =
                _mm_unpacklo_epi8(_mm_loadl_epi64(top.as_ptr().add(index).cast()), zero);
            let bottom_bytes =
                _mm_unpacklo_epi8(_mm_loadl_epi64(bottom.as_ptr().add(index).cast()), zero);
            // The difference of two eight-bit samples is at most a signed
            // sixteen-bit number, and each half of the weight keeps the product
            // inside one, so nothing here wraps.
            let difference = _mm_sub_epi16(bottom_bytes, top_bytes);
            let value = _mm_add_epi16(
                _mm_slli_epi16(top_bytes, 7),
                _mm_mullo_epi16(difference, high),
            );
            let correction =
                _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(difference, low), rounding), 7);
            _mm_storeu_si128(
                row.as_mut_ptr().add(index).cast(),
                _mm_add_epi16(value, correction),
            );
            index += 8;
        }
        for index in index..row.len() {
            row[index] = blend_pair(top[index], bottom[index], weight);
        }
    }
}

/// One row of the sixteen-bit intermediate into one output row.
fn blend_columns(row: &[u16], columns: &[Column], out: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        // Safe because every column names two pixels of a row of this length,
        // which is where the table came from.
        unsafe { blend_columns_simd(row, columns, out) };
        return;
    }
    blend_columns_one_by_one(row, columns, out);
}

fn blend_columns_one_by_one(row: &[u16], columns: &[Column], out: &mut [u8]) {
    for (x, column) in columns.iter().enumerate() {
        let inverse = 16384 - column.weight;
        for channel in 0..4 {
            let a = u32::from(row[column.left + channel]) * inverse;
            let b = u32::from(row[column.right + channel]) * column.weight;
            out[x * 4 + channel] = ((a + b + (1 << 20)) >> 21) as u8;
        }
    }
}

/// The same, two columns at a time. The intermediate carries seven fractional
/// bits, so fourteen bits of weight and a shift of twenty-one give the eight
/// bits that are stored.
#[cfg(target_arch = "x86_64")]
unsafe fn blend_columns_simd(row: &[u16], columns: &[Column], out: &mut [u8]) {
    use std::arch::x86_64::*;
    // Safe because every column names two pixels of a row of this length, which
    // is where the table came from.
    unsafe {
        // `madd` adds neighbouring pairs of sixteen-bit lanes, so a column is
        // unpacked into (left channel, right channel) pairs and takes the
        // weights in that order.
        let weights = |weight: u32| {
            _mm_set1_epi32((((weight as i32) << 16) | (16384 - weight) as i32) as i32)
        };
        let mut x = 0;
        while x + 2 <= columns.len() {
            let (first, second) = (columns[x], columns[x + 1]);
            let a = _mm_loadl_epi64(row.as_ptr().add(first.left).cast());
            let b = _mm_loadl_epi64(row.as_ptr().add(first.right).cast());
            let c = _mm_loadl_epi64(row.as_ptr().add(second.left).cast());
            let d = _mm_loadl_epi64(row.as_ptr().add(second.right).cast());
            let first = _mm_madd_epi16(_mm_unpacklo_epi16(a, b), weights(first.weight));
            let second = _mm_madd_epi16(_mm_unpacklo_epi16(c, d), weights(second.weight));
            let rounding = _mm_set1_epi32(1 << 20);
            let both = _mm_packs_epi32(
                _mm_srai_epi32(_mm_add_epi32(first, rounding), 21),
                _mm_srai_epi32(_mm_add_epi32(second, rounding), 21),
            );
            _mm_storel_epi64(
                out.as_mut_ptr().add(x * 4).cast(),
                _mm_packus_epi16(both, both),
            );
            x += 2;
        }
        blend_columns_one_by_one(row, &columns[x..], &mut out[x * 4..]);
    }
}

#[cfg(test)]
pub(crate) fn scale_for_probe(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    scale(source, from, to)
}

#[cfg(test)]
mod tests {
    use super::{Kind, consent_needed, scale, usable};

    #[test]
    fn only_an_unremembered_portal_asks_the_user() {
        assert!(consent_needed(&[Kind::Portal], false));
        assert!(!consent_needed(&[Kind::Portal], true));
        // X11 会话要等 NvFBC 与 MIT-SHM 都失败才会落到门户。
        assert!(!consent_needed(
            &[Kind::NvFbc, Kind::X11, Kind::Portal],
            false
        ));
    }

    #[test]
    fn a_wlr_that_cannot_open_leaves_the_portal_in_front() {
        // Mutter 与 KWin 都不提供 `zwlr_screencopy_manager_v1`：登录时的探测
        // 绝不能落到底下的门户去弹共享授权窗，这正是 needs_consent() 要拦的事。
        assert!(consent_needed(
            &usable(&[Kind::Wlr, Kind::Portal], false),
            false
        ));
        assert!(!consent_needed(
            &usable(&[Kind::Wlr, Kind::Portal], true),
            false
        ));
        // 已经记住的授权怎么样都不弹。
        assert!(!consent_needed(
            &usable(&[Kind::Wlr, Kind::Portal], false),
            true
        ));
        // X11 的顺序里没有 wlr 可摘。
        assert!(!consent_needed(
            &usable(&[Kind::NvFbc, Kind::X11, Kind::Portal], false),
            false
        ));
    }

    #[test]
    fn crop_to_even_keeps_pixels() {
        let source: Vec<u8> = (0..3 * 3 * 4).map(|v| v as u8).collect();
        let out = scale(&source, (3, 3), (2, 2));
        assert_eq!(&out[..8], &source[..8]);
        assert_eq!(&out[8..16], &source[12..20]);
    }

    /// 与一个独立的 f64 双线性实现对照：采样位置、朝向、通道都必须一致，
    /// 定点权重的误差也不能超过一个色阶。
    #[test]
    fn matches_an_independent_bilinear() {
        for (from, to) in [
            ((64u32, 48u32), (32u32, 24u32)),
            ((37, 23), (16, 9)),
            ((33, 17), (12, 8)),
            ((16, 16), (24, 24)),
            ((5, 3), (10, 7)),
        ] {
            let source: Vec<u8> = (0..(from.0 as usize * from.1 as usize * 4))
                .map(|v| (v % 251) as u8)
                .collect();
            let ours = scale(&source, from, to);
            let reference = bilinear_reference(&source, from, to);
            let worst = ours
                .iter()
                .zip(&reference)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap();
            assert!(
                worst <= 1,
                "{from:?} -> {to:?} 与参考实现相差 {worst} 个色阶"
            );
        }
    }

    /// 逐像素的 f64 双线性，采样点与 `scale` 相同。
    fn bilinear_reference(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
        let (sw, sh) = (from.0 as usize, from.1 as usize);
        let (w, h) = (to.0 as usize, to.1 as usize);
        let step_x = ((sw as u64) << 16) / w as u64;
        let step_y = ((sh as u64) << 16) / h as u64;
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            let sy = ((y as u64 * step_y) + (step_y >> 1)).saturating_sub(1 << 15);
            let y0 = ((sy >> 16) as usize).min(sh - 1);
            let y1 = (y0 + 1).min(sh - 1);
            let fy = (sy & 0xffff) as f64 / 65536.0;
            for x in 0..w {
                let sx = ((x as u64 * step_x) + (step_x >> 1)).saturating_sub(1 << 15);
                let x0 = ((sx >> 16) as usize).min(sw - 1);
                let x1 = (x0 + 1).min(sw - 1);
                let fx = (sx & 0xffff) as f64 / 65536.0;
                for channel in 0..4 {
                    let at = |px: usize, py: usize| f64::from(source[(py * sw + px) * 4 + channel]);
                    let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
                    let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
                    out[(y * w + x) * 4 + channel] = (top * (1.0 - fy) + bottom * fy).round() as u8;
                }
            }
        }
        out
    }

    /// `cargo test --release --lib -- --ignored profiles_the_scalers --nocapture`
    #[test]
    #[ignore]
    fn profiles_the_scalers() {
        for (from, to) in [
            ((5120u32, 2160u32), (3840u32, 1620u32)),
            ((1600, 2560), (1350, 2160)),
            ((2560, 1600), (1350, 2160)),
        ] {
            let source: Vec<u8> = (0..(from.0 as usize * from.1 as usize * 4))
                .map(|v| v as u8)
                .collect();
            let rounds = 10;
            let at = std::time::Instant::now();
            let mut out = Vec::new();
            for _ in 0..rounds {
                out = scale(&source, from, to);
            }
            let elapsed = at.elapsed() / rounds;
            std::hint::black_box(&out);
            println!("{from:?} -> {to:?}: {elapsed:?}");
        }
    }

    #[test]
    fn downscale_of_flat_colour_is_flat() {
        let source = [10u8, 20, 30, 255].repeat(64 * 48);
        let out = scale(&source, (64, 48), (32, 24));
        assert!(out.chunks_exact(4).all(|p| p == [10, 20, 30, 255]));
    }
}

#[cfg(test)]
mod stream_tests {
    /// 真机采一路下来交给 VA-API 编码，逐帧留下「编码器看到的亮度」与码流：
    /// 用 ffmpeg 解回来逐帧对，就能看出参考帧链在长时间/大变化下会不会漂。
    /// `OPENUUYC_PROBE_OUTPUT=DP-1 cargo test --release --lib -- --ignored reports_the_real_stream --nocapture`
    #[test]
    #[ignore]
    fn reports_the_real_stream() {
        use crate::media::encoding::{Codec, Format, QualityTarget, Rate};
        let screens = super::screens().unwrap();
        let wanted = std::env::var("OPENUUYC_PROBE_OUTPUT").unwrap_or_default();
        let screen = screens
            .iter()
            .find(|screen| screen.device_name == wanted)
            .unwrap_or(&screens[0])
            .clone();
        let (quality, maximum) = (4, (3840, 2160));
        let mut desktop = super::Desktop::open_selected(&screen).unwrap();
        let format = Format {
            codec: Codec::H264,
            chroma: 1,
            depth: 8,
        };
        let rate = Rate {
            target: 24_000_000,
            peak: 24_000_000,
            fps: 30,
            quality,
            quality_target: QualityTarget {
                bitrate: 24_000_000,
                fps: 30,
            },
        };
        let mut encoder = None;
        let (mut stream, mut luma) = (Vec::new(), Vec::new());
        let started = std::time::Instant::now();
        let mut index = 0i64;
        let (mut frames, mut keyframes, mut bytes) = (0, 0, 0usize);
        let mut sizes = Vec::new();
        while started.elapsed() < std::time::Duration::from_secs(15) {
            let Ok(Some(frame)) = desktop.next(33, quality, false, false, maximum) else {
                continue;
            };
            let size = (frame.width, frame.height);
            let encoder = match encoder.as_mut() {
                Some(encoder) => encoder,
                None => {
                    println!(
                        "编码尺寸 {size:?}（源 {:?}）",
                        (screen.width, screen.height)
                    );
                    encoder = Some(
                        crate::platform::linux::vaapi::Encoder::new(size, format, rate).unwrap(),
                    );
                    encoder.as_mut().unwrap()
                }
            };
            let Ok(encoded) = encoder.encode(&frame.image, index * 333_333, index == 0) else {
                continue;
            };
            // 编码器看到的亮度，按 1/8 抽样留下，够看出漂移又不至于几个 G
            let pixels = frame.image.pixels().unwrap();
            let (width, height) = (size.0 as usize, size.1 as usize);
            let mut plane = vec![0u8; width * height * 3 / 2];
            let (y, uv) = plane.split_at_mut(width * height);
            let (u, v) = uv.split_at_mut(width * height / 4);
            yuv::bgra_to_yuv420(
                &mut yuv::YuvPlanarImageMut {
                    y_plane: yuv::BufferStoreMut::Borrowed(y),
                    y_stride: size.0,
                    u_plane: yuv::BufferStoreMut::Borrowed(u),
                    u_stride: size.0 / 2,
                    v_plane: yuv::BufferStoreMut::Borrowed(v),
                    v_stride: size.0 / 2,
                    width: size.0,
                    height: size.1,
                },
                &pixels,
                size.0 * 4,
                yuv::YuvRange::Limited,
                yuv::YuvStandardMatrix::Bt601,
                yuv::YuvConversionMode::Balanced,
            )
            .unwrap();
            for row in (0..height).step_by(8) {
                for column in (0..width).step_by(8) {
                    luma.push(y[row * width + column]);
                }
            }
            for output in &encoded {
                stream.extend_from_slice(&output.data);
                bytes += output.data.len();
                sizes.push((output.data.len(), output.keyframe));
                if output.keyframe {
                    keyframes += 1;
                }
            }
            frames += 1;
            index += 1;
        }
        println!("{frames} 帧，{keyframes} 个关键帧，{bytes} 字节");
        println!(
            "前 10 帧字节数 {:?}",
            sizes.iter().take(10).collect::<Vec<_>>()
        );
        println!(
            "后 10 帧字节数 {:?}",
            sizes.iter().rev().take(10).collect::<Vec<_>>()
        );
        std::fs::write("/tmp/openuuyc-real.h264", &stream).unwrap();
        std::fs::write("/tmp/openuuyc-real-luma.raw", &luma).unwrap();
    }
}

#[cfg(test)]
mod session_tests {
    /// Needs a desktop session: `OPENUUYC_CAPTURE=portal cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn captures_frames_for_a_few_seconds() {
        let screens = super::screens().unwrap();
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend: {}", desktop.backend_name());
        let started = std::time::Instant::now();
        let (mut frames, mut size) = (0, None);
        while started.elapsed() < std::time::Duration::from_secs(4) {
            if let Some(frame) = desktop.next(50, 4, false, false, (3840, 2160)).unwrap() {
                frames += 1;
                size = Some((frame.width, frame.height));
            }
        }
        println!("frames: {frames} size: {size:?}");
        assert!(frames > 0);
    }
}
