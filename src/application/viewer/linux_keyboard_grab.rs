//! 主动的 X11 键盘 grab，Windows 键盘钩子的 X11 对应物：播放器持有输入期间，窗口
//! 管理器的被动按键 grab（Alt+Tab、Super 以及它自己的绑定）就看不到这些按键了。
//!
//! grab 在 winit 自己的 Xlib 连接上以 `owner_events` 建立，服务器照旧把按键报到
//! winit 的窗口；换一条连接去 grab，按键就会被送到那条连接。只在播放器窗口持有 X
//! 输入焦点时才抓，所以抓取不移动焦点。服务器只用 core FocusOut/FocusIn 事件报告
//! 聚焦窗口上的 grab；winit 跟的是 XInput2 焦点，它没动，所以窗口不会被告知失去了
//! 焦点（下面的 Xvfb 测试正是在验证这一点）。
//!
//! grab 会挡住其它所有窗口的键盘，所以门一关、焦点一丢、播放器一消失就立刻放开；
//! 窗口被 unmap 或连接关闭时，服务器自己也会把它丢掉。
use std::ffi::{c_int, c_ulong, c_void};
use std::time::{Duration, Instant};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

const CURRENT_TIME: c_ulong = 0;
const GRAB_MODE_ASYNC: c_int = 1;
const TRUE: c_int = 1;
/// 被拒绝的 grab 等待多久再试下一次。
const RETRY: Duration = Duration::from_millis(500);
/// 放开键盘到再次抓取之间的最短间隔。万一某个 X 服务器真把 grab 报成焦点变化，
/// 那么在那次「焦点丢失」上放开、又在下一个 focus-in 上抓取就会互相追逐；这个
/// 间隔把它限制成一次缓慢可见的闪烁，而不是空转。
const REGRAB: Duration = Duration::from_millis(250);

type Display = c_void;

struct Xlib {
    grab_keyboard:
        unsafe extern "C" fn(*mut Display, c_ulong, c_int, c_int, c_int, c_ulong) -> c_int,
    ungrab_keyboard: unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
    get_input_focus: unsafe extern "C" fn(*mut Display, *mut c_ulong, *mut c_int) -> c_int,
    flush: unsafe extern "C" fn(*mut Display) -> c_int,
    // 让上面那些函数指针保持有效。winit 为同一个 display 加载了同一个库，所以这里
    // 只是多持有一份引用。
    _library: libloading::Library,
}

impl Xlib {
    fn open() -> Option<Self> {
        // 安全：加载 libX11 不会运行我们的代码，且这些符号都按其 C 原型声明。
        unsafe {
            let library = libloading::Library::new("libX11.so.6").ok()?;
            Some(Self {
                grab_keyboard: *library.get(b"XGrabKeyboard\0").ok()?,
                ungrab_keyboard: *library.get(b"XUngrabKeyboard\0").ok()?,
                get_input_focus: *library.get(b"XGetInputFocus\0").ok()?,
                flush: *library.get(b"XFlush\0").ok()?,
                _library: library,
            })
        }
    }
}

/// 上一次为什么没抓成，让每种原因只记一次日志。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Refusal {
    /// X 输入焦点在别的窗口上（或在边框、root 上）。
    Unfocused(c_ulong),
    /// `XGrabKeyboard` 的状态：AlreadyGrabbed、GrabInvalidTime、
    /// GrabNotViewable 或 GrabFrozen。
    Status(c_int),
}

pub(super) struct KeyboardGrab {
    xlib: Xlib,
    display: *mut Display,
    window: c_ulong,
    grabbed: bool,
    /// 在这个时刻之前不再尝试：被拒绝或放开之后。
    retry_at: Option<Instant>,
    retry: Duration,
    regrab: Duration,
    refused: Option<Refusal>,
}

impl KeyboardGrab {
    /// 窗口不是 Xlib 窗口、或找不到 libX11 时返回 `None`。
    pub(super) fn new(window: &winit::window::Window) -> Option<Self> {
        let display = match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Xlib(handle) => handle.display?.as_ptr(),
            _ => return None,
        };
        let window = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(handle) => handle.window,
            _ => return None,
        };
        let grab = Self::from_raw(display, window);
        if grab.is_none() {
            tracing::warn!("libX11 is missing XGrabKeyboard; window-manager shortcuts stay local");
        }
        grab
    }

    /// `display` 必须在本值存活期间一直开着；winit 的 display 属于事件循环，它比
    /// 每个播放器都活得久。
    fn from_raw(display: *mut Display, window: c_ulong) -> Option<Self> {
        Some(Self {
            xlib: Xlib::open()?,
            display,
            window,
            grabbed: false,
            retry_at: None,
            retry: RETRY,
            regrab: REGRAB,
            refused: None,
        })
    }

    /// 抓取或放开键盘。重复当前状态不发 X 请求；被拒绝的 grab 最多每 [`RETRY`]
    /// 重试一次。
    pub(super) fn set_active(&mut self, active: bool) {
        if !active {
            self.release();
            return;
        }
        if self.grabbed
            || self
                .retry_at
                .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return;
        }
        match self.grab() {
            Ok(()) => {
                self.grabbed = true;
                self.retry_at = None;
                self.refused = None;
                tracing::debug!(window = self.window, "keyboard grabbed for the remote");
            }
            Err(refusal) => {
                self.retry_at = Some(Instant::now() + self.retry);
                if self.refused != Some(refusal) {
                    self.refused = Some(refusal);
                    tracing::info!(
                        ?refusal,
                        "the keyboard could not be grabbed; window-manager shortcuts stay local for now"
                    );
                }
            }
        }
    }

    fn focus(&self) -> c_ulong {
        let mut focus: c_ulong = 0;
        let mut revert: c_int = 0;
        // 安全：display 开着（见 `from_raw`），两个出参指针在本次调用中都有效。
        unsafe { (self.xlib.get_input_focus)(self.display, &mut focus, &mut revert) };
        focus
    }

    fn grab(&self) -> Result<(), Refusal> {
        let focus = self.focus();
        if focus != self.window {
            return Err(Refusal::Unfocused(focus));
        }
        // 同上安全；窗口是 winit 的，且在持有焦点期间仍然 mapped。
        let status = unsafe {
            (self.xlib.grab_keyboard)(
                self.display,
                self.window,
                TRUE,
                GRAB_MODE_ASYNC,
                GRAB_MODE_ASYNC,
                CURRENT_TIME,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Refusal::Status(status))
        }
    }

    fn release(&mut self) {
        if !self.grabbed {
            // 门一关就忘掉被拒绝这件事；下次开门立刻重试并记录自己的结果。
            if self.refused.take().is_some() {
                self.retry_at = None;
            }
            return;
        }
        self.grabbed = false;
        self.retry_at = Some(Instant::now() + self.regrab);
        // 安全：display 开着（见 `from_raw`）。放开一个服务器已经丢掉的 grab 是空操作。
        unsafe {
            (self.xlib.ungrab_keyboard)(self.display, CURRENT_TIME);
            (self.xlib.flush)(self.display);
        }
        tracing::debug!(window = self.window, "keyboard released");
    }
}

impl Drop for KeyboardGrab {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    //! 跑在私有的 X 服务器上，绝不能用桌面的：
    //! `Xvfb :98 & DISPLAY=:98 cargo test --lib linux_keyboard_grab -- --ignored --test-threads=1`
    use super::*;
    use std::ffi::{c_char, c_uint};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, GrabStatus};

    const REVERT_TO_PARENT: c_int = 2;
    const POINTER_ROOT: c_ulong = 1;

    fn private_display() -> String {
        let name = std::env::var("DISPLAY").unwrap();
        assert!(
            name != ":0" && name != ":1",
            "point DISPLAY at a private Xvfb, not the desktop"
        );
        name
    }

    /// 测试用来顶替 winit 的那几个 Xlib 调用。
    struct Client {
        open_display: unsafe extern "C" fn(*const c_char) -> *mut Display,
        close_display: unsafe extern "C" fn(*mut Display) -> c_int,
        default_root_window: unsafe extern "C" fn(*mut Display) -> c_ulong,
        create_simple_window: unsafe extern "C" fn(
            *mut Display,
            c_ulong,
            c_int,
            c_int,
            c_uint,
            c_uint,
            c_uint,
            c_ulong,
            c_ulong,
        ) -> c_ulong,
        map_window: unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
        unmap_window: unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
        set_input_focus: unsafe extern "C" fn(*mut Display, c_ulong, c_int, c_ulong) -> c_int,
        sync: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
        _library: libloading::Library,
    }

    impl Client {
        fn open() -> Self {
            unsafe {
                let library = libloading::Library::new("libX11.so.6").unwrap();
                Self {
                    open_display: *library.get(b"XOpenDisplay\0").unwrap(),
                    close_display: *library.get(b"XCloseDisplay\0").unwrap(),
                    default_root_window: *library.get(b"XDefaultRootWindow\0").unwrap(),
                    create_simple_window: *library.get(b"XCreateSimpleWindow\0").unwrap(),
                    map_window: *library.get(b"XMapWindow\0").unwrap(),
                    unmap_window: *library.get(b"XUnmapWindow\0").unwrap(),
                    set_input_focus: *library.get(b"XSetInputFocus\0").unwrap(),
                    sync: *library.get(b"XSync\0").unwrap(),
                    _library: library,
                }
            }
        }
    }

    /// 是否已有客户端持有键盘，从第二条连接去问，就像别的程序（或窗口管理器）会
    /// 做的那样。
    fn keyboard_taken(other: &x11rb::rust_connection::RustConnection, root: u32) -> bool {
        let reply = other
            .grab_keyboard(
                false,
                root,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .unwrap()
            .reply()
            .unwrap();
        if reply.status == GrabStatus::SUCCESS {
            other.ungrab_keyboard(x11rb::CURRENT_TIME).unwrap();
            other.flush().unwrap();
            false
        } else {
            assert_eq!(reply.status, GrabStatus::ALREADY_GRABBED);
            true
        }
    }

    #[test]
    #[ignore = "needs a private X server: DISPLAY=:98 with Xvfb :98 running"]
    fn grab_follows_the_gate_and_the_focus() {
        let name = private_display();
        let client = Client::open();
        let (other, screen) = x11rb::connect(Some(&name)).unwrap();
        let other_root = other.setup().roots[screen].root;
        unsafe {
            let display = (client.open_display)(std::ptr::null());
            assert!(!display.is_null());
            let root = (client.default_root_window)(display);
            let window = (client.create_simple_window)(display, root, 0, 0, 64, 64, 0, 0, 0);
            (client.map_window)(display, window);
            (client.sync)(display, 0);
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);

            let mut grab = KeyboardGrab::from_raw(display, window).unwrap();
            grab.retry = Duration::ZERO;
            grab.regrab = Duration::ZERO;

            // 门关着：什么都没持有。
            grab.set_active(false);
            assert!(!keyboard_taken(&other, other_root));

            // 门开着且窗口有焦点：键盘被拿走，再问一次也不会有新东西。
            grab.set_active(true);
            assert!(grab.grabbed);
            assert!(keyboard_taken(&other, other_root));
            grab.set_active(true);
            assert!(keyboard_taken(&other, other_root));

            // 门关上：立刻放开。
            grab.set_active(false);
            assert!(!grab.grabbed);
            assert!(!keyboard_taken(&other, other_root));

            // 焦点在别处：grab 被拒绝，而不是去移动焦点。
            (client.set_input_focus)(display, POINTER_ROOT, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            grab.set_active(true);
            assert!(!grab.grabbed);
            assert!(matches!(grab.refused, Some(Refusal::Unfocused(_))));
            assert!(!keyboard_taken(&other, other_root));

            // 另一个客户端持有键盘：被拒绝，稍后重试。
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            other
                .grab_keyboard(
                    false,
                    other_root,
                    x11rb::CURRENT_TIME,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .unwrap()
                .reply()
                .unwrap();
            grab.set_active(true);
            assert!(!grab.grabbed);
            assert_eq!(grab.refused, Some(Refusal::Status(1)));
            other.ungrab_keyboard(x11rb::CURRENT_TIME).unwrap();
            other.flush().unwrap();
            grab.set_active(true);
            assert!(grab.grabbed);
            assert!(keyboard_taken(&other, other_root));

            // 被拒绝后要等重试时刻，而不是每帧都去问。
            grab.set_active(false);
            grab.retry = Duration::from_secs(60);
            (client.set_input_focus)(display, POINTER_ROOT, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            grab.set_active(true);
            assert!(grab.refused.is_some());
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            grab.set_active(true);
            assert!(!grab.grabbed);
            // 关上门就忘掉这次拒绝；下次开门会重试。
            grab.set_active(false);
            grab.set_active(true);
            assert!(grab.grabbed);
            grab.retry = Duration::ZERO;

            // unmap 窗口会让服务器那边丢掉 grab；之后的放开就是无害的空操作。
            (client.unmap_window)(display, window);
            (client.sync)(display, 0);
            assert!(!keyboard_taken(&other, other_root));
            grab.set_active(false);
            (client.map_window)(display, window);
            (client.sync)(display, 0);
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);

            // 析构播放器会放开持有的 grab。
            grab.set_active(true);
            assert!(keyboard_taken(&other, other_root));
            drop(grab);
            assert!(!keyboard_taken(&other, other_root));

            (client.close_display)(display);
        }
    }

    /// winit 测试观察到的东西。
    #[derive(Default)]
    struct Seen {
        /// 每个 `Focused` 事件的 (focused, step)。
        focus: Vec<(bool, u32)>,
        /// (key, pressed, synthetic, 当时是否已 grab)。
        keys: Vec<(winit::keyboard::PhysicalKey, bool, bool, bool)>,
        taken_while_grabbed: Option<bool>,
        taken_after_release: Option<bool>,
    }

    struct App {
        other: x11rb::rust_connection::RustConnection,
        root: u32,
        window: Option<winit::window::Window>,
        grab: Option<KeyboardGrab>,
        step: u32,
        step_at: Instant,
        seen: Seen,
    }

    impl App {
        fn next(&mut self) {
            self.step += 1;
            self.step_at = Instant::now();
        }

        fn type_key(&self, keycode: u8) {
            use x11rb::protocol::xtest::ConnectionExt as _;
            for kind in [2, 3] {
                self.other
                    .xtest_fake_input(kind, keycode, 0, x11rb::NONE, 0, 0, 0)
                    .unwrap();
            }
            self.other.flush().unwrap();
        }
    }

    impl winit::application::ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            if self.window.is_none() {
                let attributes = winit::window::Window::default_attributes()
                    .with_inner_size(winit::dpi::PhysicalSize::new(200, 200));
                self.window = Some(event_loop.create_window(attributes).unwrap());
                self.next();
            }
        }

        fn window_event(
            &mut self,
            _: &winit::event_loop::ActiveEventLoop,
            _: winit::window::WindowId,
            event: winit::event::WindowEvent,
        ) {
            match event {
                winit::event::WindowEvent::Focused(focused) => {
                    self.seen.focus.push((focused, self.step));
                    // 和 presenter 一样：真正的焦点丢失会结束 grab。
                    if !focused && let Some(grab) = self.grab.as_mut() {
                        grab.set_active(false);
                    }
                    if focused && self.step == 2 {
                        let window = self.window.as_ref().unwrap();
                        let mut grab = KeyboardGrab::new(window).unwrap();
                        grab.set_active(true);
                        assert!(grab.grabbed, "{:?}", grab.refused);
                        self.grab = Some(grab);
                        self.next();
                    }
                }
                winit::event::WindowEvent::KeyboardInput {
                    event,
                    is_synthetic,
                    ..
                } => {
                    let grabbed = self.grab.as_ref().is_some_and(|grab| grab.grabbed);
                    self.seen.keys.push((
                        event.physical_key,
                        event.state == winit::event::ElementState::Pressed,
                        is_synthetic,
                        grabbed,
                    ));
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            use x11rb::protocol::xproto::{GrabMode, InputFocus, ModMask};
            let waited = self.step_at.elapsed();
            let settle = Duration::from_millis(300);
            let handle = self
                .window
                .as_ref()
                .map(|window| window.window_handle().unwrap().as_raw());
            let window = || match handle {
                Some(RawWindowHandle::Xlib(handle)) => handle.window as u32,
                _ => unreachable!(),
            };
            match self.step {
                1 if waited > settle => {
                    self.other
                        .set_input_focus(InputFocus::PARENT, window(), x11rb::CURRENT_TIME)
                        .unwrap();
                    self.other.flush().unwrap();
                    self.next();
                }
                3 if waited > settle => {
                    self.seen.taken_while_grabbed = Some(keyboard_taken(&self.other, self.root));
                    // 在 A 上做一个窗口管理器绑定，就像 Alt+Tab 或 Super。
                    self.other
                        .grab_key(
                            false,
                            self.root,
                            ModMask::ANY,
                            38,
                            GrabMode::ASYNC,
                            GrabMode::ASYNC,
                        )
                        .unwrap()
                        .check()
                        .unwrap();
                    self.type_key(38);
                    self.next();
                }
                // 另一个窗口拿走焦点，就像在别处点了一下。
                4 if waited > settle => {
                    self.other
                        .set_input_focus(
                            InputFocus::POINTER_ROOT,
                            InputFocus::POINTER_ROOT,
                            x11rb::CURRENT_TIME,
                        )
                        .unwrap();
                    self.other.flush().unwrap();
                    self.next();
                }
                5 if waited > settle => {
                    self.seen.taken_after_release = Some(keyboard_taken(&self.other, self.root));
                    self.other
                        .set_input_focus(InputFocus::PARENT, window(), x11rb::CURRENT_TIME)
                        .unwrap();
                    self.other.flush().unwrap();
                    self.next();
                }
                // 焦点回来后再次 grab，然后在仍有焦点时放开，就像门关上时那样
                // （释放快捷键、菜单）。
                6 if waited > settle => {
                    let grab = self.grab.as_mut().unwrap();
                    grab.set_active(true);
                    assert!(grab.grabbed, "{:?}", grab.refused);
                    self.next();
                }
                7 if waited > settle => {
                    self.grab.as_mut().unwrap().set_active(false);
                    self.next();
                }
                8 if waited > settle => {
                    self.type_key(38);
                    self.type_key(56);
                    self.next();
                }
                9 if waited > settle => event_loop.exit(),
                _ if waited > Duration::from_secs(5) => panic!("stuck at step {}", self.step),
                _ => {}
            }
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(20),
            ));
        }

        fn exiting(&mut self, _: &winit::event_loop::ActiveEventLoop) {
            self.grab = None;
            self.window = None;
        }
    }

    /// grab 不能让 winit 丢掉它的按键或焦点，并且要拦住一个窗口管理器绑定，不让它
    /// 拿走按键。
    #[test]
    #[ignore = "needs a private X server: DISPLAY=:98 with Xvfb :98 running"]
    fn winit_keeps_its_keys_and_focus_under_the_grab() {
        use winit::keyboard::{KeyCode, PhysicalKey};
        use winit::platform::x11::EventLoopBuilderExtX11 as _;
        let name = private_display();
        let (other, screen) = x11rb::connect(Some(&name)).unwrap();
        let root = other.setup().roots[screen].root;
        let event_loop = winit::event_loop::EventLoop::builder()
            .with_x11()
            .with_any_thread(true)
            .build()
            .unwrap();
        let mut app = App {
            other,
            root,
            window: None,
            grab: None,
            step: 0,
            step_at: Instant::now(),
            seen: Seen::default(),
        };
        event_loop.run_app(&mut app).unwrap();
        let seen = app.seen;
        eprintln!("focus: {:?}", seen.focus);
        eprintln!("keys: {:?}", seen.keys);
        assert_eq!(seen.taken_while_grabbed, Some(true));
        assert_eq!(seen.taken_after_release, Some(false));
        let real = |code, grabbed| {
            seen.keys
                .iter()
                .filter(|(key, _, synthetic, during)| {
                    *key == PhysicalKey::Code(code) && !synthetic && *during == grabbed
                })
                .map(|(_, pressed, _, _)| *pressed)
                .collect::<Vec<_>>()
        };
        // grab 生效时被绑定的键会到达 winit；不生效时那个绑定会把它拿走，就像窗口
        // 管理器会做的那样；其它键两种情况都能到达。
        assert_eq!(real(KeyCode::KeyA, true), vec![true, false]);
        assert_eq!(real(KeyCode::KeyA, false), Vec::<bool>::new());
        assert_eq!(real(KeyCode::KeyB, false), vec![true, false]);
        // 抓取或放开 grab 不会移动 winit 报告的焦点（第 3、4、7、8 步都没有），
        // 所以 presenter 绝不会因为自己的 grab 而释放输入。真正的焦点丢失会在 grab
        // 生效期间被报告（第 5 步）并放开 grab；第 9 步那个绑定做的 grab 会被报告，
        // 和它一向如此一样。
        let at = |step| {
            seen.focus
                .iter()
                .filter(|(_, at)| *at == step)
                .map(|(focused, _)| *focused)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(3), Vec::<bool>::new(), "{:?}", seen.focus);
        assert_eq!(at(4), Vec::<bool>::new(), "{:?}", seen.focus);
        assert_eq!(at(5), vec![false], "{:?}", seen.focus);
        assert_eq!(at(6), vec![true], "{:?}", seen.focus);
        assert_eq!(at(7), Vec::<bool>::new(), "{:?}", seen.focus);
        assert_eq!(at(8), Vec::<bool>::new(), "{:?}", seen.focus);
        assert!(at(9).contains(&false), "{:?}", seen.focus);
    }
}
