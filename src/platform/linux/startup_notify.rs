//! Startup notification for a window that already exists: a second start of
//! the program hands its launcher's token to the running process, which uses
//! it to raise its control center and to end the launcher's busy feedback.
//!
//! winit applies a token only while creating a window
//! (`WindowAttributesExtStartupNotify::with_activation_token`); its Wayland
//! `focus_window` does nothing and its X11 one sends no timestamp. So for a
//! window that is already there this module speaks the protocols itself:
//!
//! - X11 ([startup-notification spec]): `_NET_STARTUP_ID` is set on the window
//!   before it is mapped, so the window manager's focus-stealing prevention
//!   sees the launch; a window that is already mapped is raised with
//!   `_NET_ACTIVE_WINDOW` and the timestamp the ID carries; then the
//!   `remove:` message ends the launch.
//! - Wayland (`xdg_activation_v1`): the token activates the window's surface,
//!   through winit's own connection.
//!
//! The same machinery raises the control center when the click came from the
//! notification area: there is no launcher to hand us a token, so [`focus`]
//! asks the compositor for one itself.
//!
//! [startup-notification spec]: https://specifications.freedesktop.org/startup-notification-spec/startup-notification-latest.txt
use anyhow::{Context, Result, bail};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use winit::window::Window;

/// The tokens a launcher passed to the program, read from its environment.
/// GLib sets both on Wayland; X11 launchers set only `DESKTOP_STARTUP_ID`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LaunchToken {
    /// `XDG_ACTIVATION_TOKEN`, for Wayland.
    pub(crate) wayland: Option<String>,
    /// `DESKTOP_STARTUP_ID`, for X11.
    pub(crate) x11: Option<String>,
}

impl LaunchToken {
    pub(crate) fn from_env() -> Self {
        let read = |name| std::env::var(name).ok().filter(|token| valid(token));
        Self {
            wayland: read("XDG_ACTIVATION_TOKEN"),
            x11: read("DESKTOP_STARTUP_ID"),
        }
    }

    /// The token for a window on this display server.
    pub(crate) fn for_window(&self, window: &Window) -> Option<&str> {
        match window.window_handle().map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Wayland(_)) => self.wayland.as_deref(),
            Ok(RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_)) => self.x11.as_deref(),
            _ => None,
        }
    }
}

/// Tokens are opaque, but they travel in a text line and an X11 message:
/// printable ASCII without spaces, quotes or backslashes, of a sane length.
pub(crate) fn valid(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 512
        && token
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\')
}

/// Show `window` (mapping it if it is hidden) and raise it for a launch that
/// carried `token`. `show` maps the window; it runs after the launch is
/// announced on the window, as the X11 window manager reads it at map time.
pub(crate) fn raise(window: &Window, token: &str, show: impl FnOnce()) -> Result<()> {
    anyhow::ensure!(valid(token), "启动通知令牌无效");
    let display = window
        .display_handle()
        .context("读取窗口的显示连接失败")?
        .as_raw();
    match window.window_handle().context("读取窗口句柄失败")?.as_raw() {
        RawWindowHandle::Xlib(handle) => x11::raise(handle.window as u32, token, show),
        RawWindowHandle::Xcb(handle) => x11::raise(handle.window.get(), token, show),
        RawWindowHandle::Wayland(handle) => {
            let RawDisplayHandle::Wayland(display) = display else {
                bail!("Wayland 窗口没有 Wayland 显示连接");
            };
            show();
            // SAFETY: both pointers come from the live winit window, which
            // outlives this call.
            unsafe { wayland::activate(display.display.as_ptr(), handle.surface.as_ptr(), token) }
        }
        _ => bail!("这个窗口系统不支持启动通知"),
    }
}

/// Raise and focus an already mapped window for a click that did not come with
/// a launcher's token — the notification area's "打开 OpenUUYC", or a
/// notification action.
///
/// Wayland has no channel from the tray host to pass the click's input serial,
/// so the token this asks for carries a placeholder serial. Compositors are
/// free to downgrade that to an urgency hint; niri's
/// `honor-xdg-activation-with-invalid-serial` and `on-xdg-activate` window rule
/// turn it into a real focus. X11 uses the pager source of
/// `_NET_ACTIVE_WINDOW`, which window managers accept without a timestamp.
pub(crate) fn focus(window: &Window) -> Result<()> {
    let display = window
        .display_handle()
        .context("读取窗口的显示连接失败")?
        .as_raw();
    match window.window_handle().context("读取窗口句柄失败")?.as_raw() {
        RawWindowHandle::Xlib(handle) => x11::focus(handle.window as u32),
        RawWindowHandle::Xcb(handle) => x11::focus(handle.window.get()),
        RawWindowHandle::Wayland(handle) => {
            let RawDisplayHandle::Wayland(display) = display else {
                bail!("Wayland 窗口没有 Wayland 显示连接");
            };
            // SAFETY: both pointers come from the live winit window, which
            // outlives this call.
            unsafe { wayland::focus(display.display.as_ptr(), handle.surface.as_ptr()) }
        }
        _ => bail!("这个窗口系统不支持窗口激活"),
    }
}

pub(super) mod x11 {
    use anyhow::{Context, Result};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::wrapper::ConnectionExt as _;

    x11rb::atom_manager! {
        pub Atoms: AtomsCookie {
            _NET_STARTUP_ID,
            _NET_STARTUP_INFO,
            _NET_STARTUP_INFO_BEGIN,
            _NET_ACTIVE_WINDOW,
            UTF8_STRING,
        }
    }

    pub(in super::super) fn raise(window: u32, token: &str, show: impl FnOnce()) -> Result<()> {
        let (connection, screen) = x11rb::connect(None).context("连接 X 服务器失败")?;
        let root = connection.setup().roots[screen].root;
        let atoms = Atoms::new(&connection)?.reply()?;
        let mapped = announce(&connection, &atoms, window, token)?;
        show();
        if mapped {
            activate(&connection, &atoms, root, window, timestamp(token))?;
        }
        complete(&connection, &atoms, root, token)?;
        connection.flush()?;
        Ok(())
    }

    /// Raise a window that is already mapped. Source 1 marks the request as a
    /// pager's, which window managers honour without a user timestamp.
    pub(in super::super) fn focus(window: u32) -> Result<()> {
        let (connection, screen) = x11rb::connect(None).context("连接 X 服务器失败")?;
        let root = connection.setup().roots[screen].root;
        let atoms = Atoms::new(&connection)?.reply()?;
        activate(&connection, &atoms, root, window, x11rb::CURRENT_TIME)?;
        connection.flush()?;
        Ok(())
    }

    /// Put the launch's ID on the window before the window manager maps or
    /// raises it. Reports whether the window is already mapped.
    pub(in super::super) fn announce(
        connection: &impl Connection,
        atoms: &Atoms,
        window: u32,
        token: &str,
    ) -> Result<bool> {
        connection
            .change_property8(
                xproto::PropMode::REPLACE,
                window,
                atoms._NET_STARTUP_ID,
                atoms.UTF8_STRING,
                token.as_bytes(),
            )?
            .check()
            .context("设置窗口的启动通知 ID 失败")?;
        let attributes = connection.get_window_attributes(window)?.reply()?;
        Ok(attributes.map_state != xproto::MapState::UNMAPPED)
    }

    /// Ask the window manager to raise an already mapped window, as a pager
    /// would (source 1), with the launch's time so focus-stealing prevention
    /// compares it with the user's last input.
    pub(in super::super) fn activate(
        connection: &impl Connection,
        atoms: &Atoms,
        root: u32,
        window: u32,
        time: u32,
    ) -> Result<()> {
        let event = xproto::ClientMessageEvent::new(
            32,
            window,
            atoms._NET_ACTIVE_WINDOW,
            [1, time, 0, 0, 0],
        );
        connection.send_event(
            false,
            root,
            xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        Ok(())
    }

    /// End the launch so the launcher stops its busy cursor or bouncing icon:
    /// a `remove:` message to the root window in 20-byte pieces, sent from a
    /// window of this connection as the spec asks.
    pub(in super::super) fn complete(
        connection: &impl Connection,
        atoms: &Atoms,
        root: u32,
        token: &str,
    ) -> Result<()> {
        let mut message = format!("remove: ID=\"{token}\"").into_bytes();
        message.push(0);
        let sender = connection.generate_id()?;
        connection.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            sender,
            root,
            -100,
            -100,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new().override_redirect(1),
        )?;
        let mut kind = atoms._NET_STARTUP_INFO_BEGIN;
        for chunk in message.chunks(20) {
            let mut data = [0u8; 20];
            data[..chunk.len()].copy_from_slice(chunk);
            let event = xproto::ClientMessageEvent::new(8, sender, kind, data);
            connection.send_event(false, root, xproto::EventMask::PROPERTY_CHANGE, event)?;
            kind = atoms._NET_STARTUP_INFO;
        }
        connection.destroy_window(sender)?;
        Ok(())
    }

    /// The X server time a launcher put after `_TIME` in its ID, or
    /// `CurrentTime` when it put none.
    pub(in super::super) fn timestamp(token: &str) -> u32 {
        token
            .rsplit_once("_TIME")
            .and_then(|(_, time)| time.parse().ok())
            .unwrap_or(x11rb::CURRENT_TIME)
    }
}

mod wayland {
    use anyhow::{Context, Result};
    use wayland_client::backend::{Backend, ObjectId};
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat::WlSeat, wl_surface::WlSurface};
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
    use wayland_protocols::xdg::activation::v1::client::{
        xdg_activation_token_v1::{self, XdgActivationTokenV1},
        xdg_activation_v1::XdgActivationV1,
    };

    #[derive(Default)]
    struct State {
        /// 合成器在 `done` 里给出的令牌；拒绝激活时是空串。
        token: Option<String>,
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
    wayland_client::delegate_noop!(State: ignore XdgActivationV1);
    wayland_client::delegate_noop!(State: ignore WlSeat);

    impl Dispatch<XdgActivationTokenV1, ()> for State {
        fn event(
            state: &mut Self,
            _: &XdgActivationTokenV1,
            event: xdg_activation_token_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_activation_token_v1::Event::Done { token } = event {
                state.token = Some(token);
            }
        }
    }

    /// Activate winit's surface with `token` over winit's own connection, on
    /// a private event queue so winit's events are left alone.
    ///
    /// # Safety
    ///
    /// `display` and `surface` must be the live `wl_display` and `wl_surface`
    /// of a window that outlives this call.
    pub(super) unsafe fn activate(
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
        token: &str,
    ) -> Result<()> {
        // SAFETY: the caller guarantees the display; the guest backend does
        // not close it.
        let connection =
            Connection::from_backend(unsafe { Backend::from_foreign_display(display.cast()) });
        // SAFETY: the caller guarantees a live wl_surface proxy.
        let surface = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }
            .context("窗口的 Wayland 表面无效")?;
        let surface =
            WlSurface::from_id(&connection, surface).context("窗口的 Wayland 表面无效")?;
        // One registry per activation stays with the connection (wl_registry
        // has no destructor); activations follow user launches, so they are few.
        let (globals, mut queue) =
            registry_queue_init::<State>(&connection).context("枚举 Wayland 全局对象失败")?;
        let activation: XdgActivationV1 = globals
            .bind(&queue.handle(), 1..=1, ())
            .context("合成器不支持 xdg_activation_v1，无法激活窗口")?;
        activation.activate(token.to_owned(), &surface);
        activation.destroy();
        queue
            .roundtrip(&mut State::default())
            .context("激活窗口失败")?;
        Ok(())
    }

    /// 托盘点击发生在状态栏进程里，Wayland 没有把那次点击的输入序号交给我们
    /// 的通道。这里用一个占位序号换令牌：合成器可以只把它当紧急提示，niri 的
    /// `honor-xdg-activation-with-invalid-serial` 则会当成真正的聚焦请求。
    ///
    /// # Safety
    ///
    /// `display` and `surface` must be the live `wl_display` and `wl_surface`
    /// of a window that outlives this call.
    pub(super) unsafe fn focus(
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    ) -> Result<()> {
        // SAFETY: the caller guarantees the display and a live wl_surface proxy.
        let connection =
            Connection::from_backend(unsafe { Backend::from_foreign_display(display.cast()) });
        let surface = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }
            .context("窗口的 Wayland 表面无效")?;
        let surface =
            WlSurface::from_id(&connection, surface).context("窗口的 Wayland 表面无效")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&connection).context("枚举 Wayland 全局对象失败")?;
        let activation: XdgActivationV1 = globals
            .bind(&queue.handle(), 1..=1, ())
            .context("合成器不支持 xdg_activation_v1，无法激活窗口")?;
        let seat: WlSeat = globals
            .bind(&queue.handle(), 1..=1, ())
            .context("合成器没有 wl_seat，无法激活窗口")?;
        // 先带上占位序号（niri 的 honor-xdg-activation-with-invalid-serial 靠它
        // 真正聚焦）；合成器不认就退回不带序号的令牌，至少把窗口标成紧急。
        let token = match request_token(&activation, &mut queue, &surface, Some(&seat))? {
            Some(token) => token,
            None => request_token(&activation, &mut queue, &surface, None)?
                .context("合成器没有发放激活令牌")?,
        };
        activation.activate(token, &surface);
        activation.destroy();
        queue
            .roundtrip(&mut State::default())
            .context("激活窗口失败")?;
        Ok(())
    }

    fn request_token(
        activation: &XdgActivationV1,
        queue: &mut wayland_client::EventQueue<State>,
        surface: &WlSurface,
        seat: Option<&WlSeat>,
    ) -> Result<Option<String>> {
        let mut state = State::default();
        let token = activation.get_activation_token(&queue.handle(), ());
        if let Some(seat) = seat {
            token.set_serial(0, seat);
        }
        token.set_surface(surface);
        token.commit();
        queue.roundtrip(&mut state).context("申请激活令牌失败")?;
        token.destroy();
        Ok(state.token.filter(|token| !token.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_validated_and_timestamps_read() {
        assert!(valid("gnome-shell/OpenUUYC/2194-0-host_TIME1234567"));
        assert!(valid("4c2f0a7e-1b36-4a8a-9d0a-3d6c3e1f9b21"));
        assert!(!valid(""));
        assert!(!valid("with space"));
        assert!(!valid("quote\"d"));
        assert!(!valid("back\\slash"));
        assert!(!valid("line\nbreak"));
        assert!(!valid(&"x".repeat(513)));
        assert_eq!(x11::timestamp("host-1-2_TIME1234567"), 1234567);
        assert_eq!(x11::timestamp("a_TIME1_TIME42"), 42);
        assert_eq!(x11::timestamp("no-time"), x11rb::CURRENT_TIME);
        assert_eq!(x11::timestamp("bad_TIMEx"), x11rb::CURRENT_TIME);
    }

    /// Needs an X server without anything of the user's on it, for example
    /// `Xvfb :99 & DISPLAY=:99 cargo test -- --ignored startup_notify`.
    #[test]
    #[ignore = "needs a private X server in DISPLAY"]
    fn x11_announces_raises_and_completes_a_launch() {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{self, ConnectionExt as _};
        let (connection, screen) = x11rb::connect(None).unwrap();
        let root = connection.setup().roots[screen].root;
        let atoms = x11::Atoms::new(&connection).unwrap().reply().unwrap();
        let window = connection.generate_id().unwrap();
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                64,
                64,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &xproto::CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        let token = "test-host-1-0_TIME77";
        // Unmapped, as a control center hidden in the tray.
        assert!(!x11::announce(&connection, &atoms, window, token).unwrap());
        let property = connection
            .get_property(
                false,
                window,
                atoms._NET_STARTUP_ID,
                atoms.UTF8_STRING,
                0,
                128,
            )
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(property.value, token.as_bytes());
        connection.map_window(window).unwrap().check().unwrap();
        assert!(x11::announce(&connection, &atoms, window, token).unwrap());
        x11::activate(&connection, &atoms, root, window, x11::timestamp(token)).unwrap();
        x11::complete(&connection, &atoms, root, token).unwrap();
        // Every request went through: a round trip reports no error.
        connection.get_input_focus().unwrap().reply().unwrap();
    }
}
