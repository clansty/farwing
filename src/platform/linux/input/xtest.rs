//! Input injection through the X Test extension, which needs no device
//! permissions inside the user's own Xorg session.
//!
//! Keys travel by physical position: a Windows virtual-key code is mapped to
//! its evdev scan position and from there to the X keycode (evdev + 8 under
//! the evdev and libinput drivers), so the local layout decides the character,
//! as it would for a keyboard plugged into this machine.
use super::{Lock, evdev};
use anyhow::{Context, Result, bail, ensure};
use std::time::Duration;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Keycode, Keysym, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;
const NO_SYMBOL: Keysym = 0;
/// The pause after each typed character.
const TEXT_PACE: Duration = Duration::from_millis(3);
/// XWayland 注册的扩展名，用来认出这台 X 是不是本地 Wayland 会话的那一份。
const XWAYLAND: &[u8] = b"XWAYLAND";
/// `XCB_POINTER_ROOT`：焦点跟着指针时 X 报的这个值，它不是某一块窗口。
const POINTER_ROOT: u32 = 1;

pub(super) struct Injector {
    connection: RustConnection,
    root: Window,
    min_keycode: Keycode,
    max_keycode: Keycode,
}

impl Injector {
    pub(super) fn label(&self) -> &'static str {
        "X11 XTest · 当前用户"
    }

    pub(super) fn open() -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
        ensure!(
            connection
                .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
                .is_some(),
            "X 服务器没有 XTEST 扩展，无法注入输入"
        );
        connection.xtest_get_version(2, 2)?.reply()?;
        let setup = connection.setup();
        let root = setup.roots[screen].root;
        let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);
        Ok(Self {
            connection,
            root,
            min_keycode,
            max_keycode,
        })
    }

    /// 这台 X 服务器是不是随本地 Wayland 会话跑的 XWayland。
    ///
    /// XWayland 注册了同名扩展；真正的 Xorg、以及 ssh 转发来的远端 X 都没有。
    /// 用它确认这台 X 和本地合成器属于同一套会话，才不会把文字发到别处的 X 上。
    pub(super) fn is_xwayland(&self) -> Result<bool> {
        Ok(self.connection.query_extension(XWAYLAND)?.reply()?.present)
    }

    /// 焦点现在是不是落在某个 X 客户端上。
    ///
    /// XWayland 在前台是原生 Wayland 窗口时，把自己的输入焦点设成 `None`（有时
    /// 是 `PointerRoot`）；有 X 窗口拿到焦点时才是那块窗口。所以问一次 X 就能分清
    /// 前台属于谁，不必依赖某个合成器的 IPC。
    pub(super) fn x_client_focused(&self) -> Result<bool> {
        let focus = self.connection.get_input_focus()?.reply()?.focus;
        Ok(focus != x11rb::NONE && focus != POINTER_ROOT)
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?
            .check()
            .context("注入输入事件失败")?;
        Ok(())
    }

    /// A key by Windows virtual-key code.
    pub(super) fn key(&self, key: u16, down: bool) -> Result<()> {
        let code = evdev(key).with_context(|| format!("不支持的按键 {key:#04x}"))?;
        let keycode = u8::try_from(code + 8).context("按键超出 X11 键码范围")?;
        ensure!(
            (self.min_keycode..=self.max_keycode).contains(&keycode),
            "按键超出 X 服务器键码范围"
        );
        self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
    }

    /// A button by X button number (1 left, 2 middle, 3 right, 8/9 side).
    pub(super) fn button(&self, button: u8, down: bool) -> Result<()> {
        self.fake(
            if down { BUTTON_PRESS } else { BUTTON_RELEASE },
            button,
            0,
            0,
        )
    }

    /// Move to a root-window position.
    pub(super) fn move_to(&self, x: i32, y: i32) -> Result<()> {
        self.fake(
            MOTION_NOTIFY,
            0,
            i16::try_from(x).context("指针坐标超出 X11 范围")?,
            i16::try_from(y).context("指针坐标超出 X11 范围")?,
        )
    }

    /// Move by a pixel delta from wherever the pointer is.
    pub(super) fn move_by(&self, mut dx: i32, mut dy: i32) -> Result<()> {
        while dx != 0 || dy != 0 {
            let x = dx.clamp(i16::MIN.into(), i16::MAX.into());
            let y = dy.clamp(i16::MIN.into(), i16::MAX.into());
            self.fake(MOTION_NOTIFY, 1, x as i16, y as i16)?;
            dx -= x;
            dy -= y;
        }
        Ok(())
    }

    /// One wheel notch: X delivers wheels as buttons 4-7.
    pub(super) fn wheel_notch(&self, horizontal: bool, positive: bool) -> Result<()> {
        // Windows: positive vertical is away from the user (up), positive
        // horizontal is to the right.
        let button = match (horizontal, positive) {
            (false, true) => 4,
            (false, false) => 5,
            (true, false) => 6,
            (true, true) => 7,
        };
        self.button(button, true)?;
        self.button(button, false)
    }

    pub(super) fn toggled(&self, lock: Lock) -> Result<bool> {
        let leds = self.connection.get_keyboard_control()?.reply()?.led_mask;
        // Core LEDs 1-3 are Caps, Num and Scroll Lock on every X keyboard map.
        Ok(leds
            & match lock {
                Lock::Caps => 1,
                Lock::Num => 2,
                Lock::Scroll => 4,
            }
            != 0)
    }

    pub(super) fn pointer(&self) -> Result<(i32, i32)> {
        let reply = self.connection.query_pointer(self.root)?.reply()?;
        Ok((reply.root_x.into(), reply.root_y.into()))
    }

    /// Type text independently of the key layout, the way Windows' Unicode
    /// keyboard input does: each character's keysym is found in the current
    /// map, or bound for the moment to a spare keycode.
    ///
    /// Unicode input on Windows ignores the keys being held, so modifiers the
    /// controller still holds (the Ctrl of the Ctrl+V that asked for a paste)
    /// are released while typing and pressed again afterwards, and Caps Lock
    /// does not change the case of a letter. A Windows line break (CR LF)
    /// is one Enter.
    pub(super) fn text(&self, text: &str, permitted: impl Fn() -> bool) -> Result<()> {
        let count = self.max_keycode - self.min_keycode + 1;
        let map = self
            .connection
            .get_keyboard_mapping(self.min_keycode, count)?
            .reply()?;
        let per = usize::from(map.keysyms_per_keycode);
        ensure!(per > 0, "键盘映射无效");
        let find = |sym: Keysym| -> Option<(Keycode, bool)> {
            map.keysyms.chunks(per).enumerate().find_map(|(i, syms)| {
                let code = self.min_keycode + i as u8;
                if syms.first() == Some(&sym) {
                    Some((code, false))
                } else if syms.get(1) == Some(&sym) {
                    Some((code, true))
                } else {
                    None
                }
            })
        };
        let spare = map
            .keysyms
            .chunks(per)
            .rposition(|syms| syms.iter().all(|s| *s == NO_SYMBOL))
            .map(|i| self.min_keycode + i as u8);
        let shift = evdev(0xa0)
            .map(|code| (code + 8) as u8)
            .context("缺少 Shift 键")?;
        let caps = self.toggled(Lock::Caps).unwrap_or(false);
        let held = self.held_modifiers()?;
        for code in &held {
            self.fake(KEY_RELEASE, *code, 0, 0)?;
        }
        // Caps Lock changes the case of letters, including those bound to the
        // spare keycode, so it is off while typing.
        if caps {
            self.tap(0x14)?;
        }
        let mut bound = false;
        let result = (|| -> Result<()> {
            let mut previous = None;
            for c in text.chars() {
                ensure!(permitted(), "文字输入已取消");
                let after_cr = previous == Some('\r');
                previous = Some(c);
                if c == '\n' && after_cr {
                    continue;
                }
                let sym = keysym(c);
                let (code, shifted) = match find(sym) {
                    Some(found) => found,
                    None => {
                        let spare = spare.context("没有空闲键码可用于输入该字符")?;
                        let syms = vec![sym; per];
                        self.connection
                            .change_keyboard_mapping(1, spare, per as u8, &syms)?
                            .check()?;
                        bound = true;
                        // Clients refresh their keymap on MappingNotify; give
                        // the focused one a moment before the key arrives.
                        self.connection.get_input_focus()?.reply()?;
                        std::thread::sleep(Duration::from_millis(12));
                        (spare, false)
                    }
                };
                if shifted {
                    self.fake(KEY_PRESS, shift, 0, 0)?;
                }
                let typed = self
                    .fake(KEY_PRESS, code, 0, 0)
                    .and_then(|()| self.fake(KEY_RELEASE, code, 0, 0));
                if shifted {
                    self.fake(KEY_RELEASE, shift, 0, 0)?;
                }
                typed?;
                // Input methods (IBus) pass each key through another process
                // and drop keys that arrive faster than they are handled.
                std::thread::sleep(if bound {
                    Duration::from_millis(12)
                } else {
                    TEXT_PACE
                });
            }
            Ok(())
        })();
        if bound && let Some(spare) = spare {
            let empty = vec![NO_SYMBOL; per];
            let _ = self
                .connection
                .change_keyboard_mapping(1, spare, per as u8, &empty);
            let _ = self.connection.flush();
        }
        if caps {
            let _ = self.tap(0x14);
        }
        for code in &held {
            let _ = self.fake(KEY_PRESS, *code, 0, 0);
        }
        result
    }

    /// Press and release a key by Windows virtual-key code.
    fn tap(&self, key: u16) -> Result<()> {
        self.key(key, true)?;
        self.key(key, false)
    }

    /// Modifier keys other than the lock keys that are down right now.
    fn held_modifiers(&self) -> Result<Vec<Keycode>> {
        let mapping = self.connection.get_modifier_mapping()?.reply()?;
        let keys = self.connection.query_keymap()?.reply()?.keys;
        let per = usize::from(mapping.keycodes_per_modifier()).max(1);
        let mut held = Vec::new();
        for (index, codes) in mapping.keycodes.chunks(per).enumerate() {
            // Lock (index 1) is a toggle: pressing it again would flip it.
            if index == 1 {
                continue;
            }
            for &code in codes {
                if code != 0
                    && keys[usize::from(code / 8)] & (1 << (code % 8)) != 0
                    && !held.contains(&code)
                {
                    held.push(code);
                }
            }
        }
        Ok(held)
    }

    /// The process that owns the active window, as the window manager reports.
    pub(super) fn foreground(&self) -> Option<u32> {
        let atom = |name: &str| {
            self.connection
                .intern_atom(true, name.as_bytes())
                .ok()?
                .reply()
                .ok()
                .map(|r| r.atom)
                .filter(|atom| *atom != 0)
        };
        let active = atom("_NET_ACTIVE_WINDOW")?;
        let pid = atom("_NET_WM_PID")?;
        let window = self
            .connection
            .get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()?;
        self.connection
            .get_property(false, window, pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
    }

    pub(super) fn flush(&self) -> Result<()> {
        self.connection.flush()?;
        Ok(())
    }
}

fn keysym(c: char) -> Keysym {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        // Latin-1 keysyms equal their code points; everything else uses the
        // Unicode keysym range.
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 | c as u32,
    }
}
