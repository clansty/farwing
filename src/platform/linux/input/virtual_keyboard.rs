//! Text through `zwp_virtual_keyboard_v1`.
//!
//! uinput carries key codes, and the compositor resolves them against the
//! keymap the local user configured — so the only characters this backend can
//! type with it are the ones that layout produces, which is why a phone's
//! input method (Chinese, and every other composed character) never arrived.
//! This protocol lets the client **bring its own xkb keymap**, so a key code
//! means whatever we say it means.
//!
//! Ported from DoyoDia/openuuyc-linux (`linux-port`), commits 22471b7,
//! 2fe4b64 and 19ba785, where it was written and tested against Hyprland.
//! A compositor that does not serve the protocol simply leaves the uinput
//! path in place.
//!
//! 上机自测走 `input::session_tests::types_text`：
//! `OPENUUYC_TYPE_TEXT=中文abc cargo test --lib -- --ignored types_text`。
//! 它会打进当时聚焦的窗口，先把目标窗口点出来再跑。
use anyhow::{Context, Result};
use std::os::fd::AsFd;
use std::time::Duration;
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as manager, zwp_virtual_keyboard_v1 as keyboard,
};

struct State;

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
wayland_client::delegate_noop!(State: ignore manager::ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore keyboard::ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);

/// The first key code handed to the compositor. The protocol carries **evdev
/// style** codes, and both sides add `KEYCODE_OFFSET` to reach an xkb key
/// code, so the keymap has to declare `FIRST_CODE + KEYCODE_OFFSET`. wtype
/// picks 1 as well.
const FIRST_CODE: u8 = 1;
/// The fixed evdev-to-xkb offset (Hyprland: `updateXkbStateWithKey(key + 8)`).
const KEYCODE_OFFSET: u8 = 8;
/// How many characters one keymap can carry: the protocol allows 248 key
/// codes, and one key per distinct character is plenty for a text message.
const MAX_CHARS: usize = 56;
/// What an input method needs between two keys it has to pass through.
const TEXT_PACE: Duration = Duration::from_millis(4);

pub(super) struct Keyboard {
    /// Held so the queue and the globals registry stay connected.
    connection: Connection,
    globals: GlobalList,
    queue: EventQueue<State>,
}

/// A keymap that binds one key code per distinct character in `text`.
fn keymap_for(text: &str) -> (String, usize) {
    let mut chars: Vec<char> = Vec::new();
    for c in text.chars() {
        if !chars.contains(&c) && chars.len() < MAX_CHARS {
            chars.push(c);
        }
    }
    let mut out = String::from("xkb_keymap {\n  xkb_keycodes \"vk\" {\n");
    out.push_str(&format!("    minimum = {};\n", FIRST_CODE + KEYCODE_OFFSET));
    out.push_str(&format!(
        "    maximum = {};\n",
        FIRST_CODE as usize + KEYCODE_OFFSET as usize + chars.len().saturating_sub(1)
    ));
    for index in 0..chars.len() {
        out.push_str(&format!(
            "    <VK{index:02}> = {};\n",
            FIRST_CODE as usize + KEYCODE_OFFSET as usize + index
        ));
    }
    out.push_str("  };\n  xkb_types \"vk\" { include \"complete\" };\n");
    out.push_str("  xkb_compat \"vk\" { include \"complete\" };\n");
    out.push_str("  xkb_symbols \"vk\" {\n");
    for (index, c) in chars.iter().enumerate() {
        // Latin-1 codepoints are their own keysym; everything above needs the
        // Unicode prefix (0x01000000), which is how 中 is named.
        let point = *c as u32;
        let sym = if point < 0x100 {
            point
        } else {
            0x0100_0000 | point
        };
        out.push_str(&format!("    key <VK{index:02}> {{ [ 0x{sym:08x} ] }};\n"));
    }
    out.push_str("  };\n};\n");
    (out, chars.len())
}

/// The keymap has to reach the compositor as a file descriptor.
fn keymap_fd(bytes: &[u8]) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let name = c"openuuyc-vk";
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    anyhow::ensure!(raw >= 0, "创建 keymap 内存文件失败");
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    use std::io::Write as _;
    file.write_all(bytes).context("写入 keymap 失败")?;
    Ok(std::os::fd::OwnedFd::from(file))
}

impl Keyboard {
    /// Binds the manager up front: a desktop without the protocol is reported
    /// once at startup rather than on the first keystroke.
    pub(super) fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env().context("连接 Wayland 失败")?;
        let (globals, queue): (GlobalList, EventQueue<State>) =
            registry_queue_init(&connection).context("枚举 Wayland 全局对象失败")?;
        {
            let qh = queue.handle();
            let _manager: manager::ZwpVirtualKeyboardManagerV1 = globals
                .bind(&qh, 1..=1, ())
                .context("桌面没有提供 zwp_virtual_keyboard_manager_v1")?;
            let _seat: wl_seat::WlSeat =
                globals.bind(&qh, 1..=8, ()).context("没有 Wayland seat")?;
        }
        queue.flush().context("提交虚拟键盘请求失败")?;
        Ok(Self {
            connection,
            globals,
            queue,
        })
    }

    /// Types `text` on a keyboard object built for it, then throws the object
    /// away.
    ///
    /// **One object per call is not an optimisation we skipped.** Hyprland
    /// only reads the keymap the first time an object receives one, so a
    /// second upload on the same object changes nothing; a phone's input
    /// method sends one character at a time (each its own call), and reusing
    /// the object types the first character's key over and over — `XYZ`
    /// arrives as `XXX`. wtype builds a new object per call for the same
    /// reason.
    pub(super) fn text(&mut self, text: &str, permitted: impl Fn() -> bool) -> Result<()> {
        let (keymap, bound) = keymap_for(text);
        if bound == 0 {
            return Ok(());
        }
        let qh = self.queue.handle();
        let manager: manager::ZwpVirtualKeyboardManagerV1 = self
            .globals
            .bind(&qh, 1..=1, ())
            .context("桌面没有提供 zwp_virtual_keyboard_manager_v1")?;
        let seat: wl_seat::WlSeat = self
            .globals
            .bind(&qh, 1..=8, ())
            .context("没有 Wayland seat")?;
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        // The keymap goes first and has to be in effect before any key is
        // sent, or the compositor reports NO_KEYMAP. xkbcommon wants the
        // string NUL-terminated, so the length counts that byte too.
        let mut bytes = keymap.into_bytes();
        bytes.push(0);
        let fd = keymap_fd(&bytes)?;
        keyboard.keymap(1, fd.as_fd(), bytes.len() as u32);
        self.queue
            .roundtrip(&mut State)
            .context("等待虚拟键盘 keymap 生效失败")?;
        // Whatever modifiers a previous device left behind do not apply here.
        keyboard.modifiers(0, 0, 0, 0);
        let _ = self.queue.roundtrip(&mut State);

        let mut index = 0u8;
        let mut seen: Vec<char> = Vec::new();
        for c in text.chars() {
            if !permitted() {
                break;
            }
            let position = match seen.iter().position(|seen| *seen == c) {
                Some(found) => found as u8,
                None => {
                    if seen.len() >= MAX_CHARS {
                        continue;
                    }
                    seen.push(c);
                    let assigned = index;
                    index += 1;
                    assigned
                }
            };
            // evdev style: both sides add the offset when they look it up.
            let code = u32::from(FIRST_CODE + position);
            keyboard.key(0, code, 1);
            let _ = self.queue.roundtrip(&mut State);
            keyboard.key(0, code, 0);
            let _ = self.queue.roundtrip(&mut State);
            // Input methods drop keys that arrive faster than they are handled.
            std::thread::sleep(TEXT_PACE);
        }
        keyboard.destroy();
        let _ = self.queue.roundtrip(&mut State);
        let _ = &self.connection;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `keymap_for` 里每个字符都要有自己的键码，且键码范围与 `minimum`/
    /// `maximum` 对得上 —— 写错的话合成器只会报一句 NO_KEYMAP，看不出是哪儿。
    #[test]
    fn the_keymap_binds_every_distinct_character() {
        let (keymap, bound) = keymap_for("中文abc中");
        assert_eq!(bound, 5, "中、文、a、b、c 各占一个键，重复的中不再占");
        let first = FIRST_CODE as usize + KEYCODE_OFFSET as usize;
        assert!(keymap.contains(&format!("minimum = {first};")));
        assert!(keymap.contains(&format!("maximum = {};", first + 4)));
        // 中文在 keymap 里是 Unicode keysym（0x01000000 | codepoint）。
        assert!(keymap.contains(&format!("0x{:08x}", 0x0100_0000 | '中' as u32)));
        assert!(keymap.contains("0x00000061"), "a 用自己的码点");
    }
}
