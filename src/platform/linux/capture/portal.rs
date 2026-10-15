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
use anyhow::{Context, Result, bail};
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

    /// The shared monitor that is `screen`, matched by its desktop rectangle.
    fn source(&self, screen: &Screen) -> Result<&Source> {
        let rectangle = (
            (screen.left, screen.top),
            (screen.width as i32, screen.height as i32),
        );
        if let Some(source) = self
            .sources
            .iter()
            .find(|s| s.position == Some(rectangle.0) && s.size == Some(rectangle.1))
        {
            return Ok(source);
        }
        match self.sources.as_slice() {
            // A single shared monitor without geometry can only be this one.
            [only] if only.position.is_none() => Ok(only),
            _ => bail!("所选显示器不在本机授权共享的屏幕中"),
        }
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
    /// `dmabuf` 是「这次会话能力上能不能吃显存帧」——真就照它去和门户协商
    /// DMA-BUF。探针这类只解码一两帧看能力的地方传 `false`，绝不能让它们撞上
    /// 合成器的 dma-buf（不可映射就 mmap 失败，整个能力检查都起不来）。
    pub fn open(screen: &Screen, cursor: bool, dmabuf: bool) -> Result<Self> {
        let session = Session::shared(cursor)?;
        let node = session.source(screen)?.node;
        let fd = on_portal(
            &session.runtime,
            session
                .proxy
                .open_pipe_wire_remote(&session.session, OpenPipeWireRemoteOptions::default()),
        )
        .context("无法打开屏幕共享的 PipeWire 连接")?;
        // `dmabuf` 由上层给：只有本机的编码器能把 dma-buf 导进 GPU、且这次是
        // 真采集时才去要显存帧。要来了没人能读只会每帧多一次 mmap 读回，还不如
        // 让合成器拷进共享内存。合成器给不出线性缓冲时它自己会退回共享内存。
        let stream = Stream::connect(fd, node, dmabuf)?;
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
