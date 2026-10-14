//! Capture through the compositor's own `zwlr_screencopy` protocol: wlroots,
//! niri, Sway, Hyprland and their relatives all serve it. Unlike the portal it
//! asks the local user for nothing, and it needs no permission the session does
//! not already have.
//!
//! 画面有两条路回来。默认走共享内存：合成器拷进来，这里再读出来交给编码器。
//! 另一条是让合成器直接画进我们分配的 dma-buf（`hardware` 打开时），像素全程
//! 留在显存里——这条路只有在编码器也直接在显存上干活时才划算，否则光是把它
//! 映射回来读一遍就比共享内存那条整条还慢（离散显卡上实测 195 ms 对 1.4 ms）。
//! 谁来开、怎么退回去见 `capture::Desktop::hardware_images`。
//!
//! The compositor copies its own framebuffer, not the picture the monitor
//! shows: a rotated or flipped output arrives sideways, and the transform
//! `wl_output.geometry` advertises has to be undone here, exactly as `grim`
//! and every other screencopy client does. Left undone, a portrait monitor
//! streams a landscape picture of a sideways desktop twice over — the shape
//! is wrong and so is everything in it.
//!
//! `copy_with_damage` is used rather than `copy`: the compositor then holds the
//! frame back until something has actually changed, which is what keeps an idle
//! desktop from being encoded and sent over and over.
//!
//! The copy carries no scaling: the buffer has to be exactly the output's size,
//! and the caller scales afterwards like it does for the portal.
use super::Picture;
use super::Screen;
use anyhow::{Context, Result, bail, ensure};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_output::Transform;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

/// `wl_output.name` arrived at 4, and it is how a captured monitor is paired
/// with the one the caller asked for.
const OUTPUT_VERSION: u32 = 4;

/// One output the compositor offers to copy.
struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    /// The mode the output is driving, before `transform` is applied.
    mode: (u32, u32),
    transform: Transform,
}

impl Output {
    /// The size the desktop sees: the mode as the transform turns it, which is
    /// what the caller's `Screen` describes and what a frame will come back as.
    fn size(&self) -> (u32, u32) {
        match self.transform {
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
                (self.mode.1, self.mode.0)
            }
            _ => self.mode,
        }
    }
}

/// The shared memory every frame is copied into, and the buffer that names it.
/// The pool and the descriptor stay alive as long as the buffer does: the
/// compositor reads the memory through them.
struct Shm {
    _fd: OwnedFd,
    _pool: wl_shm_pool::WlShmPool,
    map: *mut u8,
    length: usize,
    buffer: wl_buffer::WlBuffer,
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

impl Shm {
    fn matches(&self, format: u32, width: u32, height: u32, stride: u32) -> bool {
        self.format == format
            && self.width == width
            && self.height == height
            && self.stride == stride
    }

    /// The picture as tightly packed BGRA rows, without the stride padding and
    /// with the output's transform undone.
    fn picture(&self, transform: Transform) -> Picture {
        let row = self.width as usize * 4;
        // Safe because the mapping is live for as long as `self` is, and the
        // compositor has finished with it: a `ready` event preceded this.
        let all = unsafe { std::slice::from_raw_parts(self.map, self.length) };
        let mut pixels = Vec::with_capacity(row * self.height as usize);
        if self.stride as usize == row {
            pixels.extend_from_slice(&all[..row * self.height as usize]);
        } else {
            for line in all
                .chunks_exact(self.stride as usize)
                .take(self.height as usize)
            {
                pixels.extend_from_slice(&line[..row]);
            }
        }
        let (pixels, width, height) = orient(pixels, self.width, self.height, transform);
        Picture {
            width,
            height,
            pixels,
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // Safe because the mapping was made for this length in `Shm::new`.
        unsafe { libc::munmap(self.map.cast(), self.length) };
    }
}

pub(super) struct Grab {
    queue: EventQueue<State>,
    state: State,
    cursor: bool,
    output: usize,
    /// 零拷贝：让合成器直接画进我们分配的 dma-buf，而不是共享内存。
    hardware: bool,
    /// 那块缓冲。格式要等合成器报出来才知道，所以第一次用的时候才分配。
    target: Option<Hardware>,
}

/// 合成器画进去的显存缓冲，以及引用它的 `wl_buffer`。
///
/// 两块轮着用：合成器往一块里画的时候，编码器正在读另一块。只有一块的话，
/// 要么等编码器读完才敢让合成器动手（平白丢一帧的重叠），要么让两边抢同一块内存。
struct Hardware {
    buffers: [std::sync::Arc<super::dmabuf::Buffer>; 2],
    wl: [wl_buffer::WlBuffer; 2],
    /// 已经请求、还没兑现的那一块。
    pending: usize,
}

/// 一块画好的显存缓冲。
pub(super) struct Captured {
    pub buffer: std::sync::Arc<super::dmabuf::Buffer>,
}

#[derive(Default)]
struct State {
    connection: Option<Connection>,
    manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    shm: Option<wl_shm::WlShm>,
    /// 合成器愿意把画面画进显存时用的那个协议。
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    /// 它给这一帧开的格式与尺寸（`linux_dmabuf` 事件）。
    offer: Option<(u32, u32, u32)>,
    /// 探针自己接管这一次拷贝时置上，免得状态机先把帧用掉。
    manual: bool,
    /// 零拷贝：这一帧要画进去的那块显存缓冲。
    dmabuf_target: Option<wl_buffer::WlBuffer>,
    outputs: Vec<Output>,
    frame: Option<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1>,
    /// What the frame in flight says its buffer must look like.
    wanted: Option<(u32, u32, u32, u32)>,
    shm_buffer: Option<Shm>,
    /// The compositor has finished drawing the frame already requested.
    ready: bool,
    failed: bool,
    error: Option<String>,
}

impl Grab {
    pub fn open(screen: &Screen, cursor: bool) -> Result<Self> {
        let connection =
            Connection::connect_to_env().context("没有可用的 Wayland 会话（WAYLAND_DISPLAY）")?;
        let mut queue = connection.new_event_queue::<State>();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = State {
            connection: Some(connection),
            ..Default::default()
        };
        queue
            .roundtrip(&mut state)
            .context("读取 Wayland 全局对象失败")?;
        // The compositor answers a bind from its own event loop, so what the
        // outputs say about themselves arrives one round trip later.
        queue
            .roundtrip(&mut state)
            .context("读取 Wayland 输出描述失败")?;
        ensure!(
            state.manager.is_some(),
            "合成器没有提供 zwlr_screencopy_manager_v1"
        );
        ensure!(state.shm.is_some(), "合成器没有提供 wl_shm");
        ensure!(!state.outputs.is_empty(), "合成器没有提供任何输出");
        let output = pick(&state.outputs, screen)?;
        tracing::info!(
            output = screen.device_name,
            mode = ?state.outputs[output].mode,
            transform = ?state.outputs[output].transform,
            "host wlr-screencopy output selected"
        );
        Ok(Self {
            queue,
            state,
            cursor,
            output,
            hardware: false,
            target: None,
        })
    }

    pub fn name(&self) -> &'static str {
        "wlroots screencopy"
    }

    /// 零拷贝那条路现在是不是开着。
    pub fn hardware(&self) -> bool {
        self.hardware
    }

    /// 合成器认不认我们自己分配的 dma-buf。
    pub fn supports_hardware(&self) -> bool {
        self.state.dmabuf.is_some()
    }

    /// 输出自己的旋转，原样给出去（`orient` 是它的 CPU 版本）。
    pub fn transform(&self) -> Transform {
        self.state
            .outputs
            .get(self.output)
            .map_or(Transform::Normal, |output| output.transform)
    }

    /// 这块屏「看到」的尺寸，也就是解过旋转之后的画面尺寸。
    pub fn displayed_size(&self) -> (u32, u32) {
        self.state.outputs[self.output].size()
    }

    /// 能不能在 GPU 上把画面摆正。摆不正就不开这条路，别送出一张躺着的画面。
    pub fn hardware_rotation(&self) -> Option<super::Rotation> {
        super::rotation_of(self.transform())
    }

    /// 切开零拷贝。等到真的用不了（合成器没报格式、分配失败）再退回去。
    pub fn set_hardware(&mut self, enabled: bool) -> bool {
        if enabled && (!self.supports_hardware() || self.hardware_rotation().is_none()) {
            return false;
        }
        if !enabled {
            self.state.dmabuf_target = None;
            self.target = None;
        }
        self.hardware = enabled;
        // 返回的是「现在是不是显存模式」，关掉的时候就是 false。
        enabled
    }

    /// Whether the compositor draws the cursor into the pictures.
    pub fn cursor(&self) -> bool {
        self.cursor
    }

    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        self.state.dmabuf_target = None;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(error) = self.state.error.take() {
                bail!("{error}");
            }
            if std::mem::take(&mut self.state.ready) {
                let transform = self
                    .state
                    .outputs
                    .get(self.output)
                    .map_or(Transform::Normal, |output| output.transform);
                let picture = self
                    .state
                    .shm_buffer
                    .as_ref()
                    .map(|shm| shm.picture(transform));
                // Ask for the next frame before returning, so the compositor
                // draws into the buffer while the caller encodes this one.
                self.request()?;
                if let Some(picture) = picture {
                    return Ok(Some(picture));
                }
            }
            if std::mem::take(&mut self.state.failed) {
                self.request()?;
                bail!("合成器拒绝了屏幕复制");
            }
            if self.state.frame.is_none() {
                self.request()?;
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.wait(deadline - now)?;
        }
    }

    /// 零拷贝：让合成器把这一帧画进我们自己分配的显存缓冲。
    ///
    /// 回来之前就把下一帧要好了——画进另一块缓冲——于是调用方编码这一块的时候
    /// 合成器正在画那一块。两块轮换，谁也不会读到别人正在写的那一份。
    pub fn next_hardware(&mut self, timeout: Duration) -> Result<Option<Captured>> {
        let deadline = Instant::now() + timeout;
        if !self.ensure_targets(deadline)? {
            return Ok(None);
        }
        let mut ready = false;
        loop {
            if let Some(error) = self.state.error.take() {
                bail!("{error}");
            }
            if std::mem::take(&mut self.state.ready) {
                ready = true;
                break;
            }
            if std::mem::take(&mut self.state.failed) {
                bail!("合成器拒绝了显存复制");
            }
            if self.state.frame.is_none() {
                let pending = self.target.as_ref().context("没有显存缓冲")?.pending;
                self.request_into(pending)?;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            self.wait(deadline - now)?;
        }
        if !ready {
            return Ok(None);
        }
        let hardware = self.target.as_mut().context("没有显存缓冲")?;
        let done = hardware.pending;
        let buffer = hardware.buffers[done].clone();
        // 先把下一帧要上：它画进另一块，和调用方接下来对这一块的编码并行。
        hardware.pending = 1 - done;
        let next = hardware.pending;
        self.request_into(next)?;
        Ok(Some(Captured { buffer }))
    }

    /// 要一帧，并且说明它要画进哪一块缓冲。
    fn request_into(&mut self, index: usize) -> Result<()> {
        let target = self.target.as_ref().context("没有显存缓冲")?;
        self.state.dmabuf_target = Some(target.wl[index].clone());
        self.request()
    }

    /// 分配那两块显存缓冲。格式是合成器在 `linux_dmabuf` 事件里报的，所以得先
    /// 要一帧只为听它说一句；那一帧不会兑现。
    ///
    /// 说不上来（静止的屏上合成器不到下一次上屏就不会处理这个请求）就返回
    /// `false` 让调用方当「这一帧没有画面」处理，等下一次再来问，别把一次没赶上
    /// 当成采集坏掉。
    fn ensure_targets(&mut self, deadline: Instant) -> Result<bool> {
        if self.target.is_some() {
            return Ok(true);
        }
        let dmabuf = self
            .state
            .dmabuf
            .clone()
            .context("合成器没有 zwp_linux_dmabuf_v1")?;
        self.state.dmabuf_target = None;
        self.request()?;
        while self.state.offer.is_none() {
            if let Some(error) = self.state.error.take() {
                bail!("{error}");
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            self.wait(deadline - now)?;
        }
        if let Some(frame) = self.state.frame.take() {
            frame.destroy();
        }
        self.state.failed = false;
        let (format, width, height) = self.state.offer.context("合成器没有说明 dma-buf")?;
        let handle = self.queue.handle();
        let mut buffers = Vec::with_capacity(2);
        let mut wl = Vec::with_capacity(2);
        for _ in 0..2 {
            let buffer = std::sync::Arc::new(super::dmabuf::Buffer::allocate_format(
                width, height, format,
            )?);
            wl.push(buffer.wrap(&dmabuf, &handle));
            buffers.push(buffer);
        }
        let [b0, b1] = <[_; 2]>::try_from(buffers).ok().context("缓冲数量不对")?;
        let [w0, w1] = <[_; 2]>::try_from(wl).ok().context("缓冲数量不对")?;
        self.target = Some(Hardware {
            buffers: [b0, b1],
            wl: [w0, w1],
            pending: 0,
        });
        Ok(true)
    }

    /// Starts one frame; the frame object is single-use, so every copy needs a
    /// new one.
    fn request(&mut self) -> Result<()> {
        let handle = self.queue.handle();
        let manager = self.state.manager.clone().context("采集已关闭")?;
        let output = self
            .state
            .outputs
            .get(self.output)
            .context("采集的输出已消失")?;
        self.state.wanted = None;
        self.state.frame =
            Some(manager.capture_output(i32::from(self.cursor), &output.proxy, &handle, ()));
        Ok(())
    }

    /// Dispatches until an event arrives or `timeout` runs out. Events already
    /// queued are handled first, so a compositor that answers at once never
    /// waits on the socket.
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        let connection = self.state.connection.as_ref().context("采集已关闭")?;
        // `dispatch_pending` only drains what has already arrived; a request
        // made since the last wait is still in the outgoing buffer.
        connection.flush().context("发送 Wayland 请求失败")?;
        let Some(guard) = connection.prepare_read() else {
            return Ok(());
        };
        let mut poll = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Safe because `poll` only writes into the one descriptor it was given.
        let ready = unsafe { libc::poll(&mut poll, 1, millis(timeout)) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            drop(guard);
            bail!("等待 Wayland 事件失败：{error}");
        }
        if ready == 0 {
            // Dropping the guard releases the prepared read.
            drop(guard);
            return Ok(());
        }
        match guard.read() {
            Ok(_) => {}
            // The socket can report readable with nothing left to read.
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(());
            }
            Err(error) => return Err(error).context("读取 Wayland 事件失败"),
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

/// Undoes the output's transform, which the compositor leaves to the client:
/// what arrives is the framebuffer, and a monitor standing on its side shows up
/// in it lying down. The mapping is `wl_output`'s own, read as "the framebuffer
/// pixel this picture pixel is", and `Normal` — the one case that needs no work
/// — passes the rows straight through.
pub(super) fn orient(source: Vec<u8>, width: u32, height: u32, transform: Transform) -> (Vec<u8>, u32, u32) {
    if transform == Transform::Normal {
        return (source, width, height);
    }
    let turned = matches!(
        transform,
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
    );
    let (out_width, out_height) = if turned {
        (height, width)
    } else {
        (width, height)
    };
    let (last_x, last_y) = (width as i32 - 1, height as i32 - 1);
    let source_at = |x: i32, y: i32| match transform {
        Transform::_90 => (y, last_y - x),
        Transform::_180 => (last_x - x, last_y - y),
        Transform::_270 => (last_x - y, x),
        Transform::Flipped => (last_x - x, y),
        Transform::Flipped90 => (y, x),
        Transform::Flipped180 => (x, last_y - y),
        Transform::Flipped270 => (last_x - y, last_y - x),
        // `Normal`, and any transform a later revision adds.
        _ => (x, y),
    };
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    for y in 0..out_height as i32 {
        for x in 0..out_width as i32 {
            let (from_x, from_y) = source_at(x, y);
            let from = (from_y as usize * width as usize + from_x as usize) * 4;
            let to = (y as usize * out_width as usize + x as usize) * 4;
            out[to..to + 4].copy_from_slice(&source[from..from + 4]);
        }
    }
    (out, out_width, out_height)
}

fn millis(timeout: Duration) -> i32 {
    timeout.as_millis().min(i32::MAX as u128) as i32
}

/// The output the caller asked for, by the name the compositor gives it (the
/// same `DP-1` the display topology uses). A compositor too old to name its
/// outputs is matched by size, and a single-output desktop needs no match.
fn pick(outputs: &[Output], screen: &Screen) -> Result<usize> {
    if let Some(index) = outputs
        .iter()
        .position(|output| output.name.as_deref() == Some(screen.device_name.as_str()))
    {
        return Ok(index);
    }
    if outputs.len() == 1 {
        return Ok(0);
    }
    if let Some(index) = outputs
        .iter()
        .position(|output| output.name.is_none() && output.size() == (screen.width, screen.height))
    {
        return Ok(index);
    }
    bail!(
        "合成器没有提供名为 {} 的输出（可用：{}）",
        screen.device_name,
        outputs
            .iter()
            .map(|output| output.name.as_deref().unwrap_or("未命名"))
            .collect::<Vec<_>>()
            .join("、")
    )
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_screencopy_manager_v1" => {
                    state.manager = Some(
                        registry.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(
                            name,
                            version.min(3),
                            handle,
                            (),
                        ),
                    );
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                        name,
                        version.min(1),
                        handle,
                        (),
                    ));
                }
                "zwp_linux_dmabuf_v1" => {
                    state.dmabuf = Some(registry.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(
                        name,
                        version.min(3),
                        handle,
                        (),
                    ));
                }
                "wl_output" => state.outputs.push(Output {
                    proxy: registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        version.min(OUTPUT_VERSION),
                        handle,
                        (),
                    ),
                    name: None,
                    mode: (0, 0),
                    transform: Transform::Normal,
                }),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state
            .outputs
            .iter_mut()
            .find(|entry| entry.proxy == *output)
        else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.name = Some(name),
            // The transform lives here and nowhere else: no later protocol
            // revision reports it another way.
            wl_output::Event::Geometry { transform, .. } => {
                if let Ok(transform) = transform.into_result() {
                    entry.transform = transform;
                }
            }
            wl_output::Event::Mode {
                width,
                height,
                flags,
                ..
            } if flags
                .into_result()
                .is_ok_and(|flags| flags.contains(wl_output::Mode::Current)) =>
            {
                entry.mode = (width.max(0) as u32, height.max(0) as u32)
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let Ok(format) = format.into_result() else {
                    state.error = Some("合成器给出了未知的共享内存像素格式".into());
                    return;
                };
                state.wanted = Some((format as u32, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if state.manual {
                    return;
                }
                if let Some(error) = state.copy(handle).err() {
                    state.error = Some(format!("{error:#}"));
                }
            }
            // 这一版的协议只说格式和尺寸，不说 modifier：合成器把缓冲导入进来
            // 会用我们自己声明的那一个，所以线性排布是可以直接试的。
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                state.offer = Some((format, width, height));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            // A dma-buf offer is not taken up: see the module comment.
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
wayland_client::delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
wayland_client::delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

impl State {
    /// Points the compositor at the shared buffer for this frame, allocating it
    /// on the first frame and whenever the output's shape changes.
    fn copy(&mut self, handle: &QueueHandle<Self>) -> Result<()> {
        if let Some(target) = self.dmabuf_target.clone() {
            if let Some(frame) = &self.frame {
                frame.copy_with_damage(&target);
            }
            return Ok(());
        }
        let (format, width, height, stride) = self.wanted.context("帧没有说明缓冲格式")?;
        ensure!(width > 0 && height > 0 && stride > 0, "帧的缓冲尺寸无效");
        let stale = self
            .shm_buffer
            .as_ref()
            .is_none_or(|shm| !shm.matches(format, width, height, stride));
        if stale {
            let shm = self.shm.as_ref().context("采集已关闭")?;
            self.shm_buffer = Some(allocate(shm, format, width, height, stride, handle)?);
        }
        let shm = self.shm_buffer.as_ref().context("没有共享缓冲")?;
        if let Some(frame) = &self.frame {
            frame.copy_with_damage(&shm.buffer);
        }
        Ok(())
    }
}

/// The shared memory the compositor writes into: a memfd, mapped here and
/// exported as a pool with a single buffer over the whole of it.
fn allocate(
    shm: &wl_shm::WlShm,
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
    handle: &QueueHandle<State>,
) -> Result<Shm> {
    use wayland_client::protocol::wl_shm::Format;
    let format = Format::try_from(format)
        .map_err(|()| anyhow::anyhow!("不支持的共享内存像素格式 {format:#x}"))?;
    let length = stride as usize * height as usize;
    let name = c"openuuyc-screencopy";
    // Safe because the name is a valid C string and the flags are plain bits.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("无法创建共享内存：{}", std::io::Error::last_os_error());
    }
    // Safe because `memfd_create` returned a fresh descriptor on success.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Safe because `fd` is a live descriptor and the length was just computed.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) } < 0 {
        bail!("无法调整共享内存大小：{}", std::io::Error::last_os_error());
    }
    let pool = shm.create_pool(fd.as_fd(), length as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        handle,
        (),
    );
    // Safe because the mapping covers the whole of a descriptor this process
    // owns, and it is unmapped again in `Shm::drop`.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        bail!("无法映射共享内存：{}", std::io::Error::last_os_error());
    }
    Ok(Shm {
        _fd: fd,
        _pool: pool,
        map: map.cast(),
        length,
        buffer,
        format: format as u32,
        width,
        height,
        stride,
    })
}

#[cfg(test)]
mod tests {
    use super::orient;
    use wayland_client::protocol::wl_output::Transform;

    /// A `width`x`height` BGRA picture whose pixels count up from 1, one value
    /// per pixel repeated across its four bytes.
    fn numbered(width: u32, height: u32) -> Vec<u8> {
        (1..=(width * height))
            .flat_map(|value| [value as u8; 4])
            .collect()
    }

    /// The value of each pixel, in order.
    fn values(pixels: &[u8]) -> Vec<u8> {
        pixels.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }

    #[test]
    fn normal_leaves_the_rows_alone() {
        let (pixels, width, height) = orient(numbered(2, 3), 2, 3, Transform::Normal);
        assert_eq!((width, height), (2, 3));
        assert_eq!(values(&pixels), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn turning_swaps_the_shape_and_keeps_every_pixel() {
        let source = numbered(4, 3);
        let mut seen = Vec::new();
        for transform in [
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let (pixels, width, height) = orient(source.clone(), 4, 3, transform);
            let turned = matches!(
                transform,
                Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
            );
            assert_eq!(
                (width, height),
                if turned { (3, 4) } else { (4, 3) },
                "{transform:?} size"
            );
            let mut sorted = values(&pixels);
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                (1..=12).collect::<Vec<u8>>(),
                "{transform:?} lost or duplicated a pixel"
            );
            seen.push(values(&pixels));
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 7, "a transform repeats another one");
    }

    #[test]
    fn a_quarter_turn_is_clockwise_and_270_undoes_it() {
        let source = numbered(2, 3);
        let (turned, width, height) = orient(source.clone(), 2, 3, Transform::_90);
        assert_eq!((width, height), (3, 2));
        assert_eq!(values(&turned), vec![5, 3, 1, 6, 4, 2]);
        let (back, width, height) = orient(turned, 3, 2, Transform::_270);
        assert_eq!((width, height), (2, 3));
        assert_eq!(back, source);
    }

    #[test]
    fn a_flipped_quarter_turn_is_the_plain_one_mirrored() {
        let source = numbered(2, 3);
        let (plain, _, _) = orient(source.clone(), 2, 3, Transform::_90);
        let (flipped, _, _) = orient(source.clone(), 2, 3, Transform::Flipped90);
        assert_eq!(values(&plain), vec![5, 3, 1, 6, 4, 2]);
        assert_eq!(values(&flipped), vec![1, 3, 5, 2, 4, 6]);
        let (plain, _, _) = orient(source.clone(), 2, 3, Transform::_270);
        let (flipped, _, _) = orient(source, 2, 3, Transform::Flipped270);
        assert_eq!(values(&plain), vec![2, 4, 6, 1, 3, 5]);
        assert_eq!(values(&flipped), vec![6, 4, 2, 5, 3, 1]);
    }

    #[test]
    fn flipping_mirrors_without_turning() {
        let (pixels, _, _) = orient(numbered(3, 2), 3, 2, Transform::Flipped);
        assert_eq!(values(&pixels), vec![3, 2, 1, 6, 5, 4]);
        let (pixels, _, _) = orient(numbered(3, 2), 3, 2, Transform::Flipped180);
        assert_eq!(values(&pixels), vec![4, 5, 6, 1, 2, 3]);
    }
}

#[cfg(test)]
pub(crate) mod dmabuf_tests {
    use super::*;
    use crate::platform::linux::capture::dmabuf::Buffer;

    /// 测试辅助：让合成器把 `screen` 的画面画进一块我们自己分配的 dma-buf。
    /// 返回缓冲与仍然活着的 `Grab`（帧对象挂在它上面，别提前丢）。
    pub(crate) fn capture_into_dmabuf(screen: &Screen) -> Result<Buffer, anyhow::Error> {
        let mut grab = Grab::open(screen, false)?;
        // 合成器要的是它自己画布的尺寸，不是这块屏「看到」的尺寸：竖屏的输出
        // 画布是横的，尺寸报错就会被判成 invalid dmabuf parameters。
        let size = grab.state.outputs[grab.output].mode;
        let Some(dmabuf) = grab.state.dmabuf.clone() else {
            bail!("合成器没有 zwp_linux_dmabuf_v1");
        };
        let handle = grab.queue.handle();
        let output = grab.state.outputs[grab.output].proxy.clone();
        let manager = grab.state.manager.clone().context("采集已关闭")?;
        grab.state.manual = true;
        let frame = manager.capture_output(0, &output, &handle, ());
        grab.state.frame = Some(frame.clone());
        for _ in 0..10 {
            grab.queue.roundtrip(&mut grab.state)?;
            if grab.state.wanted.is_some() || grab.state.offer.is_some() {
                break;
            }
        }
        ensure!(grab.state.frame.is_some(), "帧已经被收走");
        let format = grab
            .state
            .offer
            .map(|(format, _, _)| format)
            .unwrap_or(crate::platform::linux::capture::dmabuf::ARGB8888);
        let buffer = Buffer::allocate_format(size.0, size.1, format)?;
        let wl = buffer.wrap(&dmabuf, &handle);
        frame.copy(&wl);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if grab.state.ready || grab.state.failed || grab.state.error.is_some() {
                break;
            }
            grab.wait(std::time::Duration::from_millis(200))?;
        }
        ensure!(!grab.state.failed, "合成器拒绝了 dma-buf");
        ensure!(grab.state.ready, "合成器没有在 5 秒内画进这块 dma-buf");
        // 画面已经画完，连接可以放掉：内容在缓冲里，fd 的引用我们拿着。
        Ok(buffer)
    }

    /// 合成器收不收我们自己分配的 dma-buf —— 这是零拷贝的前提。
    /// `OPENUUYC_PROBE_OUTPUT=DP-1 cargo test --release --lib -- --ignored copies_into_a_dmabuf --nocapture`
    #[test]
    #[ignore]
    fn copies_into_a_dmabuf() {
        let screens = super::super::screens().unwrap();
        let wanted = std::env::var("OPENUUYC_PROBE_OUTPUT").unwrap_or_default();
        for screen in &screens {
            if !wanted.is_empty() && screen.device_name != wanted {
                continue;
            }
            let buffer = capture_into_dmabuf(screen).expect("采集到 dma-buf 失败");
            println!(
                "{} 输出 {:?} 缓冲 {}x{} 行距 {} 尺寸 {} 格式 {:#x}",
                screen.device_name,
                (screen.width, screen.height),
                buffer.width,
                buffer.height,
                buffer.stride,
                buffer.size,
                buffer.format,
            );
            let pixels = buffer.read_back().expect("读回显存缓冲失败");
            let name = format!("/tmp/openuuyc-dmabuf-{}.bgra", screen.device_name);
            std::fs::write(&name, &pixels).unwrap();
            println!("  已写出 {name}（{} 字节）", pixels.len());
        }
    }

    /// dma-buf 那条路和共享内存那条路画出来的画面要一致，包括旋转。旋转的屏上
    /// 合成器给的缓冲尺寸和「屏幕看到的尺寸」是两回事，回到 CPU 比对才看得出来。
    /// `OPENUUYC_PROBE_OUTPUT=DP-3 cargo test --release --lib -- --ignored compares_dmabuf_with_shm --nocapture`
    #[test]
    #[ignore]
    fn compares_dmabuf_with_shm() {
        let screens = super::super::screens().unwrap();
        let wanted = std::env::var("OPENUUYC_PROBE_OUTPUT").unwrap_or_default();
        for screen in &screens {
            if !wanted.is_empty() && screen.device_name != wanted {
                continue;
            }
            let mut grab = Grab::open(screen, false).unwrap();
            let transform = grab.state.outputs[grab.output].transform;
            let mode = grab.state.outputs[grab.output].mode;
            let mut picture = None;
            for _ in 0..20 {
                if let Some(captured) = grab.next(std::time::Duration::from_millis(500)).unwrap() {
                    picture = Some(captured);
                    break;
                }
            }
            let picture = picture.expect("共享内存那条路没有采到画面");
            println!(
                "{} mode={mode:?} transform={transform:?} 共享内存 {}x{}",
                screen.device_name, picture.width, picture.height
            );
            std::fs::write(
                format!("/tmp/openuuyc-shm-{}.bgra", screen.device_name),
                &picture.pixels,
            )
            .unwrap();
            let buffer = capture_into_dmabuf(screen).expect("采集到 dma-buf 失败");
            println!("  dma-buf {}x{}", buffer.width, buffer.height);
            let raw = buffer.read_back().unwrap();
            // dma-buf 里是合成器的原始画布：先按输出自己的尺寸裁掉行距，再解旋转。
            let mut tight = Vec::with_capacity(buffer.width as usize * buffer.height as usize * 4);
            for line in raw.chunks_exact(buffer.stride as usize).take(buffer.height as usize) {
                tight.extend_from_slice(&line[..buffer.width as usize * 4]);
            }
            let (pixels, width, height) = orient(tight, buffer.width, buffer.height, transform);
            println!("  解旋转后 {width}x{height}");
            std::fs::write(
                format!("/tmp/openuuyc-dmabuf-{}.bgra", screen.device_name),
                &pixels,
            )
            .unwrap();
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::super::{output, scale, screens};
    use super::{Grab, orient};
    use std::time::{Duration, Instant};

    /// 需要在图形会话里跑：
    /// `cargo test --release --lib -- --ignored profiles_the_wlr_path --nocapture`
    #[test]
    #[ignore]
    fn profiles_the_wlr_path() {
        for screen in screens().unwrap() {
            let mut grab = Grab::open(&screen, false).unwrap();
            let transform = grab.state.outputs[grab.output].transform;
            let mode = grab.state.outputs[grab.output].mode;
            let synthetic = vec![7u8; mode.0 as usize * mode.1 as usize * 4];
            let at = Instant::now();
            let turned = orient(synthetic, mode.0, mode.1, transform);
            let orient_ms = at.elapsed();
            std::hint::black_box(turned);

            let at = Instant::now();
            let mut sampler = super::super::super::cursor_shape::Sampler::default();
            let mut sampling = Duration::ZERO;
            let mut rounds = 0;
            while at.elapsed() < Duration::from_secs(1) {
                let call = Instant::now();
                let sampled = sampler.sample();
                sampling += call.elapsed();
                rounds += 1;
                let _ = std::hint::black_box(sampled);
            }
            let refresh_at = Instant::now();
            let refreshed = super::super::refresh(&screen);
            println!(
                "{} mode={mode:?} transform={transform:?} orient={orient_ms:?} \
                 sample_avg={:?} ({rounds}) refresh={:?} ok={}",
                screen.device_name,
                sampling / rounds.max(1),
                refresh_at.elapsed(),
                refreshed.is_ok()
            );

            for quality in [1, 2, 4, 6] {
                let started = Instant::now();
                let (mut frames, mut empty) = (0u64, 0u64);
                let (mut waited, mut scaling) = (Duration::ZERO, Duration::ZERO);
                let mut size = (0, 0);
                while started.elapsed() < Duration::from_secs(4) {
                    let at = Instant::now();
                    match grab.next(Duration::from_millis(33)) {
                        Ok(Some(picture)) => {
                            waited += at.elapsed();
                            frames += 1;
                            size = (picture.width, picture.height);
                            let to = output(picture.width, picture.height, quality, (3840, 2160));
                            let at = Instant::now();
                            let scaled = scale(&picture.pixels, (picture.width, picture.height), to);
                            scaling += at.elapsed();
                            std::hint::black_box(scaled);
                        }
                        Ok(None) => empty += 1,
                        Err(error) => {
                            println!("{} error: {error}", screen.device_name);
                            break;
                        }
                    }
                }
                let divisor = frames.max(1) as u32;
                println!(
                    "{} quality={quality} source={size:?} delivered={frames} empty={empty} \
                     next_avg={:?} scale_avg={:?} target={:?}",
                    screen.device_name,
                    waited / divisor,
                    scaling / divisor,
                    output(size.0, size.1, quality, (3840, 2160))
                );
            }
        }
    }
}
