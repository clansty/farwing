//! 终端会话页：列出被控端正在跑的 shell，并接到本机终端里。
//!
//! 会话属于被控角色，手机端 UU 打开「终端」时才有；页面本身不创建会话，
//! 也不随本地终端关闭而结束它们。
use super::*;

impl DeviceCenterApp {
    pub(super) fn terminal_sessions_page(&mut self, ui: &mut egui::Ui) {
        use crate::features::host::terminal;
        let sessions = terminal::local_sessions();
        // 会话可能在另一头被改名、附着或结束，页面每秒自刷新一次。
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        ui.horizontal(|ui| {
            ui.label(RichText::new("终端会话").size(theme::TITLE).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        !sessions.is_empty(),
                        crate::ui::controls::secondary("结束全部"),
                    )
                    .clicked()
                {
                    terminal::end_all();
                }
            });
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "手机端 UU 打开「终端」后，shell 会出现在这里。点「在本地打开」用系统终端接上，\
                 本机键盘与手机端操作的是同一个 shell；关掉本地终端只会断开这一端，会话继续运行。",
            )
            .small()
            .color(MUTED),
        );
        ui.add_space(14.0);
        if sessions.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("现在没有终端会话").strong());
                ui.label(
                    RichText::new("手机端连上本机并打开终端后，会话会出现在这里。")
                        .small()
                        .color(MUTED),
                );
            });
            return;
        }
        for session in sessions {
            let (state, state_color) = match session.state {
                "running" => ("运行中", GREEN),
                "detached" => ("已分离", MUTED),
                _ => ("已退出", RED),
            };
            crate::ui::controls::section_frame()
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&session.name).strong());
                        ui.label(RichText::new(state).small().color(state_color));
                        if session.remote {
                            ui.label(RichText::new("主控已连接").small().color(BLUE));
                        }
                        if session.local {
                            ui.label(RichText::new("本机已接入").small().color(GREEN));
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    session.state != "exited",
                                    crate::ui::controls::secondary("在本地打开"),
                                )
                                .clicked()
                                && let Err(error) = terminal::open_local_terminal(session.handle)
                            {
                                self.status = StatusMessage::error(format!("{error:#}"));
                            }
                            if ui.add(crate::ui::controls::secondary("结束")).clicked()
                                && !terminal::kill_local(session.handle)
                            {
                                self.status = StatusMessage::error("会话已经不存在");
                            }
                        });
                    });
                    let created = stamp(session.created_ms);
                    ui.label(
                        RichText::new(format!(
                            "{} · 编号 {} · 创建于 {created}",
                            session.shell, session.handle
                        ))
                        .small()
                        .color(MUTED),
                    );
                });
            ui.add_space(8.0);
        }
    }
}

/// 毫秒时间戳按本地时区显示，与诊断页的写法一致。
fn stamp(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "未知".into())
}
