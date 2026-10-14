//! Input injection through `/dev/uinput`: virtual devices the kernel hands to
//! whatever session is running, Xorg or Wayland alike.
//!
//! This is the backend that works where X Test does not — a Wayland session,
//! and a locked screen, which X clients cannot reach at all. It costs one
//! permission: write access to `/dev/uinput`, granted by the udev rule this
//! project ships.
//!
//! Two devices are created, because the kernel and libinput read their
//! capabilities quite differently:
//!
//! - a keyboard, which is every key code there is plus the three lock LEDs;
//! - a pointer, relative, which is what a mouse is;
//! - and, once the desktop geometry is known, an absolute pointer. Relative
//!   motion cannot express "go to this pixel", and the default mouse mode
//!   asks for exactly that.
//!
//! Typing is the one place this backend is weaker than X Test: there is no
//! keymap to extend, so a character can only be typed if the layout the user
//! configured produces it. The table here describes the US layout.
//!
//! On a Wayland session that gap is closed by `zwp_virtual_keyboard_v1`
//! (`virtual_keyboard`), which carries a keymap of our own making. This table
//! stays as the fallback for sessions whose compositor does not serve it.
use super::{Lock, evdev, virtual_keyboard};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

const UINPUT_MAX_NAME_SIZE: usize = 80;
/// The pause after each typed character, as in the X Test backend.
const TEXT_PACE: Duration = Duration::from_millis(3);

// --- kernel ABI (linux/uinput.h, linux/input-event-codes.h) -----------------

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const EV_LED: u16 = 17;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const LED_NUML: u16 = 0;
const LED_CAPSL: u16 = 1;
const LED_SCROLLL: u16 = 2;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
/// The highest key code the kernel defines, `KEY_MAX`.
const KEY_MAX: u16 = 0x2ff;
const INPUT_PROP_POINTER: u32 = 0;
const INPUT_PROP_DIRECT: u32 = 1;
const BUS_USB: u16 = 0x03;

const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;

const UI_SET_LEDBIT: libc::c_ulong = 0x4004_5569;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556E;
const UI_DEV_CREATE: libc::c_ulong = 0x0000_5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x0000_5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405C_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401C_5504;

#[repr(C)]
#[derive(Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ff_effect_id: u32,
}

#[repr(C)]
#[derive(Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Default)]
struct UinputAbsSetup {
    code: u16,
    pad: u16,
    absinfo: InputAbsInfo,
}

#[repr(C)]
#[derive(Default)]
struct InputEvent {
    seconds: i64,
    microseconds: i64,
    type_: u16,
    code: u16,
    value: i32,
}

/// An ioctl whose argument is a pointer to a `#[repr(C)]` struct.
fn ioctl<T>(fd: i32, request: libc::c_ulong, argument: &mut T) -> std::io::Result<()> {
    // Safe because every caller passes a struct matching the request.
    if unsafe { libc::ioctl(fd, request, argument as *mut T) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// An ioctl whose argument is the value itself. The uinput `UI_SET_*` bits are
/// declared `_IOW(..., int)`, which is only how their numbers are spelled: the
/// kernel reads the number straight out of the argument, and a pointer there is
/// an out-of-range bit that comes back as `EINVAL`.
fn ioctl_value(fd: i32, request: libc::c_ulong, value: u32) -> std::io::Result<()> {
    if unsafe { libc::ioctl(fd, request, libc::c_ulong::from(value)) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// One virtual device, destroyed when the last reference goes.
struct Device {
    file: std::fs::File,
}

impl Device {
    fn create(name: &str, capabilities: impl FnOnce(&Self) -> Result<()>) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uinput")
            .context("无法打开 /dev/uinput（需要 udev 规则授予的写权限）")?;
        let device = Self { file };
        capabilities(&device)?;
        let mut setup = UinputSetup {
            id: InputId {
                bustype: BUS_USB,
                vendor: 0x1d6b,
                product: 0x0104,
                version: 1,
            },
            name: [0; UINPUT_MAX_NAME_SIZE],
            ff_effect_id: 0,
        };
        let bytes = name.as_bytes();
        ensure!(bytes.len() < UINPUT_MAX_NAME_SIZE, "设备名过长");
        setup.name[..bytes.len()].copy_from_slice(bytes);
        ioctl(device.raw(), UI_DEV_SETUP, &mut setup).context("UI_DEV_SETUP 失败")?;
        // Safe because the argument is unused for a `_IO` request.
        if unsafe { libc::ioctl(device.raw(), UI_DEV_CREATE) } < 0 {
            bail!("创建虚拟输入设备失败：{}", std::io::Error::last_os_error());
        }
        Ok(device)
    }

    fn raw(&self) -> i32 {
        self.file.as_raw_fd()
    }

    fn event_type(&self, type_: u16) -> Result<()> {
        ioctl_value(self.raw(), UI_SET_EVBIT, u32::from(type_)).context("设置事件类型失败")
    }

    fn key(&self, code: u16) -> Result<()> {
        ioctl_value(self.raw(), UI_SET_KEYBIT, u32::from(code)).context("设置按键能力失败")
    }

    fn relative(&self, axis: u16) -> Result<()> {
        ioctl_value(self.raw(), UI_SET_RELBIT, u32::from(axis)).context("设置相对轴失败")
    }

    fn led(&self, led: u16) -> Result<()> {
        ioctl_value(self.raw(), UI_SET_LEDBIT, u32::from(led)).context("设置指示灯失败")
    }

    fn property(&self, property: u32) -> Result<()> {
        ioctl_value(self.raw(), UI_SET_PROPBIT, property).context("设置设备属性失败")
    }

    fn absolute(&self, axis: u16, maximum: i32) -> Result<()> {
        let mut setup = UinputAbsSetup {
            code: axis,
            absinfo: InputAbsInfo {
                maximum,
                ..Default::default()
            },
            ..Default::default()
        };
        ioctl_value(self.raw(), UI_SET_ABSBIT, u32::from(axis)).context("设置绝对轴失败")?;
        ioctl(self.raw(), UI_ABS_SETUP, &mut setup).context("设置绝对轴范围失败")
    }

    /// Writes a run of events and closes it with a report, which is what makes
    /// the kernel deliver them together.
    fn send(&self, events: &[(u16, u16, i32)]) -> Result<()> {
        let mut buffer: Vec<InputEvent> = events
            .iter()
            .map(|(type_, code, value)| InputEvent {
                type_: *type_,
                code: *code,
                value: *value,
                ..Default::default()
            })
            .collect();
        buffer.push(InputEvent {
            type_: EV_SYN,
            code: SYN_REPORT,
            ..Default::default()
        });
        let bytes = unsafe {
            // Safe because `InputEvent` is `#[repr(C)]` and has no padding
            // beyond its fields.
            std::slice::from_raw_parts(
                buffer.as_ptr().cast::<u8>(),
                std::mem::size_of_val(buffer.as_slice()),
            )
        };
        let written = unsafe { libc::write(self.raw(), bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            bail!("写入输入事件失败：{}", std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Safe because the argument is unused for a `_IO` request.
        unsafe { libc::ioctl(self.raw(), UI_DEV_DESTROY) };
    }
}

// --- the injector -----------------------------------------------------------

/// The desktop absolute coordinates are resolved against.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Desktop {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

pub(super) struct Injector {
    keyboard: Device,
    relative: Device,
    /// The absolute pointer spans the whole of whatever desk the compositor
    /// lays out, and its ranges are fixed when the device is created, so it
    /// only appears once the desktop size is known.
    absolute: Option<Device>,
    desktop: Option<Desktop>,
    /// When the absolute device was made, and the position to keep writing
    /// until the compositor has certainly opened it.
    made: Option<std::time::Instant>,
    repeat: Option<Repeat>,
    /// Where this backend last left the pointer. The kernel cannot be asked.
    position: (i32, i32),
    /// The lock keys, as a mask of the LEDs they drive.
    leds: u32,
    /// Text with a keymap of our own, where the compositor offers it.
    virtual_keyboard: Option<virtual_keyboard::Keyboard>,
}

impl Injector {
    pub(super) fn open() -> Result<Self> {
        ensure!(
            std::path::Path::new("/dev/uinput").exists(),
            "/dev/uinput 不存在（内核缺少 uinput 模块）"
        );
        let keyboard = Device::create("OpenUUYC 虚拟键盘", |device| {
            device.event_type(EV_KEY)?;
            for code in 1..=KEY_MAX {
                device.key(code)?;
            }
            device.event_type(EV_LED)?;
            for led in [LED_NUML, LED_CAPSL, LED_SCROLLL] {
                device.led(led)?;
            }
            Ok(())
        })
        .context("创建虚拟键盘失败")?;
        let relative = Device::create("OpenUUYC 虚拟鼠标", |device| {
            device.event_type(EV_KEY)?;
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                device.key(button)?;
            }
            device.event_type(EV_REL)?;
            for axis in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                device.relative(axis)?;
            }
            device.property(INPUT_PROP_DIRECT)
        })
        .context("创建虚拟鼠标失败")?;
        // Nothing here is Wayland only: an Xorg session keeps typing through
        // X Test, where the spare-keycode trick already covers any character.
        let virtual_keyboard = match virtual_keyboard::Keyboard::connect() {
            Ok(keyboard) => {
                tracing::info!("文字输入走 zwp_virtual_keyboard（可输入任意字符）");
                Some(keyboard)
            }
            Err(error) => {
                tracing::debug!(%error, "没有虚拟键盘协议，文字输入退回本地布局");
                None
            }
        };
        Ok(Self {
            keyboard,
            relative,
            absolute: None,
            desktop: None,
            made: None,
            repeat: None,
            position: (0, 0),
            leds: 0,
            virtual_keyboard,
        })
    }

    pub(super) fn label(&self) -> &'static str {
        "uinput · 系统级虚拟输入"
    }

    /// Makes the absolute device early, from the desk the compositor itself
    /// describes, so that it has been open for a while before anything aims at
    /// it. A compositor opens a device this process has just made in its own
    /// time -- hundreds of milliseconds -- and evdev hands a reader only what
    /// arrives after it opens, so a position written in that window reaches
    /// nobody. The rounds the session later reports replace this desk if it is
    /// not the same one.
    pub(super) fn prime(&mut self) -> Result<()> {
        let Some(desk) = compositor_desktop() else {
            return Ok(());
        };
        self.set_geometry(desk)
    }

    /// Records the desktop the absolute pointer spans and creates that device
    /// the first time there is a size to give it.
    pub(super) fn set_geometry(&mut self, desktop: Desktop) -> Result<()> {
        if self.desktop == Some(desktop) {
            return Ok(());
        }
        self.desktop = Some(desktop);
        // The ranges are what the compositor divides the desktop by, so a
        // desktop of a different size needs a device that says so.
        self.absolute = None;
        self.made = None;
        self.repeat = None;
        if desktop.width == 0 || desktop.height == 0 {
            return Ok(());
        }
        let width = desktop.width as i32 - 1;
        let height = desktop.height as i32 - 1;
        let device = Device::create("OpenUUYC 虚拟绝对鼠标", |device| {
            device.event_type(EV_KEY)?;
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                device.key(button)?;
            }
            device.event_type(EV_ABS)?;
            device.absolute(ABS_X, width)?;
            device.absolute(ABS_Y, height)?;
            // A pointer, not a direct device: `INPUT_PROP_DIRECT` tells a
            // compositor the coordinates map onto a single output, and these
            // are a fraction of the whole desk.
            device.property(INPUT_PROP_POINTER)
        })
        .context("创建虚拟绝对鼠标失败")?;
        self.absolute = Some(device);
        self.made = Some(std::time::Instant::now());
        Ok(())
    }

    pub(super) fn key(&mut self, key: u16, down: bool) -> Result<()> {
        let code = evdev(key).with_context(|| format!("不支持的按键 {key:#04x}"))?;
        if down && let Some(lock) = Lock::from_virtual_key(key) {
            self.leds ^= led_bit(lock);
        }
        self.keyboard.send(&[(EV_KEY, code, i32::from(down))])
    }

    /// A button by X button number (1 left, 2 middle, 3 right, 8/9 side).
    pub(super) fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let code = match button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            8 => BTN_SIDE,
            9 => BTN_EXTRA,
            other => bail!("不支持的鼠标按键 {other}"),
        };
        self.relative.send(&[(EV_KEY, code, i32::from(down))])
    }

    pub(super) fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let (target_x, target_y) = match self.absolute {
            Some(_) => (x, y),
            None => {
                // Without the absolute device the move has to be a delta from
                // wherever this backend believes the pointer is.
                let (dx, dy) = (x - self.position.0, y - self.position.1);
                self.move_by(dx, dy)?;
                return Ok(());
            }
        };
        self.write_absolute(target_x, target_y)?;
        if let Some(made) = self.made.filter(|made| made.elapsed() < FRESH_FOR) {
            self.repeat = Some(Repeat {
                until: made + FRESH_FOR,
                at: std::time::Instant::now(),
                position: (target_x, target_y),
                offset: 0,
            });
        }
        Ok(())
    }

    /// Writes one absolute position, in the desk's own coordinates.
    fn write_absolute(&mut self, target_x: i32, target_y: i32) -> Result<()> {
        let Some(desktop) = self.desktop else {
            bail!("尚未取得桌面尺寸，无法绝对定位");
        };
        let device = self.absolute.as_ref().context("没有绝对指针")?;
        let local_x = (target_x - desktop.left).clamp(0, desktop.width as i32 - 1);
        let local_y = (target_y - desktop.top).clamp(0, desktop.height as i32 - 1);
        device.send(&[(EV_ABS, ABS_X, local_x), (EV_ABS, ABS_Y, local_y)])?;
        self.position = (target_x, target_y);
        Ok(())
    }

    pub(super) fn move_by(&mut self, mut dx: i32, mut dy: i32) -> Result<()> {
        self.position.0 = self.position.0.saturating_add(dx);
        self.position.1 = self.position.1.saturating_add(dy);
        while dx != 0 || dy != 0 {
            let step_x = dx.clamp(-32_767, 32_767);
            let step_y = dy.clamp(-32_767, 32_767);
            let mut events = Vec::with_capacity(2);
            if step_x != 0 {
                events.push((EV_REL, REL_X, step_x));
            }
            if step_y != 0 {
                events.push((EV_REL, REL_Y, step_y));
            }
            self.relative.send(&events)?;
            dx -= step_x;
            dy -= step_y;
        }
        Ok(())
    }

    /// One wheel notch: the kernel takes wheel movement as a relative axis.
    pub(super) fn wheel_notch(&mut self, horizontal: bool, positive: bool) -> Result<()> {
        // Windows: positive vertical is away from the user (up), positive
        // horizontal is to the right. Linux counts the same way.
        let (axis, value) = match (horizontal, positive) {
            (false, true) => (REL_WHEEL, 1),
            (false, false) => (REL_WHEEL, -1),
            (true, true) => (REL_HWHEEL, 1),
            (true, false) => (REL_HWHEEL, -1),
        };
        self.relative.send(&[(EV_REL, axis, value)])
    }

    /// The lock keys as this backend last left them.
    ///
    /// The kernel's LED state belongs to the event device a uinput device
    /// becomes, not to the uinput handle, so it cannot be read back here
    /// without finding and opening that device too. What was sent is
    /// remembered instead; a lock key the user presses on their own keyboard
    /// is not seen.
    pub(super) fn toggled(&mut self, lock: Lock) -> Result<bool> {
        Ok(self.leds & led_bit(lock) != 0)
    }

    /// Where this backend last put the pointer. The kernel has no way to ask
    /// the compositor, so a pointer moved by anything else is not seen.
    pub(super) fn pointer(&mut self) -> Result<(i32, i32)> {
        Ok(self.position)
    }

    /// Nothing here can name the focused program: a Wayland client is not told
    /// what has focus, and there is no window manager to ask.
    pub(super) fn foreground(&mut self) -> Option<u32> {
        None
    }

    /// Types text, with the keys the controller is holding lifted for the
    /// duration — a held Ctrl would otherwise turn every character into a
    /// control code.
    ///
    /// Two mappings can do it. The virtual keyboard carries a keymap of our
    /// own, so any character goes through; the local layout is the fallback,
    /// and can only produce what that layout has. A Windows line break (CR LF)
    /// is one Enter either way.
    pub(super) fn text(
        &mut self,
        text: &str,
        held: &BTreeSet<u16>,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        let lifted = self.lift(held)?;
        let typed = self
            .virtual_keyboard
            .as_mut()
            .map(|keyboard| keyboard.text(text, &permitted));
        let result = match typed {
            Some(Ok(())) => Ok(()),
            other => {
                if let Some(Err(error)) = other {
                    // Half a text may already be through; typing it again from
                    // the top beats dropping the rest of it.
                    tracing::warn!(%error, "虚拟键盘输入失败，改用本地布局");
                }
                self.type_locally(text, &permitted)
            }
        };
        for code in lifted {
            let _ = self.keyboard.send(&[(EV_KEY, code, 1)]);
        }
        result
    }

    /// Releases the keys the controller still holds, and reports them so they
    /// can be pressed again afterwards.
    fn lift(&mut self, held: &BTreeSet<u16>) -> Result<Vec<u16>> {
        let mut lifted = Vec::new();
        for key in held {
            if let Some(code) = evdev(*key) {
                self.keyboard.send(&[(EV_KEY, code, 0)])?;
                lifted.push(code);
            }
        }
        Ok(lifted)
    }

    /// Types text with the local layout: the compositor reads these key codes
    /// as if a keyboard had sent them, so only the characters that layout
    /// produces can be typed.
    fn type_locally(&mut self, text: &str, permitted: impl Fn() -> bool) -> Result<()> {
        ensure!(us_layout(), "本地键盘布局不是 us，uinput 无法按字符输入");
        let caps = self.toggled(Lock::Caps).unwrap_or(false);
        // Caps Lock changes the case of letters, so it is off while typing.
        if caps {
            let code = evdev(0x14).context("缺少 Caps Lock 键")?;
            self.keyboard
                .send(&[(EV_KEY, code, 1), (EV_KEY, code, 0)])?;
            let _ = self.toggled(Lock::Caps);
        }
        let result = (|| -> Result<()> {
            let shift = evdev(0xa0).context("缺少 Shift 键")?;
            let mut previous = None;
            for c in text.chars() {
                ensure!(permitted(), "文字输入已取消");
                let after_cr = previous == Some('\r');
                previous = Some(c);
                if c == '\n' && after_cr {
                    continue;
                }
                let (code, shifted) =
                    us_key(c).with_context(|| format!("本地 us 布局无法输入字符 {c:?}"))?;
                let mut events = Vec::with_capacity(4);
                if shifted {
                    events.push((EV_KEY, shift, 1));
                }
                events.push((EV_KEY, code, 1));
                events.push((EV_KEY, code, 0));
                if shifted {
                    events.push((EV_KEY, shift, 0));
                }
                self.keyboard.send(&events)?;
                // Input methods pass each key through another process and drop
                // keys that arrive faster than they are handled.
                std::thread::sleep(TEXT_PACE);
            }
            Ok(())
        })();
        if caps {
            let code = evdev(0x14).context("缺少 Caps Lock 键")?;
            let _ = self.keyboard.send(&[(EV_KEY, code, 1), (EV_KEY, code, 0)]);
        }
        result
    }

    /// Every event is written as it is made, so there is nothing buffered --
    /// except a position aimed at a device that may not have been opened yet,
    /// which goes out again until it certainly has been.
    pub(super) fn flush(&mut self) -> Result<()> {
        let Some(repeat) = &mut self.repeat else {
            return Ok(());
        };
        let now = std::time::Instant::now();
        if now >= repeat.until {
            let (x, y) = repeat.position;
            self.repeat = None;
            return self.write_absolute(x, y);
        }
        if now.duration_since(repeat.at) < REPEAT_EVERY {
            return Ok(());
        }
        repeat.at = now;
        repeat.offset ^= 1;
        let (x, y) = repeat.position;
        let offset = repeat.offset;
        self.write_absolute(x + offset, y)
    }
}

/// How long a device that has just been made is given before a position aimed
/// at it stops being written again. A compositor opens a device this process
/// has just created in its own time, and evdev hands a reader only what arrives
/// after it opens, so a position written in that window reaches nobody.
const FRESH_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

/// How often that position is written again while the window lasts.
const REPEAT_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// A position aimed at a device the compositor may not have opened yet.
///
/// The repeat cannot be the same value: the kernel drops an absolute value
/// equal to the one the device already holds, which is exactly the case here.
/// It is written one unit along instead -- one unit of a range that spans the
/// whole desk, so not a pixel anyone can see -- and the exact position is
/// written once more when the window closes.
struct Repeat {
    until: std::time::Instant,
    at: std::time::Instant,
    position: (i32, i32),
    offset: i32,
}

/// The desk the compositor lays out, as the bounds of every output it
/// describes. `None` where there is no compositor to ask, which is every
/// session whose pointer space is the display server's own.
fn compositor_desktop() -> Option<Desktop> {
    let layout = crate::platform::linux::display::compositor_layout()?;
    let left = layout.iter().map(|output| output.left).min()?;
    let top = layout.iter().map(|output| output.top).min()?;
    let right = layout
        .iter()
        .map(|output| output.left + output.width as i32)
        .max()?;
    let bottom = layout
        .iter()
        .map(|output| output.top + output.height as i32)
        .max()?;
    (right > left && bottom > top).then(|| Desktop {
        left,
        top,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// The LED a lock key drives.
fn led_bit(lock: Lock) -> u32 {
    1 << match lock {
        Lock::Caps => LED_CAPSL,
        Lock::Num => LED_NUML,
        Lock::Scroll => LED_SCROLLL,
    }
}

/// Whether the layout the compositor will read these codes with is the US one.
/// The environment variable is the same one xkbcommon reads, so it is what a
/// Wayland session is actually using; an unset one is the US default.
fn us_layout() -> bool {
    std::env::var("XKB_DEFAULT_LAYOUT").map_or(true, |layout| layout.is_empty() || layout == "us")
}

/// The key that produces `c` on a US layout, and whether Shift is part of it.
///
/// Every character with no entry cannot be typed at all, which is a real limit
/// of this backend rather than a gap in the table: uinput has no keymap to
/// extend, so only what the layout already produces can be sent.
fn us_key(c: char) -> Option<(u16, bool)> {
    /// Pairs of characters that share a key, unshifted then shifted.
    const PAIRS: [(char, char, u16); 21] = [
        ('1', '!', 0x31),
        ('2', '@', 0x32),
        ('3', '#', 0x33),
        ('4', '$', 0x34),
        ('5', '%', 0x35),
        ('6', '^', 0x36),
        ('7', '&', 0x37),
        ('8', '*', 0x38),
        ('9', '(', 0x39),
        ('0', ')', 0x30),
        ('-', '_', 0xbd),
        ('=', '+', 0xbb),
        ('[', '{', 0xdb),
        (']', '}', 0xdd),
        ('\\', '|', 0xdc),
        (';', ':', 0xba),
        ('\'', '"', 0xde),
        ('`', '~', 0xc0),
        (',', '<', 0xbc),
        ('.', '>', 0xbe),
        ('/', '?', 0xbf),
    ];
    if c.is_ascii_alphabetic() {
        return evdev(u16::from(c.to_ascii_uppercase() as u8))
            .map(|code| (code, c.is_ascii_uppercase()));
    }
    if c.is_ascii_digit() {
        return evdev(u16::from(c as u8)).map(|code| (code, false));
    }
    for (plain, shifted, key) in PAIRS {
        if c == plain {
            return evdev(key).map(|code| (code, false));
        }
        if c == shifted {
            return evdev(key).map(|code| (code, true));
        }
    }
    match c {
        ' ' => evdev(0x20).map(|code| (code, false)),
        '\n' | '\r' => evdev(0x0d).map(|code| (code, false)),
        '\t' => evdev(0x09).map(|code| (code, false)),
        '\u{8}' => evdev(0x08).map(|code| (code, false)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_us_table_covers_every_ascii_printable_character() {
        for c in ' '..='~' {
            assert!(us_key(c).is_some(), "{c:?} 没有对应的按键");
        }
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('A'), Some((30, true)));
        assert_eq!(us_key('!'), Some((2, true)));
        assert_eq!(us_key('/'), Some((53, false)));
        assert_eq!(us_key('?'), Some((53, true)));
        assert_eq!(us_key('\n'), Some((28, false)));
        assert_eq!(us_key('中'), None);
    }
}
