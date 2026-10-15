//! Input injection through `/dev/uinput`: virtual devices the kernel hands to
//! whatever session is running, Xorg or Wayland alike.
//!
//! This is the backend that works where X Test does not — a Wayland session,
//! and a locked screen, which X clients cannot reach at all. It costs one
//! permission: write access to `/dev/uinput`, granted by the udev rule this
//! project ships.
//!
//! Four devices are created, because the kernel and libinput read their
//! capabilities quite differently:
//!
//! - a keyboard, which is every key code there is plus the three lock LEDs;
//! - a pointer, relative, which is what a mouse is;
//! - and, once the desktop geometry is known, an absolute pointer. Relative
//!   motion cannot express "go to this pixel", and the default mouse mode
//!   asks for exactly that;
//! - a touchscreen, which the compositor reads as a device lying on a panel
//!   and dispatches itself. That is what makes a multi-touch session real
//!   instead of a pointer moved by the first finger.
//!
//! Typing is the one place this backend is weaker than X Test: there is no
//! keymap to extend, so a character can only be typed if the layout the user
//! configured produces it. The table here describes the US layout.
//!
//! On a Wayland session that gap is closed by `zwp_virtual_keyboard_v1`
//! (`virtual_keyboard`), which carries a keymap of our own making. This table
//! stays as the fallback for sessions whose compositor does not serve it.
use super::{Lock, Touch, evdev, virtual_keyboard};
use crate::platform::Rotation;
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
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
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const BTN_TOUCH: u16 = 0x14a;
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

/// 触摸屏两条轴能取的最大值。触摸报的是屏幕上的比例，所以量程只决定精度：
/// 这个量程在 5120 宽的屏上一个单位是 0.16 个像素。
const TOUCH_RANGE: f32 = 32767.0;
/// 触摸屏自称的分辨率（单位/毫米），决定合成器算出来的面板物理尺寸。和量程成对
/// 出现才像一块真的面板，别的地方用不到。
const TOUCH_RESOLUTION: i32 = 40;
/// 触摸屏同时能按住的触点数，和协议允许的一样多。
const TOUCH_SLOTS: usize = 10;

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

    fn absolute(&self, axis: u16, maximum: i32, resolution: i32) -> Result<()> {
        let mut setup = UinputAbsSetup {
            code: axis,
            absinfo: InputAbsInfo {
                maximum,
                resolution,
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

// --- the touchscreen --------------------------------------------------------

/// 一块虚拟多点触摸屏。
///
/// 它比「用指针模拟触摸」多出来的东西，正是合成器愿意认它是一块触摸屏的理由：
/// `INPUT_PROP_DIRECT` 和多点轴（`ABS_MT_*`）让 libinput 把它判成触摸屏，于是
/// 它报的触点被合成器原样派发给应用——双指滚动、捏合、手势、触摸面积都是真的，
/// 而不是把一个触点翻译成鼠标左键。
///
/// 设备坐标是「面板自己」的坐标：合成器读它的时候会套上输出当前的旋转和缩放
/// （`Rotation::panel` 就是这件事的逆），所以屏幕大小和缩放都不进这里。
pub(super) struct Touchscreen {
    device: Device,
    contacts: Contacts,
}

impl Touchscreen {
    fn create() -> Result<Self> {
        let device = Device::create("OpenUUYC 虚拟触摸屏", |device| {
            device.event_type(EV_KEY)?;
            device.key(BTN_TOUCH)?;
            device.event_type(EV_ABS)?;
            for axis in [ABS_X, ABS_Y, ABS_MT_POSITION_X, ABS_MT_POSITION_Y] {
                device.absolute(axis, TOUCH_RANGE as i32, TOUCH_RESOLUTION)?;
            }
            device.absolute(ABS_MT_SLOT, TOUCH_SLOTS as i32 - 1, 0)?;
            device.absolute(ABS_MT_TRACKING_ID, 65535, 0)?;
            device.absolute(ABS_MT_TOUCH_MAJOR, TOUCH_RANGE as i32, 0)?;
            // 直接设备：坐标落在某一块输出上，而不是像鼠标那样在整张桌子上。
            device.property(INPUT_PROP_DIRECT)
        })
        .context("创建虚拟触摸屏失败")?;
        Ok(Self {
            device,
            contacts: Contacts::default(),
        })
    }

    fn touch(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<()> {
        let frames = self.contacts.step(kind, points, rotation)?;
        for frame in &frames {
            if let Err(error) = self.device.send(frame) {
                // 写到一半失败，设备里的状态就不知道是什么了，按全抬起来算。
                self.contacts = Contacts::default();
                return Err(error);
            }
        }
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        let frames = self.contacts.lift_all();
        for frame in &frames {
            self.device.send(frame)?;
        }
        Ok(())
    }
}

/// 触摸屏上的触点：控制器给的 id 占着哪个槽位、按没按着。
///
/// 这段逻辑和设备本身分开，是为了能脱开 `/dev/uinput` 单独验：触摸屏的协议要求
/// 全在帧的顺序里（谁先抬起、槽位什么时候能复用、BTN_TOUCH 什么时候翻转）。
#[derive(Default)]
struct Contacts {
    active: BTreeMap<u32, Contact>,
    used: [bool; TOUCH_SLOTS],
    /// 上一个发出去的 tracking id，每个新触点加一。
    tracking: i32,
    /// 上一次发出去的按下状态，只在变化时重发 BTN_TOUCH。
    touching: bool,
}

struct Contact {
    slot: u8,
    tracking: i32,
}

/// 送进设备的一帧：一组事件，内核把它们当成一次同步。
type Frame = Vec<(u16, u16, i32)>;

impl Contacts {
    /// 一帧触点。`kind` 是 UU 的说法：1 是「触点集合从现在开始」，2 是移动，
    /// 3 是抬起，4 是取消。返回要写进设备的帧。
    ///
    /// 一次同步里不能把「抬起旧触点」和「按下新触点」当成同一批，所以 kind 1
    /// 的取消自己是一个完整的帧。
    fn step(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        if kind == 1 || kind >= 3 {
            frames.extend(self.lift_all());
            if kind >= 3 {
                // 抬起和取消在 evdev 里没有区别：一个触点只有抬起这一种结束。
                return Ok(frames);
            }
        }
        let mut frame = Frame::new();
        // 这一帧里没有了的触点先抬起，槽位空出来给新触点用。
        let gone: Vec<u32> = self
            .active
            .keys()
            .copied()
            .filter(|id| !points.iter().any(|point| point.id == *id))
            .collect();
        for id in gone {
            let Some(contact) = self.active.remove(&id) else {
                continue;
            };
            self.used[usize::from(contact.slot)] = false;
            frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
            frame.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
        }
        for point in points {
            let (x, y) = position(point, rotation);
            match self.active.get(&point.id) {
                Some(contact) => {
                    frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
                }
                None => {
                    let slot = self
                        .used
                        .iter()
                        .position(|used| !used)
                        .context("虚拟触摸屏的触点已经用满")?;
                    self.used[slot] = true;
                    // 新触点要一个新的 tracking id；抬起时把那个槽位的 id 写回 -1。
                    self.tracking = self.tracking.wrapping_add(1);
                    let contact = Contact {
                        slot: slot as u8,
                        tracking: self.tracking,
                    };
                    frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
                    frame.push((EV_ABS, ABS_MT_TRACKING_ID, contact.tracking));
                    self.active.insert(point.id, contact);
                }
            }
            frame.push((EV_ABS, ABS_MT_POSITION_X, x));
            frame.push((EV_ABS, ABS_MT_POSITION_Y, y));
            frame.push((EV_ABS, ABS_MT_TOUCH_MAJOR, touch_value(point.radius)));
        }
        // 第一个触点的单点坐标。实体触摸屏也会报，libinput 的单点回退路径读它。
        if let Some(point) = points.first() {
            let (x, y) = position(point, rotation);
            frame.push((EV_ABS, ABS_X, x));
            frame.push((EV_ABS, ABS_Y, y));
        }
        let touching = !self.active.is_empty();
        if touching != self.touching {
            self.touching = touching;
            frame.push((EV_KEY, BTN_TOUCH, i32::from(touching)));
        }
        if !frame.is_empty() {
            frames.push(frame);
        }
        Ok(frames)
    }

    /// 抬起所有触点，并把槽位还回去。
    fn lift_all(&mut self) -> Vec<Frame> {
        if self.active.is_empty() {
            return Vec::new();
        }
        let mut frame = Frame::new();
        for contact in std::mem::take(&mut self.active).into_values() {
            frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
            frame.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
        }
        self.used = [false; TOUCH_SLOTS];
        self.touching = false;
        frame.push((EV_KEY, BTN_TOUCH, 0));
        vec![frame]
    }
}

/// 触点在内核里的位置：先把屏幕比例变回面板比例，再放到量程上。
fn position(point: &Touch, rotation: Rotation) -> (i32, i32) {
    let (x, y) = rotation.panel(point.x.clamp(0.0, 1.0), point.y.clamp(0.0, 1.0));
    (touch_value(x), touch_value(y))
}

/// 一个比例值放到触摸屏的量程上。
fn touch_value(fraction: f32) -> i32 {
    (fraction.clamp(0.0, 1.0) * TOUCH_RANGE).round() as i32
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
    /// 多点触摸屏，拿不到就没有：触摸退回单指模拟。
    touchscreen: Option<Touchscreen>,
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
        // 触摸屏没有别的后备：没有它就退回用指针模拟单指，见 engine 里的 touch。
        // 一块从来收不到触点的触摸屏什么也不影响，所以它和设备一起常驻。
        let touchscreen = match Touchscreen::create() {
            Ok(touchscreen) => {
                tracing::info!("虚拟触摸屏已就绪，控制器用平板触摸模式时由合成器派发多点触摸");
                Some(touchscreen)
            }
            Err(error) => {
                tracing::warn!(%error, "没有虚拟触摸屏，触摸退回单指模拟");
                None
            }
        };
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
            touchscreen,
        })
    }

    /// What the host's diagnostics page calls this session's injector. A
    /// touchscreen is worth naming there: it says whether the controller's
    /// contacts are going out as touches or as a pointer with a button held.
    pub(super) fn label(&self) -> &'static str {
        if self.touchscreen.is_some() {
            "uinput · 系统级虚拟输入（含多点触摸屏）"
        } else {
            "uinput · 系统级虚拟输入"
        }
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
            device.absolute(ABS_X, width, 0)?;
            device.absolute(ABS_Y, height, 0)?;
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

    /// 真实的多点触摸。返回 false 表示没有触摸屏，调用方退回单指模拟。
    pub(super) fn touch(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<bool> {
        let Some(touchscreen) = &mut self.touchscreen else {
            return Ok(false);
        };
        touchscreen.touch(kind, points, rotation)?;
        Ok(true)
    }

    /// 抬起所有触点。会话结束、换屏、取消时都要走一遍：合成器那边一个没抬起来
    /// 的触点会一直按着，应用也收不到结束。
    pub(super) fn release_touch(&mut self) -> Result<()> {
        match &mut self.touchscreen {
            Some(touchscreen) => touchscreen.release(),
            None => Ok(()),
        }
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

    fn contact(id: u32, x: f32, y: f32) -> Touch {
        Touch {
            id,
            x,
            y,
            radius: 0.02,
        }
    }

    /// 触点按下的那一帧：新槽位、新的 tracking id、坐标，以及整个面板的按下状态。
    #[test]
    fn the_first_contact_presses_the_panel_and_lifting_it_releases_it() {
        let mut contacts = Contacts::default();
        let frames = contacts
            .step(2, &[contact(7, 0.25, 0.75)], Rotation::Normal)
            .unwrap();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert!(frame.contains(&(EV_ABS, ABS_MT_SLOT, 0)));
        assert!(frame.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 1)));
        assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.25))));
        assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_Y, touch_value(0.75))));
        // 单点坐标跟着第一个触点走
        assert!(frame.contains(&(EV_ABS, ABS_X, touch_value(0.25))));
        assert!(frame.contains(&(EV_ABS, ABS_Y, touch_value(0.75))));
        assert!(frame.contains(&(EV_KEY, BTN_TOUCH, 1)));

        let frames = contacts.step(3, &[], Rotation::Normal).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(&(EV_ABS, ABS_MT_TRACKING_ID, -1)));
        assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
        assert!(contacts.active.is_empty());
        assert!(contacts.used.iter().all(|used| !used));
    }

    /// 合成器把设备的坐标按输出的旋转解释，所以转过的屏上两条轴要换过来报。
    #[test]
    fn a_rotated_output_moves_the_axes_the_panel_reports() {
        let mut contacts = Contacts::default();
        // 屏幕上的 (0.25, 0.6)：面板逆时针转 90° 之后落在面板的 (0.6, 0.75)
        let frames = contacts
            .step(2, &[contact(1, 0.25, 0.6)], Rotation::Rotate90)
            .unwrap();
        let frame = &frames[0];
        assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.6))));
        assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_Y, touch_value(0.75))));
        assert!(!frame.contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.25))));
    }

    /// 两根手指各占一个槽位，抬起其中一根不能碰到另一根，槽位还得还回来。
    #[test]
    fn two_contacts_take_two_slots_and_a_gone_one_frees_only_its_own() {
        let mut contacts = Contacts::default();
        contacts
            .step(
                2,
                &[contact(1, 0.1, 0.1), contact(2, 0.9, 0.9)],
                Rotation::Normal,
            )
            .unwrap();
        assert_eq!(contacts.active.len(), 2);
        assert_eq!(contacts.used.iter().filter(|used| **used).count(), 2);

        // 第二根手指抬起，第一根还在：帧里抬起后分配的那个槽位，面板仍然是按下的
        let frames = contacts
            .step(2, &[contact(1, 0.2, 0.2)], Rotation::Normal)
            .unwrap();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert!(frame.contains(&(EV_ABS, ABS_MT_SLOT, 1)));
        assert!(frame.contains(&(EV_ABS, ABS_MT_TRACKING_ID, -1)));
        assert!(!frame.contains(&(EV_KEY, BTN_TOUCH, 0)));
        assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.2))));
        assert_eq!(contacts.active.len(), 1);
        assert!(!contacts.used[1]);
        assert!(contacts.used[0]);
    }

    /// 「触点集合从现在开始」这一类帧，先抬起旧的一套，再按下新的一套。
    #[test]
    fn a_new_set_lifts_the_old_contacts_in_a_frame_of_their_own() {
        let mut contacts = Contacts::default();
        contacts
            .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
            .unwrap();
        let frames = contacts
            .step(1, &[contact(9, 0.5, 0.5)], Rotation::Normal)
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
        assert!(frames[1].contains(&(EV_KEY, BTN_TOUCH, 1)));
        assert_eq!(contacts.active.keys().copied().collect::<Vec<_>>(), [9]);
    }

    /// 取消和抬起一样：触点全抬起，再收到一次抬起不必再发一帧。
    #[test]
    fn a_cancelled_set_lifts_every_contact() {
        let mut contacts = Contacts::default();
        contacts
            .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
            .unwrap();
        let frames = contacts
            .step(4, &[contact(1, 0.5, 0.5)], Rotation::Normal)
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
        assert!(contacts.active.is_empty());
        assert!(contacts.step(3, &[], Rotation::Normal).unwrap().is_empty());
    }

    /// 触点数超过槽位时报错，而不是把触点悄悄丢掉或者panic。
    #[test]
    fn the_panel_refuses_more_contacts_than_it_has_slots() {
        let mut contacts = Contacts::default();
        let points: Vec<Touch> = (0..TOUCH_SLOTS as u32 + 1)
            .map(|id| contact(id, 0.5, 0.5))
            .collect();
        assert!(contacts.step(2, &points, Rotation::Normal).is_err());
    }

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
