//! 不让桌面自己对这些远端桌面需要的快捷键动手。
//!
//! 「拦截快捷键」是播放器许下的承诺：它转发的按键到此为止，Windows 播放器用低级
//! 键盘钩子兑现它。Linux 桌面会在焦点窗口看到按键之前先截走这些绑定：X11 窗口管理器
//! 靠被动按键 grab，Wayland 合成器则干脆连事件都不下发。两者各有各的答案，本模块
//! 按窗口所在的桌面挑一个：
//!
//! - Wayland：`zwp_keyboard_shortcuts_inhibit_v1`，合成器自己的协议。持有 inhibitor
//!   的窗口在持有键盘期间拿到自己的按键，合成器的绑定让位。合成器把它限定在焦点上，
//!   可能先问用户（GNOME 会问一次），并保留一个属于它自己的逃生快捷键。
//! - X11：对播放器窗口做主动键盘 grab，在 winit 自己的连接上建立，好让按键照旧经
//!   winit 到达，且只在窗口持有 X 输入焦点时才抓，所以抓取不移动焦点。
//!
//! 是否挡开快捷键由共享的 input gate 决定；本模块只是那套机制。
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

/// 桌面自己的快捷键，对一个播放器窗口挡开或放开。
pub(super) enum Shortcuts {
    Wayland(Inhibitor),
    X11(super::linux_keyboard_grab::KeyboardGrab),
}

impl Shortcuts {
    /// 桌面没有办法挡开自己的快捷键时返回 `None`（没有该协议的合成器）；此时它们
    /// 就留给桌面。
    pub(super) fn new(window: &winit::window::Window) -> Option<Self> {
        match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Wayland(_) => {
                let inhibitor = Inhibitor::new(window);
                if inhibitor.is_none() {
                    tracing::info!(
                        "the compositor does not offer keyboard-shortcuts-inhibit; its shortcuts stay with the desktop"
                    );
                }
                inhibitor.map(Self::Wayland)
            }
            RawDisplayHandle::Xlib(_) => {
                super::linux_keyboard_grab::KeyboardGrab::new(window).map(Self::X11)
            }
            _ => None,
        }
    }

    /// 跟随共享的门。`intercepts` 是快捷键规则（开关、远端收键盘、没有录制器或
    /// 文本框）。每帧和每个窗口事件都会调用；重复当前状态不花代价。
    pub(super) fn update(
        &mut self,
        gate: super::input_gate::InputGate,
        context: &egui::Context,
        intercepts: bool,
    ) {
        match self {
            // 合成器把 inhibitor 限定在焦点 surface 上，所以它跨焦点变化保留，而不是
            // 每次 focus-in 都重建（那样还会再问一次 GNOME 的授权）。
            Self::Wayland(inhibitor) => {
                inhibitor.set_active(intercepts && gate.holds_unless_unfocused(context))
            }
            // X11 grab 会把键盘从所有窗口手里拿走，所以它跟完整的门走，焦点一离开
            // 这个窗口就放开。
            Self::X11(grab) => grab.set_active(intercepts && gate.holds(context)),
        }
    }
}

pub(super) struct Inhibitor {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    manager: manager::ZwpKeyboardShortcutsInhibitManagerV1,
    inhibitor: Option<inhibitor::ZwpKeyboardShortcutsInhibitorV1>,
}

#[derive(Default)]
struct State {
    /// 合成器对当前 inhibitor 的最后答复：答复之前是 `None`。先问用户的合成器只在
    /// 用户答复后才有答复；直接拒绝的则永远不答复。
    active: Option<bool>,
    /// 第一次授权记 info，之后的（每次 focus-in）记 debug。
    granted_once: bool,
}

impl Inhibitor {
    fn new(window: &winit::window::Window) -> Option<Self> {
        let display = match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Wayland(handle) => handle.display.as_ptr(),
            _ => return None,
        };
        let surface = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Wayland(handle) => handle.surface.as_ptr(),
            _ => return None,
        };
        // 安全：display 属于事件循环，它比每个窗口、每个播放器都活得久（播放器在
        // `exiting` 时析构），而借用外部 display 的 backend 永远不会关闭它借来的连接。
        let backend =
            unsafe { wayland_client::backend::Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        // 安全：surface 由 winit 创建且窗口还活着。winit 的 proxy 由同一个
        // wayland-backend 管理，所以这个 id 带着 winit 的存活标志：一旦 winit 销毁
        // 该 surface，指向它的请求会失败，而不是去碰已释放的内存。
        let id = unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast()) }
            .ok()?;
        let surface = wl_surface::WlSurface::from_id(&connection, id).ok()?;
        let (globals, queue) = registry_queue_init::<State>(&connection).ok()?;
        let handle = queue.handle();
        let manager: manager::ZwpKeyboardShortcutsInhibitManagerV1 =
            globals.bind(&handle, 1..=1, ()).ok()?;
        let seat: wl_seat::WlSeat = match globals.bind(&handle, 1..=1, ()) {
            Ok(seat) => seat,
            Err(_) => {
                manager.destroy();
                let _ = connection.flush();
                return None;
            }
        };
        Some(Self {
            connection,
            queue,
            state: State::default(),
            surface,
            seat,
            manager,
            inhibitor: None,
        })
    }

    fn set_active(&mut self, active: bool) {
        // winit 读 socket 时答复会到达这条队列，在这里取出来，只用来记日志。
        let previous = self.state.active;
        let _ = self.queue.dispatch_pending(&mut self.state);
        if self.state.active != previous {
            match self.state.active {
                Some(true) if !self.state.granted_once => {
                    self.state.granted_once = true;
                    tracing::info!("the compositor holds its shortcuts back");
                }
                Some(true) => tracing::debug!("the compositor holds its shortcuts back"),
                // 焦点丢失时发出，用户用合成器的逃生快捷键把快捷键拿回去时也发出。
                // inhibitor 一直放着不动，直到门关闭再打开，好让用户的选择生效。
                Some(false) => tracing::debug!("the compositor restored its own shortcuts"),
                None => {}
            }
        }
        if active == self.inhibitor.is_some() {
            return;
        }
        if active {
            self.state.active = None;
            let handle = self.queue.handle();
            self.inhibitor =
                Some(
                    self.manager
                        .inhibit_shortcuts(&self.surface, &self.seat, &handle, ()),
                );
        } else if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
            self.state.active = None;
        }
        let _ = self.connection.flush();
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
        }
        self.manager.destroy();
        let _ = self.connection.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore manager::ZwpKeyboardShortcutsInhibitManagerV1);

impl Dispatch<inhibitor::ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &inhibitor::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            inhibitor::Event::Active => state.active = Some(true),
            inhibitor::Event::Inactive => state.active = Some(false),
            _ => {}
        }
    }
}
