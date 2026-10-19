//! Controlled-session input injection on Linux.
//!
//! Two backends sit behind one set of calls. In a Wayland session X Test only
//! reaches XWayland clients, so keys, buttons and motion go through a uinput
//! device there, as Sunshine does (it needs write access to `/dev/uinput`,
//! granted by the udev rule the packages ship). An Xorg session keeps X Test
//! alone: it needs no permission and positions the pointer absolutely, while a
//! uinput pointer there would only add libinput's acceleration and drift the
//! landing point.
//!
//! `OPENUUYC_INPUT` picks between them explicitly (`uinput`, `xtest`), and
//! any other value is an error rather than a silent choice: a session that
//! takes no input is easier to diagnose when the reason is on the screen.
mod input_method;
mod uinput;
mod virtual_keyboard;
mod xtest;

use crate::platform::Rotation;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;

pub(crate) use uinput::{Desktop, layout_bounds};

/// 控制器报来的一个触点：它的 id、屏幕上的比例位置和比例半径。
///
/// 平台层不认识 UU 的消息，所以输入引擎先把触点换算成比例；角度和压力在 evdev
/// 这边没有对应的轴，虚拟触摸屏只报位置和触点直径（见 `ABS_MT_TOUCH_MAJOR`）。
#[derive(Clone, Copy)]
pub(crate) struct Touch {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub radius: f32,
}

/// Lock keys whose state a controller can ask to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
    Caps,
    Num,
    Scroll,
}
impl Lock {
    pub fn from_virtual_key(key: u16) -> Option<Self> {
        match key {
            0x14 => Some(Self::Caps),
            0x90 => Some(Self::Num),
            0x91 => Some(Self::Scroll),
            _ => None,
        }
    }
}
/// Lock the session through logind, as Win+L does on Windows.
pub(crate) fn lock_session() -> Result<()> {
    let status = std::process::Command::new("loginctl")
        .arg("lock-session")
        .status()
        .context("无法调用 loginctl 锁定会话")?;
    ensure!(status.success(), "锁定会话失败：{status}");
    Ok(())
}

/// Open the desktop's system monitor, the counterpart of Task Manager.
pub(crate) fn system_monitor() -> Result<()> {
    for program in [
        "gnome-system-monitor",
        "plasma-systemmonitor",
        "ksysguard",
        "mate-system-monitor",
        "xfce4-taskmanager",
    ] {
        match std::process::Command::new(program)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("没有找到系统监视器程序")
}

/// The injector for this session, whichever backend it turned out to be.
pub(crate) enum Injector {
    XTest(xtest::Injector),
    UInput(uinput::Injector),
}

impl Injector {
    /// Opens the best backend this session allows. `uinput` is tried first
    /// because it is the only one that works on Wayland and at a locked
    /// screen; X Test is the fallback for a session whose `/dev/uinput` is not
    /// writable, which is every machine without the udev rule installed.
    pub fn open() -> Result<Self> {
        let chosen = std::env::var("OPENUUYC_INPUT").unwrap_or_else(|_| "auto".into());
        match chosen.trim() {
            "uinput" => Ok(Self::UInput(uinput::Injector::open()?)),
            "xtest" => Ok(Self::XTest(xtest::Injector::open()?)),
            "auto" => {
                // Xorg 会话里 XTest 本来就把指针绝对定位，而且不需要任何权限；
                // 换成 uinput 只会让位移经 libinput 加速，落点飘移。所以非 Wayland
                // 会话直接走 XTest，uinput 只在 Wayland 建。
                if !super::capture::wayland_session() {
                    return Ok(Self::XTest(xtest::Injector::open()?));
                }
                match uinput::Injector::open() {
                    Ok(injector) => Ok(Self::UInput(injector)),
                    Err(error) => {
                        tracing::info!(%error, "uinput unavailable, falling back to XTest");
                        Ok(Self::XTest(xtest::Injector::open()?))
                    }
                }
            }
            other => {
                bail!("OPENUUYC_INPUT 中有未知的注入方式：{other}（可选 uinput、xtest、auto）")
            }
        }
    }

    /// Whether there is a virtual absolute pointer. Only with one can
    /// absolute positions reach the pointer through the compositor's own
    /// layout; X Test is resolved by X, in X's rectangles.
    pub fn has_absolute_pointer(&self) -> bool {
        matches!(self, Self::UInput(injector) if injector.has_absolute_pointer())
    }

    /// 有没有虚拟多点触摸屏。触摸选路先问它，好让选出来的路与日志属实（见
    /// `features::host::input::engine_linux` 的 `choose_route`）。
    pub fn has_touchscreen(&self) -> bool {
        match self {
            Self::XTest(_) => false,
            Self::UInput(injector) => injector.has_touchscreen(),
        }
    }

    /// What the diagnostics page calls this session's injector.
    pub fn label(&self) -> &'static str {
        match self {
            Self::XTest(injector) => injector.label(),
            Self::UInput(injector) => injector.label(),
        }
    }

    pub fn key(&mut self, key: u16, down: bool) -> Result<()> {
        match self {
            Self::XTest(injector) => injector.key(key, down),
            Self::UInput(injector) => injector.key(key, down),
        }
    }

    pub fn button(&mut self, button: u8, down: bool) -> Result<()> {
        match self {
            Self::XTest(injector) => injector.button(button, down),
            Self::UInput(injector) => injector.button(button, down),
        }
    }

    pub fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        match self {
            Self::XTest(injector) => injector.move_to(x, y),
            Self::UInput(injector) => injector.move_to(x, y),
        }
    }

    /// 移到合成器全局空间里的一点，由合成器按 `bounds`（它全部输出的外接矩形）
    /// 解算。只有 uinput 后端有这台设备；XTest 由 X 在它自己的矩形里解算。
    pub fn aim(&mut self, x: i32, y: i32, bounds: Desktop) -> Result<()> {
        match self {
            Self::XTest(_) => bail!("XTest 后端不按合成器布局定位"),
            Self::UInput(injector) => injector.aim(x, y, bounds),
        }
    }

    /// 放弃还没补写完的位置（输入被收回或释放时，不再替对端移动指针）。
    pub fn forget_aim(&mut self) {
        match self {
            Self::XTest(_) => {}
            Self::UInput(injector) => injector.forget_aim(),
        }
    }

    pub fn move_by(&mut self, dx: i32, dy: i32) -> Result<()> {
        match self {
            Self::XTest(injector) => injector.move_by(dx, dy),
            Self::UInput(injector) => injector.move_by(dx, dy),
        }
    }

    pub fn wheel_notch(&mut self, horizontal: bool, positive: bool) -> Result<()> {
        match self {
            Self::XTest(injector) => injector.wheel_notch(horizontal, positive),
            Self::UInput(injector) => injector.wheel_notch(horizontal, positive),
        }
    }

    /// 真实的多点触摸，由合成器派发给应用。返回 false 表示这个后端没有触摸屏，
    /// 调用方要退回单指模拟。
    pub fn touch(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<bool> {
        match self {
            // X Test 里没有触摸设备，也没有地方放一个。
            Self::XTest(_) => Ok(false),
            Self::UInput(injector) => injector.touch(kind, points, rotation),
        }
    }

    /// 抬起所有触点，断开会话和换屏时要走一遍。
    pub fn release_touch(&mut self) -> Result<()> {
        match self {
            Self::XTest(_) => Ok(()),
            Self::UInput(injector) => injector.release_touch(),
        }
    }

    pub fn toggled(&mut self, lock: Lock) -> Result<bool> {
        match self {
            Self::XTest(injector) => injector.toggled(lock),
            Self::UInput(injector) => injector.toggled(lock),
        }
    }

    pub fn pointer(&mut self) -> Result<(i32, i32)> {
        match self {
            Self::XTest(injector) => injector.pointer(),
            Self::UInput(injector) => injector.pointer(),
        }
    }

    /// `held` is the keys the controller still has down. X Test can read them
    /// from its own keyboard state; uinput cannot, so it needs telling.
    pub fn text(
        &mut self,
        text: &str,
        held: &BTreeSet<u16>,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        match self {
            Self::XTest(injector) => injector.text(text, permitted),
            Self::UInput(injector) => injector.text(text, held, permitted),
        }
    }

    pub fn foreground(&mut self) -> Option<u32> {
        match self {
            Self::XTest(injector) => injector.foreground(),
            Self::UInput(injector) => injector.foreground(),
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        match self {
            Self::XTest(injector) => injector.flush(),
            Self::UInput(injector) => injector.flush(),
        }
    }
}

/// Windows virtual-key code to evdev key code, by physical position (US
/// names). The same table as the viewer's winit mapping, read backwards.
pub(crate) fn evdev(key: u16) -> Option<u16> {
    Some(match key {
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0d => 28,  // Enter
        0x10 => 42,  // Shift
        0x11 => 29,  // Control
        0x12 => 56,  // Alt
        0x13 => 119, // Pause
        0x14 => 58,  // Caps Lock
        0x15 => 122, // Kana / Hangul
        0x19 => 123, // Hanja
        0x1b => 1,   // Escape
        0x1c => 92,  // Convert (Henkan)
        0x1d => 94,  // NonConvert (Muhenkan)
        0x20 => 57,  // Space
        0x21 => 104, // Page Up
        0x22 => 109, // Page Down
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2c => 99,  // Print Screen
        0x2d => 110, // Insert
        0x2e => 111, // Delete
        0x2f => 138, // Help
        0x30 => 11,
        0x31..=0x39 => key - 0x31 + 2,
        0x41 => 30,
        0x42 => 48,
        0x43 => 46,
        0x44 => 32,
        0x45 => 18,
        0x46 => 33,
        0x47 => 34,
        0x48 => 35,
        0x49 => 23,
        0x4a => 36,
        0x4b => 37,
        0x4c => 38,
        0x4d => 50,
        0x4e => 49,
        0x4f => 24,
        0x50 => 25,
        0x51 => 16,
        0x52 => 19,
        0x53 => 31,
        0x54 => 20,
        0x55 => 22,
        0x56 => 47,
        0x57 => 17,
        0x58 => 45,
        0x59 => 21,
        0x5a => 44,
        0x5b => 125, // Left Windows / Super
        0x5c => 126, // Right Windows / Super
        0x5d => 127, // Menu
        0x5f => 142, // Sleep
        0x60 => 82,  // Numpad 0
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73,                      // Numpad 9
        0x6a => 55,                      // Numpad *
        0x6b => 78,                      // Numpad +
        0x6c => 121,                     // Numpad separator
        0x6d => 74,                      // Numpad -
        0x6e => 83,                      // Numpad .
        0x6f => 98,                      // Numpad /
        0x70..=0x79 => key - 0x70 + 59,  // F1-F10
        0x7a => 87,                      // F11
        0x7b => 88,                      // F12
        0x7c..=0x87 => key - 0x7c + 183, // F13-F24
        0x90 => 69,                      // Num Lock
        0x91 => 70,                      // Scroll Lock
        0xa0 => 42,                      // Left Shift
        0xa1 => 54,                      // Right Shift
        0xa2 => 29,                      // Left Control
        0xa3 => 97,                      // Right Control
        0xa4 => 56,                      // Left Alt
        0xa5 => 100,                     // Right Alt
        0xa6 => 158,                     // Browser Back
        0xa7 => 159,                     // Browser Forward
        0xa8 => 173,                     // Browser Refresh
        0xa9 => 128,                     // Browser Stop
        0xaa => 217,                     // Browser Search
        0xab => 156,                     // Browser Favorites
        0xac => 172,                     // Browser Home
        0xad => 113,                     // Volume Mute
        0xae => 114,                     // Volume Down
        0xaf => 115,                     // Volume Up
        0xb0 => 163,                     // Next Track
        0xb1 => 165,                     // Previous Track
        0xb2 => 166,                     // Stop
        0xb3 => 164,                     // Play/Pause
        0xb4 => 155,                     // Mail
        0xb5 => 226,                     // Media Select
        0xb6 => 157,                     // Launch App 1 (Computer)
        0xb7 => 140,                     // Launch App 2 (Calculator)
        0xba => 39,                      // ;
        0xbb => 13,                      // =
        0xbc => 51,                      // ,
        0xbd => 12,                      // -
        0xbe => 52,                      // .
        0xbf => 53,                      // /
        0xc0 => 41,                      // `
        0xc1 => 89,                      // Ro
        0xc2 => 121,                     // ABNT C2 / Numpad ,
        0xdb => 26,                      // [
        0xdc => 43,                      // \
        0xdd => 27,                      // ]
        0xde => 40,                      // '
        0xe2 => 86,                      // 102nd key
        0xff => 124,                     // Yen
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::evdev;

    #[test]
    fn letters_and_digits_follow_the_us_rows() {
        assert_eq!(evdev(b'Q'.into()), Some(16));
        assert_eq!(evdev(b'A'.into()), Some(30));
        assert_eq!(evdev(b'Z'.into()), Some(44));
        assert_eq!(evdev(b'1'.into()), Some(2));
        assert_eq!(evdev(b'9'.into()), Some(10));
        assert_eq!(evdev(0x70), Some(59));
        assert_eq!(evdev(0x79), Some(68));
        assert_eq!(evdev(0x7c), Some(183));
    }
}

#[cfg(test)]
mod session_tests {
    /// Needs a live session, and for the uinput backend a writable
    /// `/dev/uinput`: `cargo test -- --ignored`.
    ///
    /// Moves the pointer and puts it back, and should leave no key down.
    #[test]
    #[ignore]
    fn pointer_moves_where_asked() {
        let mut injector = super::Injector::open().unwrap();
        println!("backend: {}", injector.label());
        let before = injector.pointer().unwrap();
        injector.move_to(123, 234).unwrap();
        injector.flush().unwrap();
        assert_eq!(injector.pointer().unwrap(), (123, 234));
        injector.move_by(10, -4).unwrap();
        assert_eq!(injector.pointer().unwrap(), (133, 230));
        println!(
            "caps lock on: {}",
            injector.toggled(super::Lock::Caps).unwrap()
        );
        println!("foreground pid: {:?}", injector.foreground());
        injector.move_to(before.0, before.1).unwrap();
        injector.flush().unwrap();
    }
    /// 需要真实会话和可写的 `/dev/uinput`：`cargo test -- --ignored`。
    ///
    /// 在屏幕右下角按下再抬起一次，验证这块触摸屏真能建起来、内核收下这些帧。
    /// 触点会落到那个位置的窗口上，所以跑的时候右下角最好什么都没有。
    #[test]
    #[ignore = "injects a real touch into the live session"]
    fn a_touchscreen_takes_a_contact_and_lets_it_go() {
        let mut injector = super::Injector::open().unwrap();
        let point = super::Touch {
            id: 1,
            x: 0.999,
            y: 0.999,
            radius: 0.01,
        };
        let rotation = crate::platform::Rotation::Normal;
        if !injector.touch(2, &[point], rotation).unwrap() {
            println!("这个会话没有触摸屏设备（后端：{}）", injector.label());
            return;
        }
        injector.release_touch().unwrap();
        injector.flush().unwrap();
        println!("触摸屏后端：{}", injector.label());
    }
    /// Types OPENUUYC_TYPE_TEXT into whatever has focus.
    #[test]
    #[ignore = "types into the focused window of the live session"]
    fn types_text() {
        let text = std::env::var("OPENUUYC_TYPE_TEXT").unwrap();
        let mut injector = super::Injector::open().unwrap();
        println!("backend: {}", injector.label());
        injector.text(&text, &Default::default(), || true).unwrap();
    }
}
