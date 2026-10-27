//! Remote input and cursor policy transitions.
use super::settings::encode_capture_setting;
use super::{
    OutgoingControlMessage, PbMessageSource, StreamControlHandle, StreamControlProtocol,
    StreamControlState, ensure_ready, lock, protocol,
};
use crate::features::remote_cursor::RemoteCursor;
use crate::features::remote_input::MouseMode;
use anyhow::{Result, anyhow, bail};
use std::time::Instant;

impl StreamControlHandle {
    pub(crate) fn set_mouse_transport_ready(&self, connected: bool) {
        let mut state = lock(&self.shared);
        state.mouse_transport_connected = connected;
        if !connected {
            state.restore_input_pending = state.device_preferences.control_enabled
                && state.viewing_enabled
                && !state.annotation.enabled
                && !state.annotation.toggling();
            self.microphone.disconnect();
            self.clipboard.suspend();
            state.annotation.disconnect();
            state.peer_mouse_relative = None;
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            // Revoke actual input now. A focused, ready viewer may later restore
            // the separately saved intent; held keys/clicks are never replayed.
            state.baseline.cursor_capture = true;
            state.initial_capture_sync_sent = false;
            if let Some((sequence, _, _)) = state.cursor_pending {
                fail_cursor_request(&mut state, sequence, "鼠标连接已中断".into());
                state
                    .pending_sequences
                    .retain(|pending| *pending != sequence);
                state
                    .pending_capture_preferences
                    .retain(|pending| pending.sequence != sequence);
            }
            state.mouse.set_ready(false);
        } else {
            state.mouse.set_ready(
                protocol(&state) == StreamControlProtocol::CaptureSetting
                    && state.control_channel_open
                    && state.text_channel_open,
            );
            self.maybe_send_initial_capture_sync(&mut state);
        }
    }

    pub(crate) fn mouse_screen(&self, track: i32) -> Option<(i32, u32, u32)> {
        let s = lock(&self.shared);
        let screen = s
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track && screen.id >= 0)?;
        Some((screen.id, screen.width, screen.height))
    }

    pub(crate) fn take_mouse_restore_point(&self, track: i32) -> Option<[f64; 2]> {
        let mut s = lock(&self.shared);
        let screen = s
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track)?
            .id;
        if s.mouse.mode() == MouseMode::Smart
            && s.mouse_restore_point.is_some_and(|(id, _)| id == screen)
        {
            s.mouse_restore_point.take().map(|(_, point)| point)
        } else {
            None
        }
    }

    pub fn remote_cursor(&self) -> Option<RemoteCursor> {
        self.cursor.snapshot()
    }

    pub(crate) fn remote_cursor_hidden(&self) -> bool {
        self.cursor.hidden()
    }

    /// Whether the host is drawing its own pointer into the captured frames.
    /// While it is not, nothing but this client can put a pointer on screen.
    /// The Linux player draws the remote pointer itself and asks; the Windows
    /// player shows it as the system cursor, which needs no such check.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn remote_cursor_captured(&self) -> bool {
        lock(&self.shared).baseline.cursor_capture
    }

    pub fn set_mouse_mode(&self, mode: MouseMode) -> Result<()> {
        self.change_mouse_mode(mode, true)
    }

    pub(crate) fn suspend_mouse_control(&self) -> Result<()> {
        self.change_mouse_mode(MouseMode::View, false)
    }

    pub(crate) fn reconcile_mouse_failure(&self) {
        // RemoteInput already released the failed activation. Update cursor and
        // clipboard policy without canceling restoration armed by a concurrent
        // transport loss. Transient send recovery is owned by RemoteInput.
        let mut state = lock(&self.shared);
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(&mut state);
    }

    fn change_mouse_mode(&self, mode: MouseMode, user_choice: bool) -> Result<()> {
        let mut state = lock(&self.shared);
        expire_cursor_request(&mut state);
        state.mouse_restore_point = None;
        if user_choice {
            // 用户自己挑模式就是要那个模式：「主控端鼠标」得回到绝对定位，别再让
            // 那条「这是游戏」的锁把它按在相对上。
            state.guessed_relative = false;
        }
        if mode == MouseMode::View {
            // Local revocation never waits for remote settings.
            state.mouse.disable();
        } else {
            ensure_ready(&state)?;
            if state.annotation.enabled || state.annotation.toggling() {
                bail!("请先关闭批注，再开启键鼠控制");
            }
            let (relative, _) = mouse_policy(&state, mode);
            state.mouse.enable(mode, relative)?;
        }
        state.restore_input_pending = false;
        if user_choice {
            let previous = state.device_preferences;
            state.device_preferences.control_enabled = mode != MouseMode::View;
            if mode != MouseMode::View {
                state.device_preferences.mouse_mode = mode;
            }
            if state.device_preferences != previous {
                self.publish_device_preferences(&mut state);
            }
        }
        // Explicit choices may retry uncertain cursor capture; newer intent is
        // independent of an earlier cursor request still awaiting its response.
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(&mut state);
        tracing::debug!(?mode, user_choice, "remote input control mode changed");
        drop(state);
        self.mouse.repaint();
        Ok(())
    }

    /// Called only by a focused viewer after its first frame and outside local
    /// menus/transitions. One attempt per restored connection, not a retry loop.
    pub(crate) fn restore_input_control(&self) -> Result<()> {
        let mut state = lock(&self.shared);
        let recovering = state.mouse.transport_recovering();
        if !recovering
            && (!state.restore_input_pending || !state.device_preferences.control_enabled)
        {
            return Ok(());
        }
        if state.annotation.enabled || state.annotation.toggling() {
            state.restore_input_pending = false;
            if recovering {
                state.mouse.disable();
            }
            return Ok(());
        }
        if !state.viewing_enabled
            || !state.mouse_transport_connected
            || ensure_ready(&state).is_err()
        {
            return Ok(());
        }
        let mode = if recovering {
            let Some(mode) = state.mouse.recovered_mode() else {
                return Ok(());
            };
            mode
        } else {
            state.device_preferences.mouse_mode
        };
        state.restore_input_pending = false;
        let (relative, _) = mouse_policy(&state, mode);
        state.mouse.enable(mode, relative)?;
        if recovering {
            tracing::info!("remote input control restored after transport recovery");
        }
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.mouse.repaint();
        Ok(())
    }

    pub(super) fn request_cursor_locked(
        &self,
        state: &mut StreamControlState,
        visible: bool,
    ) -> Result<i64> {
        expire_cursor_request(state);
        ensure_ready(state)?;
        let sequence = state.next_sequence;
        let mut baseline = state.baseline;
        baseline.cursor_capture = visible;
        // BECA40/FD9FF0 update desired capture immediately, independently of
        // ACKs. Later complete snapshots must carry this same current intent.
        state.baseline.cursor_capture = visible;
        let active_protocol = protocol(state);
        let payload = match active_protocol {
            StreamControlProtocol::CaptureSetting => encode_capture_setting(sequence, baseline)?,

            StreamControlProtocol::Negotiating => bail!("PB 特性协商尚未完成"),
            StreamControlProtocol::Unsupported => {
                bail!("对端不支持当前串流协议（需要CaptureSetting RPC）")
            }
        };
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.pending_sequences.push_back(sequence);
        state.cursor_pending = Some((sequence, visible, Instant::now()));
        state.initial_capture_sync_sent = true;
        state.cursor_error = None;
        state.last_error = None;
        state.last_notice = None;
        let result = self.send_locked(
            state,
            OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: active_protocol,
                completion: None,
            },
            Some(
                if visible {
                    "显示远端光标"
                } else {
                    "隐藏远端光标"
                }
                .into(),
            ),
        );
        if let Err(error) = &result {
            fail_cursor_request(state, sequence, error.to_string());
        }
        tracing::info!(sequence, visible, "remote cursor visibility requested");
        result
    }

    /// 本地指针在动，而远端光标此刻是隐藏的。
    ///
    /// 被控端把指针锁进游戏之后就不再发光标（官方报的是 `cursor_type = -1`），
    /// 而它的 `special_game_mouse` 只对账号白名单里的程序发——自己起 JVM 的
    /// Minecraft 就不在里面。所以这里补一条本地推断：**远端光标不见了、人还在动
    /// 鼠标**，那就是要相对位移，三种模式都一样（见 `mouse_policy`）。被控端在
    /// **打字**时也会藏光标，只是那时候人手在键盘上，没有这条位移，误判不会发生。
    ///
    /// 判据和官方一致：**只看「远端光标是不是隐藏」这一个状态量**，没有时长、也没有
    /// 迟滞（官方 4.40 的模式解析里同样只有 `cursor_type == -1` 这一个条件，切来切去
    /// 全靠它对指针的 `SetCursorPos` 复位来抹平）。我们多做的一件事是要求「本地指针
    /// 真的动过」，用来挡被控端打字时藏光标那一下；光标一露出来推断立刻作废，回到
    /// 绝对定位（见 `refresh_mouse_policy`）。
    pub(crate) fn pointer_moved(&self) {
        let mut state = lock(&self.shared);
        if state.guessed_relative
            || state.peer_mouse_relative.is_some()
            // 只看画面/已经用相对位移的两种模式不需要这条推断。
            || matches!(state.mouse.mode(), MouseMode::View | MouseMode::Remote)
            // 用户要求强制本机指针时，这条推断就没有立足点了（见 `force_local_cursor`）。
            || force_local_cursor(&state)
            || !state.viewing_enabled
            || !state.remote_cursor.hidden()
        {
            return;
        }
        state.guessed_relative = true;
        tracing::info!("远端光标隐藏且本地指针在动，按三维游戏处理：切相对鼠标");
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.mouse.repaint();
    }

    pub(super) fn handle_mouse_command(
        &self,
        payload: &[u8],
        source: PbMessageSource,
    ) -> Result<()> {
        if source != PbMessageSource::Control {
            return Ok(());
        }
        anyhow::ensure!(payload.len() <= 16 * 1024, "mouse command too large");
        let value: serde_json::Value = serde_json::from_slice(payload)?;
        if let Some(vibration) = crate::protocol::gamepad::GamepadVibration::parse(&value)? {
            self.mouse().receive_gamepad_vibration(vibration);
            return Ok(());
        }
        if value.get("action").and_then(|v| v.as_str()) != Some("special_game_mouse") {
            return Ok(());
        }
        let mode = value
            .get("mouse_mode")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("invalid mouse mode"))?;
        let force = value
            .get("force_mode")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow!("invalid force mode"))?;
        let x = value.get("coordinate_x_scale");
        let y = value.get("coordinate_y_scale");
        let (relative, restore) = match mode {
            0 | 1 => {
                anyhow::ensure!(
                    force && x.is_none() && y.is_none(),
                    "invalid forced mouse command"
                );
                (Some(mode == 1), None)
            }
            2 => {
                anyhow::ensure!(
                    !force || (x.is_none() && y.is_none()),
                    "invalid mouse restore command"
                );
                let restore = match (x, y) {
                    (None, None) => None,
                    (Some(x), Some(y)) => {
                        let x = x
                            .as_f64()
                            .ok_or_else(|| anyhow!("invalid mouse restore x"))?;
                        let y = y
                            .as_f64()
                            .ok_or_else(|| anyhow!("invalid mouse restore y"))?;
                        anyhow::ensure!(
                            x.is_finite()
                                && y.is_finite()
                                && (0.0..=1.0).contains(&x)
                                && (0.0..=1.0).contains(&y),
                            "mouse restore outside screen"
                        );
                        Some([x, y])
                    }
                    _ => bail!("mouse restore coordinates must appear together"),
                };
                (None, restore)
            }
            _ => bail!("unknown mouse mode"),
        };
        let mut state = lock(&self.shared);
        state.peer_mouse_relative = relative;
        state.mouse_restore_point = if smart_mouse_requested(&state) {
            restore.and_then(|point| state.remote_display.map(|screen| (screen.screen_id, point)))
        } else {
            None
        };
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.mouse.repaint();
        Ok(())
    }

    pub(super) fn refresh_mouse_policy(&self, state: &mut StreamControlState) {
        // 远端光标一露出来就作废这条推断，回到绝对定位——和官方一致：官方那边的
        // 判据只有 `cursor_type == -1` 这一个状态量，没有时长、没有迟滞。被控端开口
        // （`peer_mouse_relative`）当然也以它为准，退出控制时同样清掉。
        if state.guessed_relative
            && (!state.remote_cursor.hidden()
                || state.peer_mouse_relative.is_some()
                || state.mouse.mode() == MouseMode::View)
        {
            state.guessed_relative = false;
        }
        let mode = state.mouse.policy_mode();
        if !state.viewing_enabled || mode == MouseMode::View || self.microphone.needs_cleanup() {
            self.disable_microphone_locked(state);
        }
        let clipboard_ready = state.viewing_enabled
            && state.pb_connected
            && state.control_channel_open
            && state.text_channel_open
            && state.mouse_transport_connected
            && state.peer_clipboard >= 1
            && mode != MouseMode::View;
        // Clipboard sync is gated on seven separate conditions, and a session
        // that never syncs looks identical whichever one is missing.
        if clipboard_ready != state.clipboard_ready_reported {
            state.clipboard_ready_reported = clipboard_ready;
            tracing::debug!(
                ready = clipboard_ready,
                viewing = state.viewing_enabled,
                pb = state.pb_connected,
                control = state.control_channel_open,
                text = state.text_channel_open,
                mouse_transport = state.mouse_transport_connected,
                peer = state.peer_clipboard,
                mode = ?mode,
                files_allowed = state.clipboard_files_allowed,
                "剪贴板同步条件"
            );
        }
        self.clipboard.policy(
            clipboard_ready,
            state.peer_clipboard >= 2 && state.clipboard_files_allowed,
        );
        self.drag_drop.enable(
            state.viewing_enabled
                && state.pb_connected
                && state.text_channel_open
                && state.mouse_transport_connected
                && self.files.allowed(),
        );
        if !state.viewing_enabled {
            return;
        }
        let (relative, wanted) = mouse_policy(state, mode);
        // Relative motion is only correct while this client holds the pointer.
        // A window that could not take it says so, and absolute positioning is
        // then the only honest way to aim, whatever the policy would prefer.
        let relative = relative && state.mouse.relative_available();
        // 每种模式都要跟一遍：三维游戏一来，「主控端鼠标」也得临时改成位移，
        // 游戏一走再回到绝对（见 `mouse_policy`）。
        if state.mouse.relative_mode() != relative {
            state.mouse.set_relative_mode(relative);
        }
        if state.cursor_desired_capture != wanted {
            state.cursor_desired_capture = wanted;
            state.cursor_sync_needed = true;
        }
        if !state.cursor_sync_needed || ensure_ready(state).is_err() {
            return;
        }
        state.cursor_sync_needed = false;
        if wanted == state.baseline.cursor_capture && state.cursor_error.is_none() {
            return;
        }
        // Failure does not revoke input or retry forever. A new policy change
        // or explicit choice is required before submitting again.
        if let Err(error) = self.request_cursor_locked(state, wanted) {
            state.cursor_error = Some(error.to_string());
        }
    }
}

pub(super) fn smart_mouse_requested(state: &StreamControlState) -> bool {
    state.mouse.policy_mode() == MouseMode::Smart
}

/// 「主控端鼠标」+ 强制本机指针。用户挑这一档就是在说：远端的光标状态量不可信，
/// 本机指针一直显示。下面那条「远端隐藏 + 本机指针在动 = 三维游戏」的推断唯一的
/// 证据就是那个状态量，所以这里一并把它关掉，否则推断会把鼠标切成位移、连本机
/// 指针也一起藏掉——正好是用户要躲的那个「画面里一个指针都没有」。
pub(super) fn force_local_cursor(state: &StreamControlState) -> bool {
    state.device_preferences.force_local_cursor
        && state.device_preferences.mouse_mode == MouseMode::Local
}

/// 这一局是不是「三维游戏把指针拿走了」。被控端明确表态时以它为准（官方按程序
/// 白名单发的 `special_game_mouse`），它不说话时才用本地的推断。
fn game_took_pointer(state: &StreamControlState) -> bool {
    state.peer_mouse_relative.unwrap_or(state.guessed_relative)
}

/// 这一局该怎么注入：第一个是「发相对位移吗」，第二个是「让被控端把它的指针
/// 画进画面吗」。
pub(super) fn mouse_policy(state: &StreamControlState, mode: MouseMode) -> (bool, bool) {
    let game = game_took_pointer(state);
    // 谁画指针：绝对定位时本机指针就代表远端指针，不用被控端画。推断出来的那一档
    // 不去动这个开关（官方在同样情形下也不 claim capture：那时光标本来就藏了，
    // 没什么可画；一露出来我们就回绝对定位、自己画了）。
    let wanted = match mode {
        MouseMode::View | MouseMode::Remote => true,
        MouseMode::Local => false,
        MouseMode::Smart => state.peer_mouse_relative.unwrap_or(false),
    };
    // 指针一旦被前台程序锁走，绝对定位一项也写不进去：锁定中的指针只接受相对位移。
    // 所以只要这一局是游戏，三种模式都发位移——「主控端鼠标」也一样，代价是本机
    // 不再有指针可看，只能看被控端画进画面的那个。
    let relative = match mode {
        MouseMode::View => false,
        MouseMode::Remote => true,
        MouseMode::Local | MouseMode::Smart => game,
    };
    (relative, wanted)
}

pub(super) fn fail_cursor_request(state: &mut StreamControlState, sequence: i64, error: String) {
    if state
        .cursor_pending
        .is_some_and(|(pending, _, _)| pending == sequence)
    {
        state.cursor_pending = None;
        state.cursor_error = Some(error);
    }
}

pub(super) fn expire_cursor_request(state: &mut StreamControlState) {
    if let Some((sequence, _, started)) = state.cursor_pending
        && started.elapsed() >= std::time::Duration::from_secs(10)
    {
        let error = "光标设置确认超时，远端状态未知；请重试".to_owned();
        fail_cursor_request(state, sequence, error.clone());
        state
            .pending_capture_preferences
            .retain(|pending| pending.sequence != sequence);
        state
            .pending_sequences
            .retain(|pending| *pending != sequence);
        state
            .performance
            .fail_stream_switch(sequence, error.clone());
        state.last_error = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> StreamControlHandle {
        let profile = crate::media::ConnectionMediaProfile {
            audio_only: false,
            muted: false,
            local_display: crate::media::LocalDisplayInfo::FALLBACK,
            stream_fps: 60,
            decoder_fps_cap: 60,
            codec: Default::default(),
            decoder: Default::default(),
        };
        StreamControlHandle::new(
            profile,
            crate::diagnostics::performance::PerformanceMonitor::new("test"),
        )
        .0
    }

    /// 被控端点名要相对之后，「主控端鼠标」也得改成位移：三维游戏把指针锁走时绝对
    /// 定位一项也写不进去。游戏一走，它要回到自己的绝对定位。
    #[test]
    fn a_game_scene_forces_relative_in_every_mode() {
        let handle = handle();
        let mut state = lock(&handle.shared);
        state.viewing_enabled = true;
        assert_eq!(mouse_policy(&state, MouseMode::Local), (false, false));
        assert_eq!(mouse_policy(&state, MouseMode::Remote), (true, true));
        assert_eq!(mouse_policy(&state, MouseMode::Smart), (false, false));
        assert_eq!(mouse_policy(&state, MouseMode::View), (false, true));

        state.peer_mouse_relative = Some(true);
        assert_eq!(mouse_policy(&state, MouseMode::Local), (true, false));
        assert_eq!(mouse_policy(&state, MouseMode::Smart), (true, true));

        // 被控端说这个程序要绝对，比本地推断更权威。
        state.peer_mouse_relative = Some(false);
        state.guessed_relative = true;
        assert_eq!(mouse_policy(&state, MouseMode::Local), (false, false));
        assert_eq!(mouse_policy(&state, MouseMode::Smart), (false, false));
    }

    /// 推断出来的那一档不改被控端的光标合成开关：那时光标正藏着，没什么可画，而
    /// 一露出来我们就回绝对定位、由本机画（官方同样不在这一档 claim capture）。
    #[test]
    fn a_guessed_game_leaves_the_host_cursor_capture_alone() {
        let handle = handle();
        let mut state = lock(&handle.shared);
        state.viewing_enabled = true;
        state.guessed_relative = true;
        assert_eq!(mouse_policy(&state, MouseMode::Smart), (true, false));
        assert_eq!(mouse_policy(&state, MouseMode::Local), (true, false));
    }

    /// 远端光标一露出来就放开推断，回到绝对定位——官方也只有 `cursor_type == -1`
    /// 这一个状态量，没有时长和迟滞。
    #[test]
    fn a_visible_cursor_drops_the_guess_immediately() {
        let handle = handle();
        let mut state = lock(&handle.shared);
        state.viewing_enabled = true;
        state.mouse.set_ready(true);
        state.mouse.enable(MouseMode::Smart, false).unwrap();
        state.guessed_relative = true;
        state.remote_cursor.clear();
        handle.refresh_mouse_policy(&mut state);
        assert!(!state.guessed_relative);
        assert_eq!(mouse_policy(&state, MouseMode::Smart), (false, false));
    }

    /// 强制本机指针只属于「主控端鼠标」：别的模式远端本来就该自己画指针，别把
    /// 这条开关也搬过去。
    #[test]
    fn forcing_the_local_cursor_belongs_to_the_local_mode() {
        let handle = handle();
        let mut state = lock(&handle.shared);
        state.device_preferences.force_local_cursor = true;
        state.device_preferences.mouse_mode = MouseMode::Smart;
        assert!(!force_local_cursor(&state));
        state.device_preferences.mouse_mode = MouseMode::Local;
        assert!(force_local_cursor(&state));
        state.device_preferences.force_local_cursor = false;
        assert!(!force_local_cursor(&state));
    }

    /// 强制本机指针的时候不再拿「远端光标隐藏 + 本机指针在动」当游戏信号：那条
    /// 推断会把鼠标切成位移、连本机指针一起藏掉，正是用户要躲的那个「画面里一个
    /// 指针都没有」。
    #[test]
    fn forcing_the_local_cursor_disables_the_hidden_cursor_guess() {
        use crate::features::remote_cursor::encode_shape;
        let handle = handle();
        let mut state = lock(&handle.shared);
        state.viewing_enabled = true;
        state.mouse.set_ready(true);
        state.mouse.enable(MouseMode::Local, false).unwrap();
        state.device_preferences.mouse_mode = MouseMode::Local;
        state.device_preferences.force_local_cursor = true;
        state
            .remote_cursor
            .receive(&encode_shape(None, 0, [0.5, 0.5]))
            .unwrap();
        assert!(state.remote_cursor.hidden());
        drop(state);
        handle.pointer_moved();
        assert!(!lock(&handle.shared).guessed_relative);
    }
}
