//! 播放器窗口何时把本机键盘和指针拿过来交给远端。
//!
//! 每个 presenter 在捕获指针、或把桌面自己的快捷键挡开之前都要问同一个问题，
//! 所以答案统一放这里：平台只提供它自己才观察得到的事实（焦点、它自己的菜单、
//! 画面是否到达），egui 知道的那部分从 context 读，作用在结果上的机制由各平台
//! 自己实现。

use crate::features::remote_input::RemoteInput;

/// 门需要的窗口级事实。每个 presenter 从自己的窗口和菜单里填这些；egui 知道的
/// （弹出层、模态、文本框）从 context 读。
#[derive(Clone, Copy, Debug)]
pub(super) struct InputGate {
    /// 这个窗口正是持有键盘的那一个。
    pub focused: bool,
    pub minimized: bool,
    /// 正在控制远端，而不是只观看。
    pub controlling: bool,
    /// 文件拖拽占着指针。
    pub dragging: bool,
    /// presenter 自己的菜单、标签页或转场正开着。
    pub menu_open: bool,
    /// 已经有画面到达，输入才有可瞄准的对象。
    pub has_video: bool,
}

impl InputGate {
    /// 此刻窗口是否持有本机输入。
    pub(super) fn holds(self, context: &egui::Context) -> bool {
        self.focused && self.holds_unless_unfocused(context)
    }

    /// 去掉焦点这一项的版本，给「桌面本来就把机制限定在焦点窗口上」的场合用
    /// （Wayland 的快捷键抑制器）。
    pub(super) fn holds_unless_unfocused(self, context: &egui::Context) -> bool {
        !self.minimized
            && self.controlling
            && !self.dragging
            && !self.menu_open
            && !context.any_popup_open()
            && !context.memory(|memory| memory.top_modal_layer().is_some())
            && !context.text_edit_focused()
            && self.has_video
    }
}

/// 窗口自己拿到的按键是否该挡开本机桌面的快捷键：设备的「拦截快捷键」开关、
/// 远端确实收键盘、且没有快捷键录制器或文本框在抢这些键。
pub(super) fn intercepts_shortcuts(input: &RemoteInput, setting: bool) -> bool {
    setting && input.keyboard_supported() && !crate::application::viewer_shortcuts::suspended()
}
