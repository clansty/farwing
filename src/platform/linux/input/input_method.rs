//! Text through `zwp_input_method_v2`.
//!
//! Chromium 系客户端（Edge、Chrome、Electron 应用）收到 `zwp_virtual_keyboard_v1`
//! 的 keymap 后会重建自己的 `wl_keyboard`，重建出来的那份拿的是合成器的默认键位表，
//! 于是整段文字只有第一个字符是对的（`@*+-` 变成数字、中文变成乱码）。输入法协议
//! 是另一条路：文本经由合成器交给焦点客户端，完全不经过 keymap。
//!
//! 前提是客户端启用了 text-input：Chromium 默认不带 Wayland IME，要加
//! `--enable-wayland-ime`。没启用时对象一直 inactive，调用方退回虚拟键盘。
//!
//! **Chromium 每次提交都会吞掉第一个字符**（niri + Chromium 实测；GTK 与终端不会）。
//! 所以对 Chromium 系前台，提交前先垫一个零宽空格当牺牲品。
//!
//! 协议要求输入法客户端同时持有 `zwp_virtual_keyboard_v1`，这里照做；我们不碰它，
//! 也不 grab 键盘，所以不影响本机键盘的正常输入。
use anyhow::{Context, Result};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1 as vk_manager;

#[allow(missing_docs, dead_code)]
pub(crate) mod protocol {
    use wayland_client;
    #[allow(unused_imports)]
    use wayland_client::protocol::*;
    pub mod __interfaces {
        #[allow(unused_imports)]
        use wayland_client::protocol::__interfaces::*;
        #[allow(unused_imports)]
        use wayland_protocols::wp::text_input::zv3::client::__interfaces::*;
        #[allow(unused_imports)]
        use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3;
        wayland_scanner::generate_interfaces!("protocols/input-method-unstable-v2.xml");
    }
    use self::__interfaces::*;
    #[allow(unused_imports)]
    use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3;
    wayland_scanner::generate_client_code!("protocols/input-method-unstable-v2.xml");
}

use protocol::zwp_input_method_manager_v2 as im_manager;
use protocol::zwp_input_method_v2 as im;

/// 等合成器把 activate 与随后的 done 送到的上限。超过就当它没启用输入法。
const ACTIVATE_WAIT: Duration = Duration::from_millis(1000);
/// 前台判断的缓存时间：`niri msg` 是几毫秒的子进程，不值得每个字符都问。
const FOREGROUND_CACHE: Duration = Duration::from_secs(1);

struct State {
    active: bool,
    /// activate 之后合成器还要发一次 done，状态才算真正交接完。
    done: u32,
    activated_at_done: u32,
    serial: u32,
}

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore im_manager::ZwpInputMethodManagerV2);
wayland_client::delegate_noop!(State: ignore vk_manager::ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(State: ignore protocol::zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2);
wayland_client::delegate_noop!(State: ignore protocol::zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2);
impl Dispatch<im::ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &im::ZwpInputMethodV2,
        event: im::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            im::Event::Activate => {
                state.active = true;
                state.activated_at_done = state.done;
            }
            im::Event::Deactivate => state.active = false,
            im::Event::Done => state.done = state.done.wrapping_add(1),
            _ => {}
        }
    }
}

pub(super) struct InputMethod {
    queue: EventQueue<State>,
    state: State,
    method: im::ZwpInputMethodV2,
    /// 持有它只是协议要求；见模块文档。
    _keyboard: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
}

impl InputMethod {
    pub(super) fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env().context("连接 Wayland 失败")?;
        let (globals, queue): (GlobalList, EventQueue<State>) =
            registry_queue_init(&connection).context("枚举 Wayland 全局对象失败")?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).context("没有 Wayland seat")?;
        let manager: im_manager::ZwpInputMethodManagerV2 = globals
            .bind(&qh, 1..=1, ())
            .context("桌面没有提供 zwp_input_method_manager_v2")?;
        let vk_manager: vk_manager::ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("桌面没有提供 zwp_virtual_keyboard_manager_v1")?;
        let keyboard = vk_manager.create_virtual_keyboard(&seat, &qh, ());
        let method = manager.get_input_method(&seat, &qh, ());
        let mut this = Self {
            queue,
            state: State {
                active: false,
                done: 0,
                activated_at_done: 0,
                serial: 0,
            },
            method,
            _keyboard: keyboard,
        };
        this.queue.flush().context("注册输入法失败")?;
        Ok(this)
    }

    /// 收一下还没处理的事件，顺带知道输入法现在是不是接上了一个 text input。
    fn refresh(&mut self) {
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.queue.flush();
    }

    /// 输入法现在能不能提交。`false` 说明前台没启用 text-input（Chromium 默认如此），
    /// 或者合成器还没把 activate 与 done 送齐。
    pub(super) fn active(&mut self) -> bool {
        self.refresh();
        self.state.active && self.state.done > self.state.activated_at_done
    }

    /// 等合成器把输入法交接完（activate 到了，它之后的 done 也到了）。
    pub(super) fn wait_active(&mut self) -> bool {
        let deadline = Instant::now() + ACTIVATE_WAIT;
        loop {
            if self.active() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 让合成器把 `text` 交给焦点客户端。
    pub(super) fn text(&mut self, text: &str, sacrifice_first: bool) -> Result<()> {
        self.refresh();
        anyhow::ensure!(
            self.state.active && self.state.done > self.state.activated_at_done,
            "输入法还没有就绪"
        );
        // Chromium 系客户端会吞掉每次提交的第一个字符（GTK、终端不会）。垫一个
        // 零宽空格当牺牲品；它万一没被吞掉也不显示。
        let text = if sacrifice_first {
            format!("\u{200b}{text}")
        } else {
            text.to_owned()
        };
        self.method.commit_string(text);
        self.state.serial = self.state.serial.wrapping_add(1);
        self.method.commit(self.state.serial);
        self.queue.flush().context("提交输入法文本失败")?;
        Ok(())
    }
}

/// (判定时间, 前台是不是 Chromium 系)。
static FOREGROUND: std::sync::Mutex<Option<(Instant, bool)>> = std::sync::Mutex::new(None);

/// 前台窗口是不是 Chromium 系客户端。判断要走 niri 的 IPC；别的合成器、锁屏、
/// 或者查询失败都按"不是"处理，那只是不加零宽垫字符而已。
pub(super) fn chromium_foreground() -> bool {
    let Ok(mut cache) = FOREGROUND.lock() else {
        return false;
    };
    if let Some((when, value)) = *cache
        && when.elapsed() < FOREGROUND_CACHE
    {
        return value;
    }
    let value = focused_pid().is_some_and(chromium_like);
    *cache = Some((Instant::now(), value));
    value
}

fn focused_pid() -> Option<u32> {
    let output = Command::new("niri")
        .args(["msg", "-j", "focused-window"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let window: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    window
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

/// Chromium 系：可执行文件是那几个浏览器，或者进程里映着 libcef（Electron）。
/// Edge、Chrome 是静态链接的，映射里看不到 libcef，所以两种判据都要留着。
fn chromium_like(pid: u32) -> bool {
    const BROWSERS: [&str; 7] = [
        "chrome", "chromium", "msedge", "electron", "brave", "vivaldi", "opera",
    ];
    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap_or_default();
    let name = executable
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if BROWSERS.iter().any(|browser| name.contains(browser)) {
        return true;
    }
    std::fs::read(format!("/proc/{pid}/maps"))
        .map(|maps| String::from_utf8_lossy(&maps).contains("libcef"))
        .unwrap_or(false)
}
