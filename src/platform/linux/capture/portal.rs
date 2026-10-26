//! Capture through the XDG ScreenCast portal, Sunshine's `portalgrab` path:
//! the desktop's own compositor hands over each shared monitor as a PipeWire
//! stream. It is the sanctioned way to capture a Wayland session, and works
//! on X11 desktops whose portal backend offers it (GNOME, KDE).
//!
//! The first session asks the local user which monitors to share. Sharing is
//! requested as persistent, and the restore token the portal returns is kept,
//! so later sessions (including after a restart) start without asking again
//! until the user revokes it.
//!
//! One portal session serves every captured monitor of this process, and its
//! cursor mode is fixed when it starts; a desktop wanting the other mode gets
//! its own session.
use super::Picture;
use super::Screen;
use super::dmabuf;
use super::pipewire::Stream;
use crate::platform::CompositorLayout;
use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// One shared monitor as the portal described it.
#[derive(Clone, Debug)]
struct Source {
    node: u32,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

struct Session {
    runtime: tokio::runtime::Runtime,
    proxy: Screencast,
    session: ashpd::desktop::Session<Screencast>,
    sources: Vec<Source>,
    cursor: bool,
}

/// Run portal I/O on a thread of its own, so it never blocks inside (or
/// panics because of) whatever runtime the caller may be part of.
fn on_portal<T: Send>(
    runtime: &tokio::runtime::Runtime,
    work: impl std::future::Future<Output = T> + Send,
) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| runtime.block_on(work))
            .join()
            .expect("portal thread panicked")
    })
}

fn token_path() -> Result<std::path::PathBuf> {
    Ok(crate::platform::paths::require_local_app_data()?.join("OpenUUYC/screencast-restore-token"))
}

/// `path` 里的恢复令牌。空文件（用户在隐私设置里撤销后门户写回来的样子）
/// 与读不到一样，都当作没有。
fn token_at(path: &std::path::Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

/// 门户上次给的恢复令牌。
fn saved_token() -> Option<String> {
    token_at(token_path().ok()?.as_path())
}

/// 用户以前授权过、门户能凭令牌直接恢复会话，这次开采集不会再问他。
pub(super) fn remembered() -> bool {
    saved_token().is_some()
}

impl Session {
    fn start(cursor: bool) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("screencast-portal")
            .enable_all()
            .build()?;
        // 取令牌失败要让这次开门户直接失败，不能当成「没有令牌」继续（那是上游的语义）。
        let token = token_at(token_path()?.as_path());
        let started = on_portal(&runtime, async {
            let proxy = Screencast::new()
                .await
                .context("桌面没有提供 ScreenCast 门户")?;
            let session = proxy.create_session(Default::default()).await?;
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_sources(Some(SourceType::Monitor.into()))
                        .set_multiple(true)
                        .set_cursor_mode(if cursor {
                            CursorMode::Embedded
                        } else {
                            CursorMode::Hidden
                        })
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(token.as_deref()),
                )
                .await?
                .response()?;
            // Without a usable restore token this shows the sharing dialog on
            // this machine and waits for the local user.
            let streams = proxy
                .start(&session, None, StartCastOptions::default())
                .await?
                .response()
                .context("本机用户没有同意屏幕共享")?;
            let sources = streams
                .streams()
                .iter()
                .map(|stream| Source {
                    node: stream.pipe_wire_node_id(),
                    position: stream.position(),
                    size: stream.size(),
                })
                .collect::<Vec<_>>();
            let restore = streams.restore_token().map(str::to_owned);
            anyhow::Ok((proxy, session, sources, restore))
        })?;
        let (proxy, session, sources, restore) = started;
        if let Some(restore) = restore {
            let path = token_path()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, restore)?;
        }
        if sources.is_empty() {
            bail!("屏幕共享没有包含任何显示器");
        }
        tracing::info!(?sources, cursor, "ScreenCast portal session started");
        Ok(Self {
            runtime,
            proxy,
            session,
            sources,
            cursor,
        })
    }

    /// The shared session with this cursor mode, started if none is alive.
    fn shared(cursor: bool) -> Result<Arc<Self>> {
        static SESSIONS: Mutex<Vec<Weak<Session>>> = Mutex::new(Vec::new());
        // Held across start: a restore token is single-use, so two sessions
        // starting together would leave one of them asking the user again.
        let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|session| session.strong_count() > 0);
        if let Some(session) = sessions
            .iter()
            .filter_map(Weak::upgrade)
            .find(|session| session.cursor == cursor)
        {
            return Ok(session);
        }
        let session = Arc::new(Self::start(cursor)?);
        sessions.push(Arc::downgrade(&session));
        Ok(session)
    }

    /// 哪一条共享的流是 `screen`。
    fn source(&self, screen: &Screen) -> Result<&Source> {
        // 每次都问布局，而不是等显示服务器的矩形失配了才问：混合缩放的桌面上那个
        // 矩形可能是另一块屏的（见 `pick`）。不在 Wayland 会话里时它立刻返回，在
        // 会话里也只有本进程第一次问要等一个来回。
        let layout = crate::platform::display::compositor_layout_soon();
        if let Some(index) = pick(&self.sources, screen, layout.as_deref()) {
            return Ok(&self.sources[index]);
        }
        // 一次没配上的报告要能看出是哪一边的问题。
        tracing::warn!(
            screen = %screen.device_name,
            left = screen.left,
            top = screen.top,
            width = screen.width,
            height = screen.height,
            sources = ?self.sources,
            layout = layout.is_some(),
            layout_output = ?layout.as_deref().and_then(|layout| layout.output(&screen.device_name)),
            "门户共享的屏幕里找不到所选显示器"
        );
        Err(anyhow!(unmatched(
            &self.sources,
            screen,
            layout.as_deref(),
            super::wayland_session(),
        )))
    }
}

/// `pick` 为什么没给 `screen` 找到流，说成人能照做的话。
///
/// 缩放的桌面上没配上很少是「这块屏没被授权共享」，照那么说会让用户白跑一趟去撤销
/// 再重授；真正缺的是用来配对的那份布局，或者查名字用的那个输出名。
fn unmatched(
    sources: &[Source],
    screen: &Screen,
    layout: Option<&CompositorLayout>,
    wayland: bool,
) -> String {
    if sources.len() > 1 && sources.iter().all(|s| s.position.is_none()) {
        return "门户没有给出共享屏幕的位置，无法区分共享的多块屏".to_owned();
    }
    match layout {
        None if wayland => "读不到合成器的输出布局（需要 Wayland 会话和 xdg-output），\
             缩放的桌面上无法确认门户共享的是哪块屏"
            .to_owned(),
        Some(layout) if layout.output(&screen.device_name).is_none() => format!(
            "合成器布局里没有名为 {} 的输出（XWayland 23.1 之前输出名不是连接器名），\
             缩放的桌面上无法确认门户共享的是哪块屏",
            screen.device_name
        ),
        _ => "所选显示器不在本机授权共享的屏幕中".to_owned(),
    }
}

/// 门户这一批流里哪一条是 `screen`，按下标给。
///
/// 门户按合成器布局描述显示器，`Screen` 装的是显示服务器给出的矩形：缩放为 1 的
/// 会话两者同一个矩形，缩放之后怎么差就是合成器的事了——Mutter、KWin 上的
/// XWayland 把位置和尺寸都乘上该屏缩放，xwayland-satellite 则保留逻辑位置、按输出
/// 自己的像素给尺寸。所以只要能，配对就靠合成器自己的布局——门户描述显示器的那
/// 个空间——经输出名（`DP-1`，RandR 也拿它当显示器名）对到屏幕；显示服务器的矩形
/// 只在没有布局点得出这块屏时才用。
///
/// 这里不做任何猜测：布局点不出名的屏、或矩形对不上布局的流，都留着不认，交给调用
/// 方报错，而不是去采一块没人要的屏。
fn pick(sources: &[Source], screen: &Screen, layout: Option<&CompositorLayout>) -> Option<usize> {
    // 布局点得出这块屏时，就只按布局配对。显示服务器的矩形在缩放的桌面上属于另一
    // 个空间，而且可能正好是另一块屏的门户矩形：Mutter 上的 XWayland 把混着 200%
    // 和 100% 的整个桌面按 2 缩放，于是两块 200% 屏里的第二块，位置正是门户给第三
    // 块 100% 屏的位置。
    if let Some(output) = layout.and_then(|layout| layout.output(&screen.device_name)) {
        if let Some(index) = sources.iter().position(|s| {
            s.position == Some((output.left, output.top))
                // 门户与 xdg-output 各自把分数的逻辑尺寸取整、方式不一定一样（niri 的
                // ScreenCast 门户截断，smithay 的 xdg-output 四舍五入），尺寸可能差一
                // 个单位；位置是整个布局坐标，从不取整。
                && s.size.is_some_and(|(width, height)| {
                    (i64::from(width) - i64::from(output.width)).abs() <= 1
                        && (i64::from(height) - i64::from(output.height)).abs() <= 1
                })
        }) {
            return Some(index);
        }
    } else {
        // 没有布局可问（X11 会话），或者布局不认识这个名字：两个空间只在缩放为 1
        // 时一致，那就只能按那个矩形配。
        let rectangle = (
            (screen.left, screen.top),
            (screen.width as i32, screen.height as i32),
        );
        if let Some(index) = sources
            .iter()
            .position(|s| s.position == Some(rectangle.0) && s.size == Some(rectangle.1))
        {
            return Some(index);
        }
    }
    match sources {
        // 门户只共享了一块、又没给几何，那只能是要的那块。
        [only] if only.position.is_none() => Some(0),
        _ => None,
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = on_portal(&self.runtime, self.session.close());
        let _ = &self.proxy;
    }
}

pub(super) struct Grab {
    stream: Stream,
    session: Arc<Session>,
    /// 上层让不让走显存（`hardware_images` 传进来的）。门户的 dma-buf 协商在连接
    /// 时就定了、关不掉重开代价又大，所以这个开关只决定「我们放不放显存帧出去」：
    /// 关掉就报 `hardware()` 为假，上层照旧走系统内存。
    enabled: bool,
    /// 显存读回失败只警告一次（不可映射的 dma-buf 之类），免得每帧刷屏。
    readback_warned: bool,
}

impl Grab {
    /// 协商由 [`format_object`](super::pipewire) 那一份格式对象定：它同时要线性
    /// dma-buf 与共享内存，合成器挑一个给得出的。所以这里不再需要「要不要显存」
    /// 的入参——拿到什么由门户决定，上层用 `set_hardware` 决定用不用。
    pub fn open(screen: &Screen, cursor: bool) -> Result<Self> {
        let session = Session::shared(cursor)?;
        let node = session.source(screen)?.node;
        let fd = on_portal(
            &session.runtime,
            session
                .proxy
                .open_pipe_wire_remote(&session.session, OpenPipeWireRemoteOptions::default()),
        )
        .context("无法打开屏幕共享的 PipeWire 连接")?;
        let stream = Stream::connect(fd, node)?;
        Ok(Self {
            stream,
            session,
            enabled: false,
            readback_warned: false,
        })
    }

    pub fn name(&self) -> &'static str {
        "XDG 门户 · PipeWire"
    }

    /// Whether the stream draws the cursor into its pictures.
    pub fn cursor(&self) -> bool {
        self.session.cursor
    }

    /// 现在走不走显存：得「协商成了 dma-buf」**且**「上层没把它关掉」。
    pub fn hardware(&self) -> bool {
        self.enabled && self.stream.dmabuf_active()
    }

    /// 门户的 dma-buf 协商在连接时就定了、重开代价大，所以这里不改协商，只记下
    /// 上层要不要用，并照它回报「现在走不走显存」。这样 `hardware_images(false)`
    /// 能真的把这条路关掉退回系统内存，而不是自相矛盾地恒为真。
    pub fn set_hardware(&mut self, enabled: bool) -> bool {
        self.enabled = enabled;
        self.hardware()
    }

    /// 这块屏「看到」的尺寸，也就是协商出来的画面尺寸。
    pub fn displayed_size(&self) -> (u32, u32) {
        self.stream.displayed_size()
    }

    /// 迟迟等不到第一帧时，画面流停在哪一步；见 [`super::pipewire::Stream::stage`]。
    pub fn stage(&self) -> String {
        self.stream.stage()
    }

    /// 零拷贝：把这一帧的显存缓冲直接交出去（内容仍在合成器池子里，靠缓冲上的
    /// 租约在帧用完时归还）。没有显存画面就给 `None`。
    pub fn next_hardware(&mut self, timeout: Duration) -> Result<Option<Arc<dmabuf::Buffer>>> {
        Ok(self.stream.next(timeout)?.and_then(|delivered| match delivered {
            super::pipewire::Delivered::Dmabuf(buffer) => Some(buffer),
            // 谈崩退回共享内存了；这一帧没有显存画面可言，交给上层当「没帧」处理。
            super::pipewire::Delivered::Shm { .. } => None,
        }))
    }

    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        let Some(delivered) = self.stream.next(timeout)? else {
            return Ok(None);
        };
        // 读回一帧显存画面失败（dma-buf 不可映射之类）只丢这一帧，不能让它冒出去
        // 把整条采集循环结束掉：门户下一帧还会给画面。只在第一次记一条警告。
        match delivered.into_cpu() {
            Ok(picture) => Ok(Some(picture)),
            Err(error) => {
                if self.readback_warned {
                    tracing::debug!(%error, "显存画面读回失败，跳过这一帧");
                } else {
                    self.readback_warned = true;
                    tracing::warn!(%error, "显存画面读回失败，跳过这一帧");
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod negotiation_tests {
    use super::*;

    /// 真机诊断：开一次门户，把两种协商（共享内存 / 线性 dma-buf）的结果都打出来。
    /// 需要本机门户与已经授权过的屏幕共享（`screencast-restore-token`），所以默认忽略：
    ///
    /// ```text
    /// PIPEWIRE_DEBUG=4 cargo test --lib -- --ignored --nocapture portal_negotiation
    /// ```
    #[test]
    #[ignore = "需要本机门户与已授权的屏幕共享"]
    fn portal_negotiation() {
        let screens = crate::platform::capture::screens().expect("枚举显示器失败");
        println!("本机屏幕：");
        for screen in &screens {
            println!(
                "  {} left/top={:?} 尺寸={:?}",
                screen.device_name,
                (screen.left, screen.top),
                (screen.width, screen.height)
            );
        }
        // 门户只共享了它授权的那块屏，所以逐块试，看哪块能认上、以及它的协商结果。
        for screen in &screens {
            match Grab::open(screen, false) {
                Ok(grab) => println!(
                    "{}: 开流成功，协商到{}，尺寸={:?}",
                    screen.device_name,
                    if grab.stream.dmabuf_active() {
                        "线性 dma-buf"
                    } else {
                        "共享内存"
                    },
                    grab.displayed_size()
                ),
                Err(error) => println!("{}: 失败：{error:#}", screen.device_name),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Source, pick, unmatched};
    use crate::media::capture::Screen;
    use crate::platform::{CompositorLayout, CompositorOutput, Rotation};

    /// 门户报的一条流。
    fn stream(x: i32, y: i32, width: i32, height: i32) -> Source {
        Source {
            node: 0,
            position: Some((x, y)),
            size: Some((width, height)),
        }
    }

    /// 显示服务器交给 `Screen` 的一块屏。
    fn screen(device_name: &str, left: i32, top: i32, width: u32, height: u32) -> Screen {
        Screen {
            id: 0,
            device_name: device_name.to_owned(),
            display_name: device_name.to_owned(),
            width,
            height,
            left,
            top,
            primary: false,
            fps: 60,
            dpi_scale: None,
            hdr: false,
            adapter: 0,
            render_adapter: None,
            identity: None,
        }
    }

    /// 同一块屏在合成器布局里的样子，正放。
    fn output(name: &str, left: i32, top: i32, width: u32, height: u32) -> CompositorOutput {
        CompositorOutput {
            name: Some(name.to_owned()),
            left,
            top,
            width,
            height,
            rotation: Rotation::Normal,
        }
    }

    fn layout(outputs: Vec<CompositorOutput>) -> CompositorLayout {
        CompositorLayout { outputs }
    }

    /// 缩放为 1 的桌面，两边报的是同一个矩形。
    #[test]
    fn an_unscaled_desktop_matches_the_display_servers_rectangle() {
        let sources = [stream(0, 0, 1920, 1080), stream(1920, 0, 1920, 1080)];
        assert_eq!(
            pick(&sources, &screen("DP-2", 1920, 0, 1920, 1080), None),
            Some(1)
        );
    }

    /// GNOME 50 与三块 4K 并排、前两块 200%、第三块 100%：XWayland 把整个桌面按
    /// 2 缩放，于是第二块的 RandR 矩形正是第三块的门户矩形。布局点得出每一块的名，
    /// 每块都必须找到自己那条流。
    #[test]
    fn mixed_scales_pair_by_the_layout_even_where_a_rectangle_collides() {
        let sources = [
            stream(0, 0, 1920, 1080),
            stream(1920, 0, 1920, 1080),
            stream(3840, 0, 3840, 2160),
        ];
        let layout = layout(vec![
            output("DP-1", 0, 0, 1920, 1080),
            output("DP-2", 1920, 0, 1920, 1080),
            output("DP-3", 3840, 0, 3840, 2160),
        ]);
        let screens = [
            screen("DP-1", 0, 0, 3840, 2160),
            screen("DP-2", 3840, 0, 3840, 2160),
            screen("DP-3", 7680, 0, 7680, 4320),
        ];
        for (index, wanted) in screens.iter().enumerate() {
            assert_eq!(
                pick(&sources, wanted, Some(&layout)),
                Some(index),
                "{}",
                wanted.device_name
            );
        }
    }

    /// GNOME 50 与 KDE 那样三块 4K 并排、缩放 2：门户报逻辑矩形，XWayland 把位置
    /// 和尺寸都乘了缩放。认第二块必须落到第二条流——第三条流才拿得到显示服务器为
    /// 它报的那个矩形，也正是按形状配对过去会落错的地方。
    #[test]
    fn a_scaled_desktop_matches_the_stream_by_the_compositors_layout() {
        let sources = [
            stream(0, 0, 1920, 1080),
            stream(1920, 0, 1920, 1080),
            stream(3840, 0, 1920, 1080),
        ];
        let layout = layout(vec![
            output("DP-1", 0, 0, 1920, 1080),
            output("DP-2", 1920, 0, 1920, 1080),
            output("DP-3", 3840, 0, 1920, 1080),
        ]);
        assert_eq!(
            pick(
                &sources,
                &screen("DP-2", 3840, 0, 3840, 2160),
                Some(&layout)
            ),
            Some(1)
        );
    }

    /// 这台机器上出过问题的那套桌面：niri 套 xwayland-satellite，一块 5120x2160
    /// 缩放 1.35，旁边一块 2560x1600 缩放 1.6、竖着放。显示服务器把输出摆在合成器
    /// 的逻辑位置、却按该输出自己的像素给尺寸，所以分数缩放的那块光看尺寸就露馅。
    ///
    /// 尺寸确实差一个单位：5120 / 1.35 = 3792.59，niri 的 ScreenCast 门户截断成
    /// 3792（`logical_output()` 那次 cast），smithay 的 xdg-output 四舍五入成 3793。
    /// 容差要盖的正是这里，不是某个工具包替 XWayland 取整。
    #[test]
    fn a_fractionally_scaled_monitor_matches_by_its_name() {
        let sources = [stream(0, 0, 1000, 1600), stream(1000, 0, 3792, 1600)];
        let layout = layout(vec![
            CompositorOutput {
                rotation: Rotation::Rotate90,
                ..output("DP-3", 0, 0, 1000, 1600)
            },
            output("DP-1", 1000, 0, 3793, 1600),
        ]);
        assert_eq!(
            pick(
                &sources,
                &screen("DP-1", 1000, 0, 5120, 2160),
                Some(&layout)
            ),
            Some(1)
        );
        // 转过的屏由显示服务器把宽高对调，布局照样点得出它的名：不用再试另一个朝向。
        assert_eq!(
            pick(&sources, &screen("DP-3", 0, 0, 1600, 2560), Some(&layout)),
            Some(0)
        );
    }

    /// 门户与 xdg-output 各自取整分数的逻辑尺寸、方式不一定一样（见上），差一个
    /// 单位还算配对；位置是整个布局坐标、从不取整，两块屏可能只隔一个单位。
    #[test]
    fn a_rounded_size_pairs_but_a_shifted_position_does_not() {
        let layout = layout(vec![output("DP-2", 1920, 0, 1920, 1080)]);
        let second = screen("DP-2", 1920, 0, 1920, 1080);
        assert_eq!(
            pick(&[stream(1920, 0, 1919, 1079)], &second, Some(&layout)),
            Some(0)
        );
        assert_eq!(
            pick(&[stream(1921, 0, 1920, 1080)], &second, Some(&layout)),
            None
        );
    }

    /// 两个空间说法不一致、又没有布局可以定夺，就留着不认：交给调用方报错，而不是
    /// 去采一块可能是别人屏幕的屏。
    #[test]
    fn a_scaled_desktop_without_a_layout_is_not_guessed_at() {
        let sources = [stream(0, 0, 1920, 1080), stream(3840, 0, 1920, 1080)];
        let wanted = screen("DP-2", 3840, 0, 3840, 2160);
        assert_eq!(pick(&sources, &wanted, None), None);
        // XWayland 23.1 之前把每个输出都叫 `XWAYLAND0…`、不叫连接器名，于是没得查。
        let layout = layout(vec![output("DP-1", 0, 0, 1920, 1080)]);
        assert_eq!(
            pick(
                &sources,
                &screen("XWAYLAND1", 3840, 0, 3840, 2160),
                Some(&layout)
            ),
            None
        );
    }

    /// 门户只共享了一块又没给几何，那只能是要的那块；两块就说不清了。
    #[test]
    fn a_lone_stream_without_geometry_is_the_monitor_asked_for() {
        let wanted = screen("DP-1", 1000, 0, 5120, 2160);
        let only = [Source {
            node: 0,
            position: None,
            size: None,
        }];
        assert_eq!(pick(&only, &wanted, None), Some(0));
        let two = [
            Source {
                node: 0,
                position: None,
                size: None,
            },
            Source {
                node: 1,
                position: None,
                size: None,
            },
        ];
        assert_eq!(pick(&two, &wanted, None), None);
    }

    /// 每一种没配上都要分开说，别在授权本来就没问题时让用户去重授一遍。
    #[test]
    fn a_miss_says_what_was_missing() {
        let sources = [stream(0, 0, 1920, 1080)];
        let wanted = screen("DP-2", 3840, 0, 3840, 2160);
        assert!(unmatched(&sources, &wanted, None, true).contains("读不到合成器的输出布局"));
        let other = layout(vec![output("DP-1", 0, 0, 1920, 1080)]);
        let named = unmatched(&sources, &wanted, Some(&other), true);
        assert!(named.contains("没有名为 DP-2 的输出"), "{named}");
        // X11 会话没有布局可读，显示服务器的矩形就是门户的：那时没配上就是没共享。
        assert_eq!(
            unmatched(&sources, &wanted, None, false),
            "所选显示器不在本机授权共享的屏幕中"
        );
        let knows = layout(vec![output("DP-2", 1920, 0, 1920, 1080)]);
        assert_eq!(
            unmatched(&sources, &wanted, Some(&knows), true),
            "所选显示器不在本机授权共享的屏幕中"
        );
        let blind = [
            Source {
                node: 0,
                position: None,
                size: None,
            },
            Source {
                node: 1,
                position: None,
                size: None,
            },
        ];
        assert!(unmatched(&blind, &wanted, Some(&knows), true).contains("没有给出共享屏幕的位置"));
    }
}
