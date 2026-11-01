//! winit 报上来的触摸屏触点，各个 presenter 共用。
//!
//! 每个 presenter 都在自己的全屏控件之后、egui 之前拦下 `WindowEvent::Touch`，
//! 按映射指针的同一套办法把触点落到画面上，然后交给这里。发原生帧还是模拟按键，
//! 由 `StreamControlHandle::touch` 决定。

use crate::features::remote_input::{Phase, Spot};
use crate::features::stream_control::StreamControlHandle;

/// 转发一个触点。`spot` 是它落在画面上的位置，落在画面外或本地界面正持有输入
/// 时是 `None`。远端输入吃掉它时返回 true；返回 false 表示留给 egui。
pub(super) fn forward(
    control: &StreamControlHandle,
    owner: u64,
    touch: &winit::event::Touch,
    spot: Option<Spot>,
) -> bool {
    let phase = match touch.phase {
        winit::event::TouchPhase::Started => Phase::Start,
        winit::event::TouchPhase::Moved => Phase::Move,
        winit::event::TouchPhase::Ended => Phase::End,
        winit::event::TouchPhase::Cancelled => Phase::Cancel,
    };
    // winit 的 id 是平台的指针、触摸或槽位 id，都是 32 位。
    let id = u32::try_from(touch.id).unwrap_or(u32::MAX);
    let pressure = touch.force.map_or(0.0, |force| force.normalized());
    control.touch(owner, phase, id, pressure, spot)
}
