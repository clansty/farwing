//! Fullscreen toolbar; it floats over the video and never changes its geometry.
//!
//! 与 Windows 的 `windows_presenter/fullscreen.rs` 是同一套行为：进入全屏先展开，
//! 鼠标离开一会儿之后收成顶部的一条细把手，移回顶部重新展开。浮条本身只叠在画面
//! 上，画面与输入的几何都不变。
use crate::ui::{controls, theme};
use std::time::{Duration, Instant};

const AUTO_COLLAPSE: Duration = Duration::from_millis(1500);
const ENTRY_HINT: Duration = Duration::from_secs(7);

#[derive(Default)]
pub(super) struct FullscreenChrome {
    active: bool,
    expanded: bool,
    last_used: Option<Instant>,
    hint_until: Option<Instant>,
    rect: Option<egui::Rect>,
}

impl FullscreenChrome {
    /// 串流设置窗口向左让出的宽度：它默认贴着右上角，全屏时那样会压在浮条上。
    pub(super) fn menu_inset(&self, ctx: &egui::Context) -> f32 {
        if self.active {
            (ctx.content_rect().width() - floating_width(ctx)) / 2.0
        } else {
            0.0
        }
    }

    pub(super) fn sync(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        *self = Self {
            active,
            ..Self::default()
        };
        if active {
            self.expanded = true;
            self.last_used = Some(Instant::now());
            self.hint_until = Some(Instant::now() + ENTRY_HINT);
        }
    }

    /// 指针是否落在浮条上。落在上面时本机界面接管输入，按键不能再发给远端。
    pub(super) fn owns_pointer(&self, ctx: &egui::Context, relative: bool) -> bool {
        // 锁定指针时它停在画面中央动不了，浮条也就点不到；快捷键释放键鼠之后
        // 才谈得上悬停。
        self.active
            && !relative
            && self.rect.is_some_and(|rect| {
                ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| rect.contains(p)))
            })
    }

    /// 画浮条，`draw` 负责展开状态里的内容。
    /// `busy` 期间（菜单打开、切屏、按住鼠标）不自动收起。
    pub(super) fn show(
        &mut self,
        ctx: &egui::Context,
        relative: bool,
        busy: bool,
        hint_suppressed: bool,
        draw: impl FnOnce(&mut egui::Ui),
    ) {
        let now = Instant::now();
        let hovering = self.owns_pointer(ctx, relative);
        if hovering || busy {
            self.last_used = Some(now);
        }
        if self.expanded && !busy && !hovering {
            let elapsed = now.saturating_duration_since(self.last_used.unwrap_or(now));
            if elapsed >= AUTO_COLLAPSE {
                self.expanded = false;
            } else {
                ctx.request_repaint_after(AUTO_COLLAPSE - elapsed);
            }
        }
        let width = floating_width(ctx);
        let surface_width = if self.expanded {
            width + 2.0 // popup frame stroke
        } else {
            theme::VIEWER_FULLSCREEN_HANDLE_HIT_SIZE.x
        };
        // 展开与收起用两个不同的 Area id，各自记住自己的尺寸，切换时不会跳一帧。
        let response = egui::Area::new(egui::Id::new(("viewer-fullscreen-toolbar", self.expanded)))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                ctx.content_rect().center().x - surface_width / 2.0,
                ctx.content_rect().top(),
            ))
            .movable(false)
            .show(ctx, |ui| {
                if self.expanded {
                    egui::Frame::popup(ui.style())
                        .fill(theme::BG)
                        .corner_radius(theme::PANEL_RADIUS)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.set_width(width - 16.0);
                            draw(ui);
                        });
                } else {
                    let (expand, hit_rect) = controls::viewer_fullscreen_handle(ui);
                    // egui 给控件留的命中容差会把顶边这条窄把手撑大，这里按显式
                    // 的矩形判定。
                    let inside =
                        ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| hit_rect.contains(p)));
                    if !relative && inside && (expand.hovered() || expand.clicked()) {
                        self.expanded = true;
                        self.last_used = Some(now);
                        ctx.request_repaint();
                    }
                }
            });
        self.rect = Some(response.response.rect);
        if let Some(until) = self
            .hint_until
            .filter(|until| now < *until && !hint_suppressed)
        {
            show_hint(ctx, response.response.rect.bottom() + 12.0);
            ctx.request_repaint_after(until.saturating_duration_since(now));
        }
    }
}

fn floating_width(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().width() - 24.0).clamp(32.0, theme::VIEWER_FLOATING_WIDTH)
}

fn show_hint(ctx: &egui::Context, top: f32) {
    use crate::application::viewer_shortcuts::{self, Action};
    let fullscreen = viewer_shortcuts::label(Action::Fullscreen);
    let release = viewer_shortcuts::label(Action::ReleaseMouse);
    let mut text = String::from("已进入全屏 · 鼠标移至顶部横条可展开操作栏并退出全屏");
    if fullscreen != "未绑定" {
        text.push_str(&format!("\n退出全屏：{fullscreen}"));
    }
    if release != "未绑定" {
        text.push_str(&format!("\n游戏鼠标锁定时，先按 {release} 释放键鼠"));
    }
    // 只画不接事件：这条提示不能挡住桌面输入，也不能抢焦点。
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("viewer-fullscreen-hint"),
    ));
    let galley = painter.layout(
        text,
        egui::FontId::proportional(theme::SMALL),
        theme::TEXT,
        (ctx.content_rect().width() - 48.0).max(1.0),
    );
    let rect = egui::Rect::from_min_size(
        egui::pos2(
            ctx.content_rect().center().x - galley.size().x / 2.0 - 12.0,
            top,
        ),
        galley.size() + egui::vec2(24.0, 16.0),
    );
    painter.rect_filled(rect, theme::PANEL_RADIUS, theme::VIEWER_SCRIM);
    painter.galley(rect.min + egui::vec2(12.0, 8.0), galley, theme::TEXT);
}
