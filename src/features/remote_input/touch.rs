//! 触摸屏转发的状态机。
//!
//! winit 一个事件只报一个触点，而协议要的是整组触点的快照；它的相位映射到协议的
//! `kind` 时有一处不对称：一个手势只有第一根手指用 kind 1 开场，之后的每一次触点
//! 变化都是 kind 2 全量快照，被控端拿它和自己的活动集合做差（新 id 注入 DOWN，
//! 消失的 id 注入 UP）。
//!
//! 被控端接不住的手势改走模拟：官方被控端，或者触点不在被控端当前屏上（触摸帧
//! 里没有屏幕字段）。这种手势由第一根手指驱动左键，跟被控端自己在没有触摸屏时
//! 模拟触摸的做法一致。
use std::collections::BTreeMap;

use crate::features::host::input::wire::{MAX_TOUCH_POINTS, Point};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Start,
    Move,
    End,
    Cancel,
}

/// 画面上的一个触点：远端的屏加上归一化坐标，和指针输入走同一套映射。
pub(crate) type Spot = (i32, f64, f64);

/// 一个触点事件最后变成什么。
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Step {
    /// 本地界面的触点，事件必须原样交给 egui。
    Local,
    /// 吃掉但什么都不发：模拟手势里的后续手指，或者超过被控端上限的触点。
    Held,
    /// 一帧原生触摸。
    Frame(u8, Vec<Point>),
    /// 模拟指针先移到 `at`（已知时），再按下或松开左键（有要求时）。
    Pointer {
        at: Option<Spot>,
        button: Option<bool>,
    },
}

/// 一个被跟踪的触点：归一化坐标加压力。winit 除了 id 只带这两个属性，
/// 线上的半径和角度一律留零。
#[derive(Clone, Copy, PartialEq, Debug)]
struct Contact {
    x: f64,
    y: f64,
    pressure: f64,
}

#[derive(Clone, Default)]
pub(crate) struct Forwarder {
    active: BTreeMap<u32, Contact>,
    /// 模拟手势跟着的那根手指。
    pointer: Option<u32>,
}

impl Forwarder {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.active.is_empty() && self.pointer.is_none()
    }

    /// 把一个 winit 触点事件变成要发出去的东西。`spot` 是触点在画面上的位置，
    /// 落在画面外时是 `None`；`native` 说明这个触点开的手势是否走原生触摸。
    pub(crate) fn step(
        &mut self,
        phase: Phase,
        id: u32,
        pressure: f64,
        spot: Option<Spot>,
        native: bool,
    ) -> Step {
        // 压力越界会让被控端整帧拒收。
        let pressure = if pressure.is_finite() {
            pressure.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self.pointer == Some(id) {
            return match phase {
                // 滑到本地界面上的触点让指针留在原地。
                Phase::Start | Phase::Move => match spot {
                    Some(at) => Step::Pointer {
                        at: Some(at),
                        button: None,
                    },
                    None => Step::Held,
                },
                Phase::End | Phase::Cancel => {
                    self.pointer = None;
                    Step::Pointer {
                        at: spot,
                        button: Some(false),
                    }
                }
            };
        }
        if !self.active.contains_key(&id) {
            // 起手就在本地界面上的触点从不加入手势，没跟踪过的触点移动或抬起也
            // 归本地界面。清空之后（切换模式、失焦）系统仍会把手势尾巴送来：
            // 已经在远端抬起过的触点不能被一次漂移的移动复活。
            let (Phase::Start, Some(at)) = (phase, spot) else {
                return Step::Local;
            };
            if self.pointer.is_some() || self.active.len() >= MAX_TOUCH_POINTS {
                return Step::Held;
            }
            if self.active.is_empty() && !native {
                self.pointer = Some(id);
                return Step::Pointer {
                    at: Some(at),
                    button: Some(true),
                };
            }
            let kind = if self.active.is_empty() { 1 } else { 2 };
            self.active.insert(
                id,
                Contact {
                    x: at.1,
                    y: at.2,
                    pressure,
                },
            );
            return Step::Frame(kind, self.snapshot());
        }
        match phase {
            Phase::Start | Phase::Move => {
                let held = self.active.get_mut(&id).expect("tracked above");
                held.pressure = pressure;
                // 离开画面的手指留在原位，直到它回来或抬起。
                if let Some((_, x, y)) = spot {
                    (held.x, held.y) = (x, y);
                }
                Step::Frame(2, self.snapshot())
            }
            Phase::End => self.lift(id, 3),
            Phase::Cancel => self.lift(id, 4),
        }
    }

    /// 抬起一切，用于所有权边界。返回抬起远端触点的那一帧；模拟指针跟着其他
    /// 按住的键一起松开。
    pub(crate) fn clear(&mut self) -> Option<(u8, Vec<Point>)> {
        self.pointer = None;
        if self.active.is_empty() {
            return None;
        }
        self.active.clear();
        Some((3, Vec::new()))
    }

    fn lift(&mut self, id: u32, last_kind: u8) -> Step {
        self.active.remove(&id);
        if self.active.is_empty() {
            Step::Frame(last_kind, Vec::new())
        } else {
            Step::Frame(2, self.snapshot())
        }
    }

    fn snapshot(&self) -> Vec<Point> {
        self.active
            .iter()
            .map(|(id, contact)| Point {
                id: *id,
                x: contact.x,
                y: contact.y,
                radius_x: 0.0,
                radius_y: 0.0,
                angle: 0.0,
                pressure: contact.pressure,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: u8, contacts: &[(u32, f64, f64)]) -> Step {
        Step::Frame(
            kind,
            contacts
                .iter()
                .map(|(id, x, y)| Point {
                    id: *id,
                    x: *x,
                    y: *y,
                    radius_x: 0.0,
                    radius_y: 0.0,
                    angle: 0.0,
                    pressure: 0.0,
                })
                .collect(),
        )
    }

    fn native(f: &mut Forwarder, phase: Phase, id: u32, spot: Option<(f64, f64)>) -> Step {
        f.step(phase, id, 0.0, spot.map(|(x, y)| (0, x, y)), true)
    }

    #[test]
    fn first_finger_opens_second_joins() {
        let mut f = Forwarder::default();
        assert_eq!(
            native(&mut f, Phase::Start, 2, Some((0.25, 0.5))),
            frame(1, &[(2, 0.25, 0.5)])
        );
        assert_eq!(
            native(&mut f, Phase::Start, 5, Some((0.75, 0.5))),
            frame(2, &[(2, 0.25, 0.5), (5, 0.75, 0.5)])
        );
        assert_eq!(
            native(&mut f, Phase::Move, 2, Some((0.3, 0.6))),
            frame(2, &[(2, 0.3, 0.6), (5, 0.75, 0.5)])
        );
        // 一根手指抬起，另一根还在：发一个不含它的快照。
        assert_eq!(
            native(&mut f, Phase::End, 2, Some((0.3, 0.6))),
            frame(2, &[(5, 0.75, 0.5)])
        );
        // 最后一根抬起时用整组的收尾类型。
        assert_eq!(native(&mut f, Phase::End, 5, None), frame(3, &[]));
        assert!(f.is_empty());
    }

    #[test]
    fn last_cancel_reports_cancel() {
        let mut f = Forwarder::default();
        native(&mut f, Phase::Start, 1, Some((0.5, 0.5)));
        assert_eq!(native(&mut f, Phase::Cancel, 1, None), frame(4, &[]));
    }

    #[test]
    fn local_ui_contacts_never_join() {
        let mut f = Forwarder::default();
        // 起手在画面外：本地界面的，不吃掉。
        assert_eq!(native(&mut f, Phase::Start, 9, None), Step::Local);
        // 它后面的移动和抬起也归本地界面。
        assert_eq!(
            native(&mut f, Phase::Move, 9, Some((0.5, 0.5))),
            Step::Local
        );
        assert_eq!(native(&mut f, Phase::End, 9, None), Step::Local);
    }

    #[test]
    fn tracked_contact_sliding_onto_ui_keeps_last_position() {
        let mut f = Forwarder::default();
        native(&mut f, Phase::Start, 1, Some((0.4, 0.4)));
        assert_eq!(
            native(&mut f, Phase::Move, 1, None),
            frame(2, &[(1, 0.4, 0.4)])
        );
        // 回到画面上继续跟。
        assert_eq!(
            native(&mut f, Phase::Move, 1, Some((0.6, 0.6))),
            frame(2, &[(1, 0.6, 0.6)])
        );
    }

    #[test]
    fn flush_suppresses_the_gesture_tail() {
        let mut f = Forwarder::default();
        native(&mut f, Phase::Start, 1, Some((0.4, 0.4)));
        assert_eq!(f.clear(), Some((3, Vec::new())));
        assert_eq!(f.clear(), None);
        // 清空之后漂移过来的移动不能把触点复活。
        assert_eq!(
            native(&mut f, Phase::Move, 1, Some((0.5, 0.5))),
            Step::Local
        );
        assert_eq!(native(&mut f, Phase::End, 1, None), Step::Local);
    }

    #[test]
    fn contacts_past_the_host_limit_are_held() {
        let mut f = Forwarder::default();
        for id in 0..MAX_TOUCH_POINTS as u32 {
            assert!(matches!(
                native(&mut f, Phase::Start, id, Some((0.5, 0.5))),
                Step::Frame(..)
            ));
        }
        assert_eq!(
            native(&mut f, Phase::Start, 99, Some((0.5, 0.5))),
            Step::Held
        );
        assert_eq!(
            native(&mut f, Phase::Move, 99, Some((0.5, 0.5))),
            Step::Local
        );
    }

    #[test]
    fn pressure_stays_in_the_wire_range() {
        let mut f = Forwarder::default();
        let Step::Frame(_, points) = f.step(Phase::Start, 1, 3.5, Some((0, 0.5, 0.5)), true) else {
            panic!("expected a frame");
        };
        assert_eq!(points[0].pressure, 1.0);
        let Step::Frame(_, points) = f.step(Phase::Move, 1, f64::NAN, Some((0, 0.5, 0.5)), true)
        else {
            panic!("expected a frame");
        };
        assert_eq!(points[0].pressure, 0.0);
    }

    #[test]
    fn emulated_gesture_follows_its_first_contact() {
        let mut f = Forwarder::default();
        assert_eq!(
            f.step(Phase::Start, 1, 0.0, Some((2, 0.1, 0.2)), false),
            Step::Pointer {
                at: Some((2, 0.1, 0.2)),
                button: Some(true)
            }
        );
        // 第二根手指被吃掉但不发出去；它后面的尾巴归界面。
        assert_eq!(
            f.step(Phase::Start, 2, 0.0, Some((2, 0.5, 0.5)), true),
            Step::Held
        );
        assert_eq!(
            f.step(Phase::Move, 1, 0.0, Some((2, 0.3, 0.4)), false),
            Step::Pointer {
                at: Some((2, 0.3, 0.4)),
                button: None
            }
        );
        assert_eq!(f.step(Phase::Move, 1, 0.0, None, false), Step::Held);
        assert_eq!(
            f.step(Phase::End, 1, 0.0, None, false),
            Step::Pointer {
                at: None,
                button: Some(false)
            }
        );
        assert!(f.is_empty());
    }
}
