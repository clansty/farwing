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
//! - an absolute pointer, whose axes are a fixed range the compositor resolves
//!   against its own output layout. Relative motion cannot express "go to this
//!   pixel", and the default mouse mode asks for exactly that;
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
//!
//! That protocol has a blind spot of its own: the keymap it carries means
//! nothing to an X client (XWayland), which resolves key codes against the X
//! server's keymap — the text arrives as whatever that map makes of those
//! codes, the first character as Escape. Text for a focused X client goes
//! through X Test instead, which is the one reason this backend keeps an X
//! connection.
use super::{Lock, Touch, evdev, virtual_keyboard, xtest};
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

/// 绝对轴的最大值，与 Sunshine 一样固定，不随布局变：布局变了只改换算，
/// 不用换设备（换设备又要等合成器重新接手）。
const AXIS_MAX: i32 = 65535;

/// 触摸屏两条轴能取的最大值。触摸报的是屏幕上的比例，所以量程只决定精度：
/// 这个量程在 5120 宽的屏上一个单位是 0.16 个像素。
const TOUCH_RANGE: f32 = 32767.0;
/// 触摸屏自称的分辨率（单位/毫米），决定合成器算出来的面板物理尺寸。和量程成对
/// 出现才像一块真的面板，别的地方用不到。
const TOUCH_RESOLUTION: i32 = 40;
/// 触摸屏同时能按住的触点数，和协议允许的一样多。
const TOUCH_SLOTS: usize = 10;
/// 设备声明的 tracking id 上限。id 在 `0..=TRACKING_MAX` 里回绕，和内核
/// `input_mt` 一样按位截断，-1 留给「抬起」。
const TRACKING_MAX: i32 = 0xffff;

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

    /// 绝对指针：`ABS_X`/`ABS_Y` 的范围固定为 `0..=AXIS_MAX`，合成器把它按
    /// 比例映射到自己全部输出的外接矩形上。
    fn open_absolute() -> Result<Self> {
        Self::create("OpenUUYC 虚拟绝对鼠标", |device| {
            device.event_type(EV_KEY)?;
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                device.key(button)?;
            }
            device.event_type(EV_ABS)?;
            device.absolute(ABS_X, AXIS_MAX, 0)?;
            device.absolute(ABS_Y, AXIS_MAX, 0)?;
            // A pointer, not a direct device: `INPUT_PROP_DIRECT` tells a
            // compositor the coordinates map onto a single output, and these
            // are a fraction of the whole desk.
            device.property(INPUT_PROP_POINTER)
        })
        .context("创建虚拟绝对鼠标失败")
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
/// 它报的触点被合成器原样派发给应用——双指滚动、捏合、手势都是真的，而不是把
/// 一个触点翻译成鼠标左键。
///
/// 触点大小照实体触摸屏的样子报在 `ABS_MT_TOUCH_MAJOR` 上，但 libinput 处理
/// 触摸屏的那条路径不读这个轴，公开接口里也没有触摸屏的接触面积，所以合成器和
/// 应用拿不到它。
///
/// 设备坐标是「面板自己」的坐标：合成器读它的时候会套上输出当前的旋转和缩放
/// （`panel` 就是这件事的逆），所以屏幕大小和缩放都不进这里。
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
            device.absolute(ABS_MT_TRACKING_ID, TRACKING_MAX, 0)?;
            // 接触椭圆的长轴，按 evdev 的定义是直径，量程和位置轴一样。
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

    /// 算出这一帧触点要写进设备的帧。触点比槽位多时报错，什么也不改，设备
    /// 照样能用。
    fn step(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<Vec<Frame>> {
        self.contacts.step(kind, points, rotation)
    }

    /// 把帧写进设备。写失败时设备里按着哪些触点就不知道了，调用方要丢掉这块
    /// 触摸屏：设备一移除，合成器（libinput）会自己把它上面的触点全部收尾。
    fn write(&self, frames: &[Frame]) -> Result<()> {
        for frame in frames {
            self.device.send(frame)?;
        }
        Ok(())
    }

    /// 抬起所有触点。和 `write` 一样，失败了这块触摸屏就不能再用。
    fn release(&mut self) -> Result<()> {
        let frames = self.contacts.lift_all();
        self.write(&frames)
    }
}

/// 触摸屏上的触点：控制器给的 id 占着哪个槽位、按没按着。
///
/// 这段逻辑和设备本身分开，是为了能脱开 `/dev/uinput` 单独验：触摸屏的协议要求
/// 全在帧的顺序里（谁先抬起、槽位什么时候能复用、BTN_TOUCH 什么时候翻转）。
#[derive(Clone, Default)]
struct Contacts {
    active: BTreeMap<u32, Contact>,
    used: [bool; TOUCH_SLOTS],
    /// 上一个发出去的 tracking id，每个新触点加一，在声明的范围里回绕。
    tracking: i32,
    /// 上一次发出去的按下状态，只在变化时重发 BTN_TOUCH。
    touching: bool,
}

#[derive(Clone)]
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
    /// 一次同步里不能把「抬起旧触点」和「按下新触点」当成同一批：新触点常常拿到
    /// 刚空出来的槽位，同一帧里同一个槽位先写 -1 再写新 id，libinput
    /// （`evdev-fallback.c`）会当成内核的错误，抬起和按下一起丢掉。所以抬起
    /// 总是自己一帧，和 Windows 的 `Touch::update` 先单独注入一帧 UP 一样。
    ///
    /// 出错时（触点比槽位多）什么也不改：整步先在一份副本上算完，成功了才换上。
    /// 否则消失的触点已经从这里删掉，抬起它们的那一帧却没有发出去，合成器那边
    /// 就会一直按着。
    fn step(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<Vec<Frame>> {
        let mut next = self.clone();
        let frames = next.advance(kind, points, rotation)?;
        *self = next;
        Ok(frames)
    }

    fn advance(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        if kind == 1 || kind >= 3 {
            frames.extend(self.lift_all());
            if kind >= 3 {
                // 抬起和取消在 evdev 里没有区别：一个触点只有抬起这一种结束。
                return Ok(frames);
            }
        }
        // 这一帧里没有了的触点先抬起，槽位空出来给新触点用。
        let gone: Vec<u32> = self
            .active
            .keys()
            .copied()
            .filter(|id| !points.iter().any(|point| point.id == *id))
            .collect();
        let mut lifts = Frame::new();
        for id in gone {
            let Some(contact) = self.active.remove(&id) else {
                continue;
            };
            self.used[usize::from(contact.slot)] = false;
            lifts.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
            lifts.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
        }
        if !lifts.is_empty() {
            if self.active.is_empty() && self.touching {
                self.touching = false;
                lifts.push((EV_KEY, BTN_TOUCH, 0));
            }
            frames.push(lifts);
        }
        let mut frame = Frame::new();
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
                    let contact = Contact {
                        slot: slot as u8,
                        tracking: self.next_tracking(),
                    };
                    frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
                    frame.push((EV_ABS, ABS_MT_TRACKING_ID, contact.tracking));
                    self.active.insert(point.id, contact);
                }
            }
            frame.push((EV_ABS, ABS_MT_POSITION_X, x));
            frame.push((EV_ABS, ABS_MT_POSITION_Y, y));
            frame.push((EV_ABS, ABS_MT_TOUCH_MAJOR, touch_value(2.0 * point.radius)));
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

    /// 下一个 tracking id：加一后截到声明的范围里，跳过还按着的触点正在用的 id
    /// （回绕一圈之后才可能撞上；按着的最多 `TOUCH_SLOTS` 个，所以一定找得到）。
    fn next_tracking(&mut self) -> i32 {
        loop {
            self.tracking = (self.tracking + 1) & TRACKING_MAX;
            let tracking = self.tracking;
            if !self
                .active
                .values()
                .any(|contact| contact.tracking == tracking)
            {
                return tracking;
            }
        }
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
    let (x, y) = panel(rotation, point.x.clamp(0.0, 1.0), point.y.clamp(0.0, 1.0));
    (touch_value(x), touch_value(y))
}

/// A position on the screen as a fraction of it, to the same position as a
/// fraction of the touchscreen's own, untransformed panel.
///
/// The compositor does the opposite: it reads a touch as a fraction of the
/// panel and applies the output's transform to it, in the output's rectangle
/// (smithay's `transform_point_in`, which is what a compositor scales a touch
/// device's axes with). Undoing that is this inverse, in normalized
/// coordinates: both spaces are the unit square, so the axis lengths, the
/// output's scale and its physical size never enter.
fn panel(rotation: Rotation, x: f32, y: f32) -> (f32, f32) {
    match rotation {
        Rotation::Normal => (x, y),
        Rotation::Rotate90 => (y, 1.0 - x),
        Rotation::Rotate180 => (1.0 - x, 1.0 - y),
        Rotation::Rotate270 => (1.0 - y, x),
        Rotation::Flipped => (1.0 - x, y),
        Rotation::Flipped90 => (y, x),
        Rotation::Flipped180 => (x, 1.0 - y),
        Rotation::Flipped270 => (1.0 - y, 1.0 - x),
    }
}

/// 一个比例值放到触摸屏的量程上。
fn touch_value(fraction: f32) -> i32 {
    (fraction.clamp(0.0, 1.0) * TOUCH_RANGE).round() as i32
}

// --- the injector -----------------------------------------------------------

/// 绝对坐标换算所在的区域：合成器全局布局的外接矩形，左上角加尺寸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Desktop {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// 合成器全局布局的外接矩形，包括没有名字的输出：合成器把绝对指针映射到
/// 它全部输出的外接矩形上（wlroots 的 output layout、Hyprland、niri、KWin 的
/// 工作区、Mutter 的 stage），少算一块屏落点就整体偏。
pub(crate) fn layout_bounds(layout: &crate::platform::CompositorLayout) -> Option<Desktop> {
    let mut outputs = layout
        .outputs
        .iter()
        .filter(|o| o.width > 0 && o.height > 0);
    let first = outputs.next()?;
    let span = |o: &crate::platform::CompositorOutput| {
        (
            i64::from(o.left),
            i64::from(o.top),
            i64::from(o.left) + i64::from(o.width),
            i64::from(o.top) + i64::from(o.height),
        )
    };
    let (mut left, mut top, mut right, mut bottom) = span(first);
    for output in outputs {
        let (l, t, r, b) = span(output);
        left = left.min(l);
        top = top.min(t);
        right = right.max(r);
        bottom = bottom.max(b);
    }
    Some(Desktop {
        left: i32::try_from(left).ok()?,
        top: i32::try_from(top).ok()?,
        width: u32::try_from(right - left).ok()?,
        height: u32::try_from(bottom - top).ok()?,
    })
}

/// 合成器把一个轴值落到像素 `origin + floor(value × extent / (AXIS_MAX + 1))`
/// 上（libinput 按 `max - min + 1` 换算）。取像素中心对应的值，取整之后正好
/// 落回这个像素。超过 `AXIS_MAX + 1` 像素宽的布局会有相邻像素共用一个值。
fn axis_value(position: i32, origin: i32, extent: u32) -> i32 {
    if extent == 0 {
        return 0;
    }
    let range = i64::from(AXIS_MAX) + 1;
    let extent = i64::from(extent);
    let offset = (i64::from(position) - i64::from(origin)).clamp(0, extent - 1);
    ((2 * offset + 1) * range / (2 * extent)).clamp(0, i64::from(AXIS_MAX)) as i32
}

/// 内核丢弃与设备当前值相同的绝对坐标，所以要重写同一个值时先在单独一帧里
/// 挪开一个单位（不到一像素）。在轴的最大值处往回挪，不能出界。只有两条轴
/// 都相同才需要：只丢一条轴时，合成器沿用设备自己的那个值，正是目标值。
fn nudge(last: (i32, i32), value: (i32, i32)) -> Option<(i32, i32)> {
    (last == value).then(|| {
        let x = if value.0 >= AXIS_MAX {
            value.0 - 1
        } else {
            value.0 + 1
        };
        (x, value.1)
    })
}

/// 合成器接手一台刚建的 uinput 设备要多久。这段时间里写进去的绝对坐标谁都
/// 收不到（evdev 只把 open 之后的事件交给读者），所以要反复补写。
const FRESH_FOR: Duration = Duration::from_millis(1500);

/// 上面那个窗口期里补写的间隔。
const REPEAT_EVERY: Duration = Duration::from_millis(250);

/// 窗口期里还要补写的位置。
struct Repeat {
    until: std::time::Instant,
    next: std::time::Instant,
    value: (i32, i32),
}

/// 由合成器解算的绝对指针。轴范围固定（与 Sunshine 一样），按布局外接矩形
/// 换算，所以布局变了也不用换设备。
struct Absolute {
    device: Device,
    made: std::time::Instant,
    /// 设备轴上的当前值（内核里的那份），新建时为 0。
    last: (i32, i32),
    repeat: Option<Repeat>,
}

impl Absolute {
    fn create() -> Result<Self> {
        Ok(Self {
            device: Device::open_absolute()?,
            made: std::time::Instant::now(),
            last: (0, 0),
            repeat: None,
        })
    }

    /// 写入一对轴值。与当前值相同时内核会把它整帧丢掉，而光标可能已经被别的
    /// 设备（相对设备、本机鼠标、程序自己移动指针）挪走了，所以先在单独一帧里
    /// 挪开一个单位，再写目标值。
    fn emit(&mut self, value: (i32, i32)) -> Result<()> {
        if let Some((x, y)) = nudge(self.last, value) {
            self.device
                .send(&[(EV_ABS, ABS_X, x), (EV_ABS, ABS_Y, y)])?;
        }
        self.device
            .send(&[(EV_ABS, ABS_X, value.0), (EV_ABS, ABS_Y, value.1)])?;
        self.last = value;
        Ok(())
    }

    /// 把合成器空间里的一点写进设备，设备还新的时候安排补写。
    fn aim(&mut self, x: i32, y: i32, bounds: Desktop) -> Result<()> {
        let value = (
            axis_value(x, bounds.left, bounds.width),
            axis_value(y, bounds.top, bounds.height),
        );
        self.emit(value)?;
        let now = std::time::Instant::now();
        let until = self.made + FRESH_FOR;
        self.repeat = (now < until).then(|| Repeat {
            until,
            next: now + REPEAT_EVERY,
            value,
        });
        Ok(())
    }

    /// 窗口期里按间隔补写。补写的值与当前值相同，`emit` 会先挪开一个单位，
    /// 每次补写都以目标值结束，窗口结束时不必再写。
    fn flush(&mut self) -> Result<()> {
        let Some(repeat) = &mut self.repeat else {
            return Ok(());
        };
        let now = std::time::Instant::now();
        if now >= repeat.until {
            self.repeat = None;
            return Ok(());
        }
        if now < repeat.next {
            return Ok(());
        }
        repeat.next = now + REPEAT_EVERY;
        let value = repeat.value;
        self.emit(value)
    }
}

pub(super) struct Injector {
    keyboard: Device,
    relative: Device,
    /// 由合成器按它自己的全局布局解算的绝对指针；拿不到就没有，退回相对移动。
    absolute: Option<Absolute>,
    /// Where this backend last left the pointer. The kernel cannot be asked.
    position: (i32, i32),
    /// The lock keys, as a mask of the LEDs they drive.
    leds: u32,
    /// Text with a keymap of our own, where the compositor offers it.
    virtual_keyboard: Option<virtual_keyboard::Keyboard>,
    /// 前台是 XWayland 客户端时的文字输入，见 [`Self::x_client_focused`]。
    /// 没有 XWayland（纯 Wayland 会话、或 X 在别处）就是 None。
    xtest: Option<xtest::Injector>,
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
        // 绝对定位另建一台设备，越早建越好：合成器接手它要几百毫秒。轴范围
        // 固定，用不用它由引擎按合成器布局决定。
        let absolute = match Absolute::create() {
            Ok(absolute) => {
                tracing::info!("虚拟绝对指针已就绪，绝对定位按合成器布局解算");
                Some(absolute)
            }
            Err(error) => {
                tracing::warn!(%error, "虚拟绝对指针不可用，绝对定位改用相对移动");
                None
            }
        };
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
        // XTest 在这里只为一件合成器做不到的事留着：把文字送进 XWayland 客户端
        // （虚拟键盘自带的 keymap 对它没用，见 `x_client_focused`）。
        //
        // 只认本地 Wayland 会话的那台 XWayland。真正的 Xorg 没有 `XWAYLAND` 扩展，
        // 用不到它；DISPLAY 指到别处时连都不连，见 `local_display`。
        let xtest = if local_display() {
            xtest::Injector::open()
                .ok()
                .filter(|injector| injector.is_xwayland().unwrap_or(false))
        } else {
            None
        };
        Ok(Self {
            keyboard,
            relative,
            absolute,
            position: (0, 0),
            leds: 0,
            virtual_keyboard,
            xtest,
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

    /// 有没有虚拟绝对指针。有它，引擎才可能按合成器布局定位。
    pub(super) fn has_absolute_pointer(&self) -> bool {
        self.absolute.is_some()
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

    /// Move to a root-window position, in the display server's own space.
    /// Positions in the compositor's layout go through [`Self::aim`] instead.
    pub(super) fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        // 设备不声明 ABS 轴（否则会被 libinput 误分类），所以按当前指针位置
        // 换算成相对位移。
        let (dx, dy) = (x - self.position.0, y - self.position.1);
        self.move_by(dx, dy)
    }

    /// 移到合成器全局空间里的一点，由合成器按 `bounds`（它全部输出的外接
    /// 矩形）解算。设备写入失败就不再用它，之后的绝对定位回到相对移动。
    pub(super) fn aim(&mut self, x: i32, y: i32, bounds: Desktop) -> Result<()> {
        let absolute = self.absolute.as_mut().context("虚拟绝对指针不可用")?;
        let result = absolute.aim(x, y, bounds);
        match &result {
            Ok(()) => {
                // 记下落点：设备之后被停用时，相对移动要靠它算位移，`pointer()`
                // 问的也是它。
                self.position = (x, y);
            }
            Err(error) => {
                tracing::warn!(%error, "虚拟绝对指针写入失败，停用并改用相对移动");
                self.absolute = None;
            }
        }
        result
    }

    /// 放弃还没补写完的位置（输入被收回或释放时，不再替对端移动指针）。
    pub(super) fn forget_aim(&mut self) {
        if let Some(absolute) = &mut self.absolute {
            absolute.repeat = None;
        }
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
        let frames = touchscreen.step(kind, points, rotation)?;
        let written = touchscreen.write(&frames);
        self.keep_touchscreen(written)?;
        Ok(true)
    }

    /// 抬起所有触点。会话结束、换屏、取消时都要走一遍：合成器那边一个没抬起来
    /// 的触点会一直按着，应用也收不到结束。
    pub(super) fn release_touch(&mut self) -> Result<()> {
        let Some(touchscreen) = &mut self.touchscreen else {
            return Ok(());
        };
        let released = touchscreen.release();
        self.keep_touchscreen(released)
    }

    /// 触摸屏写入失败就不再用它，和绝对指针一样：写到一半时合成器那边可能还按着
    /// 触点，而这边已经说不清是哪些了。丢掉设备，内核和 libinput 会在它移除时
    /// 把上面的触点全部抬起；之后的触摸退回单指模拟，日志也只记这一次。
    fn keep_touchscreen(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = &result {
            tracing::warn!(%error, "虚拟触摸屏写入失败，停用它，触摸退回单指模拟");
            self.touchscreen = None;
        }
        result
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
    /// Three mappings can do it, picked by who has focus (see
    /// [`Self::type_text`]): the virtual keyboard carries a keymap of our own,
    /// so any character goes through; X Test reaches an X client, which that
    /// keymap never would; the local layout is the fallback, and can only
    /// produce what that layout has. A Windows line break (CR LF) is one Enter
    /// either way.
    pub(super) fn text(
        &mut self,
        text: &str,
        held: &BTreeSet<u16>,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        let lifted = self.lift(held)?;
        let result = self.type_text(text, &permitted);
        for code in lifted {
            let _ = self.keyboard.send(&[(EV_KEY, code, 1)]);
        }
        result
    }

    /// 按前台窗口是谁来选一条打字的路。
    ///
    /// 虚拟键盘自带的那份 keymap 只在合成器内部生效；前台是 X 客户端时，X 按自己
    /// 的键位表解 keycode，会打出一串乱码（第一个字符还常常变成 Esc），这种情况
    /// 交给 XTest。前台是原生 Wayland 客户端（或没有 X）时才用虚拟键盘。
    fn type_text(&mut self, text: &str, permitted: &impl Fn() -> bool) -> Result<()> {
        if self.x_client_focused() {
            // 前台是 X 客户端，虚拟键盘对它无效；没有 XTest 时只剩本地布局。
            if let Some(xtest) = &self.xtest {
                return xtest.text(text, permitted);
            }
        } else {
            let typed = self
                .virtual_keyboard
                .as_mut()
                .map(|keyboard| keyboard.text(text, permitted));
            match typed {
                Some(Ok(())) => return Ok(()),
                other => {
                    if let Some(Err(error)) = other {
                        // Half a text may already be through; typing it again
                        // from the top beats dropping the rest of it.
                        tracing::warn!(%error, "虚拟键盘输入失败，改用本地布局");
                    }
                }
            }
        }
        self.type_locally(text, permitted)
    }

    /// 前台窗口是不是跑在 XWayland 里（也就是一个 X 客户端）。
    ///
    /// 虚拟键盘自带的那份 keymap 只在合成器内部生效，X 客户端拿自己那份键位表解
    /// keycode，于是打出来是乱码（第一个字符就变成 Esc）。XWayland 在前台是原生
    /// Wayland 窗口时，把自己的输入焦点设成 `None`（或者 `PointerRoot`），有 X
    /// 窗口拿到焦点时才是那块窗口——问一次 X 就够，不必依赖某个合成器的 IPC
    /// （niri、sway 都没有 Hyprland 那套）。
    ///
    /// 问不出来时按原生客户端处理。本机的 X 连接上这个查询不会平白失败，失败只
    /// 说明连接已经没了（XWayland 崩溃或重启），那时 XTest 也够不着任何 X 客户端，
    /// 退回设备这条路反而能让原生窗口继续打字，而不是次次都报错。
    fn x_client_focused(&self) -> bool {
        self.xtest
            .as_ref()
            .is_some_and(|xtest| xtest.x_client_focused().unwrap_or(false))
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
        if let Some(absolute) = &mut self.absolute
            && let Err(error) = absolute.flush()
        {
            tracing::warn!(%error, "虚拟绝对指针写入失败，停用并改用相对移动");
            self.absolute = None;
            return Err(error);
        }
        Ok(())
    }
}

/// The LED a lock key drives.
fn led_bit(lock: Lock) -> u32 {
    1 << match lock {
        Lock::Caps => LED_CAPSL,
        Lock::Num => LED_NUML,
        Lock::Scroll => LED_SCROLLL,
    }
}

/// `DISPLAY` 指的是本机的 X 服务器（Unix socket）才去连它。
///
/// `x11rb::connect` 走 TCP 时没有超时，一个连不上的远端 DISPLAY（ssh 转发那种）
/// 会把输入引擎的启动卡住两分钟；XWayland 一定在本机。
fn local_display() -> bool {
    std::env::var("DISPLAY")
        .is_ok_and(|display| matches!(display.split(':').next(), Some("") | Some("unix")))
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
    use crate::platform::{CompositorLayout, CompositorOutput};

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
        // 触点大小报的是直径：半径 0.02 的触点是 0.04
        assert!(frame.contains(&(EV_ABS, ABS_MT_TOUCH_MAJOR, touch_value(0.04))));
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

        // 第二根手指抬起，第一根还在：抬起自己一帧，面板仍然是按下的；剩下那根的
        // 移动在下一帧
        let frames = contacts
            .step(2, &[contact(1, 0.2, 0.2)], Rotation::Normal)
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0],
            [(EV_ABS, ABS_MT_SLOT, 1), (EV_ABS, ABS_MT_TRACKING_ID, -1)]
        );
        assert!(!frames[1].contains(&(EV_KEY, BTN_TOUCH, 0)));
        assert!(frames[1].contains(&(EV_ABS, ABS_MT_SLOT, 0)));
        assert!(frames[1].contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.2))));
        assert_eq!(contacts.active.len(), 1);
        assert!(!contacts.used[1]);
        assert!(contacts.used[0]);
    }

    /// 每一帧里每个槽位写过的 tracking id。libinput 在同一帧里看到一个槽位先
    /// 写 -1 再写新 id，会把抬起和按下一起丢掉。
    fn ends_and_begins_in_one_frame(frames: &[Frame]) -> bool {
        frames.iter().any(|frame| {
            let mut slot = None;
            let mut written: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
            for &(kind, code, value) in frame {
                match (kind, code) {
                    (EV_ABS, ABS_MT_SLOT) => slot = Some(value),
                    (EV_ABS, ABS_MT_TRACKING_ID) => {
                        written.entry(slot.unwrap()).or_default().push(value)
                    }
                    _ => {}
                }
            }
            written
                .values()
                .any(|ids| ids.contains(&-1) && ids.iter().any(|id| *id >= 0))
        })
    }

    /// 一根手指抬起、另一根同时按下（主控在两次点击之间没有发抬起）：抬起和按下
    /// 落在两帧里，按下的那一帧可以复用刚空出来的槽位。
    #[test]
    fn a_contact_replaced_in_one_step_lifts_and_presses_in_separate_frames() {
        let mut contacts = Contacts::default();
        contacts
            .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
            .unwrap();
        let frames = contacts
            .step(2, &[contact(2, 0.3, 0.3)], Rotation::Normal)
            .unwrap();
        assert_eq!(frames.len(), 2);
        // 先抬起，面板也跟着松开
        assert_eq!(
            frames[0],
            [
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_KEY, BTN_TOUCH, 0),
            ]
        );
        // 再按下新触点，同一个槽位、新的 tracking id
        assert!(frames[1].contains(&(EV_ABS, ABS_MT_SLOT, 0)));
        assert!(frames[1].contains(&(EV_ABS, ABS_MT_TRACKING_ID, 2)));
        assert!(!frames[1].contains(&(EV_ABS, ABS_MT_TRACKING_ID, -1)));
        assert!(frames[1].contains(&(EV_KEY, BTN_TOUCH, 1)));
        assert!(!ends_and_begins_in_one_frame(&frames));
    }

    /// 一串换手指的帧里，没有哪一帧对同一个槽位既写 -1 又写新 id。
    #[test]
    fn no_frame_ends_and_begins_a_contact_in_the_same_slot() {
        let mut contacts = Contacts::default();
        let steps: [&[Touch]; 6] = [
            &[contact(1, 0.1, 0.1), contact(2, 0.2, 0.2)],
            // 1 抬起，3 按下：3 拿到 1 空出来的槽位 0
            &[contact(2, 0.2, 0.3), contact(3, 0.4, 0.4)],
            // 两根都换掉
            &[contact(4, 0.5, 0.5), contact(5, 0.6, 0.6)],
            &[contact(5, 0.6, 0.7)],
            &[contact(6, 0.7, 0.7)],
            &[],
        ];
        for points in steps {
            let frames = contacts.step(2, points, Rotation::Normal).unwrap();
            assert!(!ends_and_begins_in_one_frame(&frames), "{frames:?}");
        }
        assert!(contacts.active.is_empty());
        assert!(!contacts.touching);
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

    /// 走不下去的一步什么也不改：本来按着的触点还记着，之后照样能抬起来。
    #[test]
    fn a_refused_step_keeps_the_contacts_it_had() {
        let mut contacts = Contacts::default();
        contacts
            .step(
                2,
                &[contact(1, 0.1, 0.1), contact(2, 0.2, 0.2)],
                Rotation::Normal,
            )
            .unwrap();
        // 1 和 2 都没了，换上 11 个新触点：槽位不够
        let points: Vec<Touch> = (10..10 + TOUCH_SLOTS as u32 + 1)
            .map(|id| contact(id, 0.5, 0.5))
            .collect();
        assert!(contacts.step(2, &points, Rotation::Normal).is_err());
        assert_eq!(contacts.active.keys().copied().collect::<Vec<_>>(), [1, 2]);
        assert_eq!(contacts.used.iter().filter(|used| **used).count(), 2);
        assert_eq!(contacts.tracking, 2);
        assert!(contacts.touching);

        let frames = contacts.step(3, &[], Rotation::Normal).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(&(EV_ABS, ABS_MT_SLOT, 0)));
        assert!(frames[0].contains(&(EV_ABS, ABS_MT_SLOT, 1)));
        assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
    }

    /// tracking id 在声明的 `0..=TRACKING_MAX` 里回绕，不会是 -1，也不会撞上
    /// 还按着的触点。
    #[test]
    fn tracking_ids_wrap_within_the_declared_range() {
        let mut contacts = Contacts::default();
        contacts.tracking = TRACKING_MAX - 1;
        contacts
            .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
            .unwrap();
        assert_eq!(contacts.active[&1].tracking, TRACKING_MAX);
        // 1 一直按着；下一个回绕到 0
        let frames = contacts
            .step(
                2,
                &[contact(1, 0.5, 0.5), contact(2, 0.6, 0.6)],
                Rotation::Normal,
            )
            .unwrap();
        assert!(frames[0].contains(&(EV_ABS, ABS_MT_TRACKING_ID, 0)));
        assert_eq!(contacts.active[&2].tracking, 0);

        // 又绕一圈回到 TRACKING_MAX 和 0 时，它们还被 1 和 2 占着，跳过去
        contacts.tracking = TRACKING_MAX - 1;
        contacts
            .step(
                2,
                &[
                    contact(1, 0.5, 0.5),
                    contact(2, 0.6, 0.6),
                    contact(3, 0.7, 0.7),
                ],
                Rotation::Normal,
            )
            .unwrap();
        assert_eq!(contacts.active[&1].tracking, TRACKING_MAX);
        assert_eq!(contacts.active[&3].tracking, 1);
    }

    /// Where a screen position lands on an untransformed panel: on an output
    /// the compositor has turned 90°, the screen's top-left is the panel's
    /// bottom-left, and so on.
    #[test]
    fn a_rotated_output_maps_screen_corners_to_panel_corners() {
        let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        assert_eq!(panel(Rotation::Normal, 0.25, 0.75), (0.25, 0.75));
        assert_eq!(
            corners.map(|(x, y)| panel(Rotation::Rotate90, x, y)),
            [(0.0, 1.0), (0.0, 0.0), (1.0, 1.0), (1.0, 0.0)]
        );
        assert_eq!(
            corners.map(|(x, y)| panel(Rotation::Rotate180, x, y)),
            [(1.0, 1.0), (0.0, 1.0), (1.0, 0.0), (0.0, 0.0)]
        );
        assert_eq!(
            corners.map(|(x, y)| panel(Rotation::Rotate270, x, y)),
            [(1.0, 0.0), (1.0, 1.0), (0.0, 0.0), (0.0, 1.0)]
        );
    }

    /// The compositor's side of the same mapping: a point as a fraction of the
    /// panel, to a point as a fraction of the screen, which is smithay's
    /// `transform_point_in` with the panel's own size as the area. Written out
    /// here so [`panel`] is checked against it rather than against itself.
    fn compositor(rotation: Rotation, panel: (f32, f32)) -> (f32, f32) {
        let (x, y) = panel;
        match rotation {
            Rotation::Normal => (x, y),
            Rotation::Rotate90 => (1.0 - y, x),
            Rotation::Rotate180 => (1.0 - x, 1.0 - y),
            Rotation::Rotate270 => (y, 1.0 - x),
            Rotation::Flipped => (1.0 - x, y),
            Rotation::Flipped90 => (y, x),
            Rotation::Flipped180 => (x, 1.0 - y),
            Rotation::Flipped270 => (1.0 - y, 1.0 - x),
        }
    }

    /// Every turn, including the flipped ones: where the compositor puts the
    /// point this side names for it has to be the point the controller aimed
    /// at in the first place.
    #[test]
    fn a_panel_position_is_the_inverse_of_what_the_compositor_does() {
        let rotations = [
            Rotation::Normal,
            Rotation::Rotate90,
            Rotation::Rotate180,
            Rotation::Rotate270,
            Rotation::Flipped,
            Rotation::Flipped90,
            Rotation::Flipped180,
            Rotation::Flipped270,
        ];
        for rotation in rotations {
            for (x, y) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.25, 0.6)] {
                let point = panel(rotation, x, y);
                let (sx, sy) = compositor(rotation, point);
                assert!(
                    (sx - x).abs() < 1e-6 && (sy - y).abs() < 1e-6,
                    "{rotation:?}: 屏幕 ({x}, {y}) 报成面板 {point:?}，合成器又算回 ({sx}, {sy})"
                );
            }
        }
    }

    /// What libinput hands the compositor (`value × extent / (max − min + 1)`)
    /// and the pixel that position falls on.
    fn landed(value: i32, origin: i32, extent: u32) -> i32 {
        let position = f64::from(value) * f64::from(extent) / (f64::from(AXIS_MAX) + 1.0);
        origin + position.floor() as i32
    }

    #[test]
    fn every_pixel_maps_back_onto_itself() {
        for (origin, extent) in [(0, 1), (0, 1000), (-1920, 4793), (1000, 3793), (0, 65536)] {
            for position in origin..origin + extent as i32 {
                let value = axis_value(position, origin, extent);
                assert!((0..=AXIS_MAX).contains(&value));
                assert_eq!(landed(value, origin, extent), position, "{origin} {extent}");
            }
        }
    }

    #[test]
    fn positions_outside_the_bounds_stay_on_its_edge() {
        assert_eq!(
            axis_value(-5000, -1920, 4793),
            axis_value(-1920, -1920, 4793)
        );
        assert_eq!(axis_value(9000, -1920, 4793), axis_value(2872, -1920, 4793));
        assert_eq!(axis_value(10, 0, 0), 0);
    }

    #[test]
    fn a_repeated_value_is_nudged_within_the_axis() {
        assert_eq!(nudge((10, 20), (11, 20)), None);
        assert_eq!(nudge((10, 20), (10, 21)), None);
        assert_eq!(nudge((10, 20), (10, 20)), Some((11, 20)));
        assert_eq!(nudge((0, 0), (0, 0)), Some((1, 0)));
        assert_eq!(nudge((AXIS_MAX, 5), (AXIS_MAX, 5)), Some((AXIS_MAX - 1, 5)));
    }

    #[test]
    fn bounds_cover_every_output_named_or_not() {
        let output = |name: Option<&str>, left, top, width, height| CompositorOutput {
            name: name.map(Into::into),
            left,
            top,
            width,
            height,
            rotation: Rotation::default(),
        };
        let layout = CompositorLayout {
            outputs: vec![
                output(Some("DP-1"), 0, 0, 2560, 1440),
                output(None, -1080, -480, 1080, 1920),
                output(Some("HDMI-A-1"), 2560, 200, 1280, 1024),
                output(Some("broken"), 9000, 9000, 0, 0),
            ],
        };
        assert_eq!(
            layout_bounds(&layout),
            Some(Desktop {
                left: -1080,
                top: -480,
                width: 4920,
                height: 1920
            })
        );
        assert_eq!(layout_bounds(&CompositorLayout::default()), None);
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
