//! 本机上报给账号的设备名片：壁纸取自桌面，右上角的程序图标可以不要。
use super::*;
use crate::account::{device_card::Settings, reporting};

#[derive(Default)]
pub(super) struct ViewState {
    settings: Option<Settings>,
    /// 「重新上报」那条常驻管道调用会阻塞，放到线程里等。
    pending: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    error: Option<String>,
}

impl DeviceCenterApp {
    pub(super) fn device_card_settings(&mut self, ui: &mut egui::Ui) {
        let state = &mut self.center_ui.device_card;
        if let Some(rx) = &state.pending {
            match rx.try_recv() {
                Ok(result) => {
                    state.error = result.err();
                    state.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.error = Some("重新上报已中断".into());
                    state.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let mut settings = *state
            .settings
            .get_or_insert_with(|| Settings::load().unwrap_or_default());
        let busy = state.pending.is_some();
        let mut changed = false;
        section(ui, "设备名片");
        form_row(
            ui,
            "叠加程序图标",
            "其他设备上这张壁纸卡片的右上角带 OpenUUYC 图标；关掉后只剩壁纸本身",
            |ui| {
                changed = ui
                    .add_enabled_ui(!busy, |ui| {
                        crate::ui::controls::switch(ui, &mut settings.logo)
                    })
                    .inner
                    .changed();
            },
        );
        if !changed {
            if let Some(error) = &state.error {
                ui.colored_label(RED, error);
            }
            return;
        }
        // 先落盘再认这个值：写不进去时界面下一帧就退回开关原来的位置。
        if let Err(error) = settings.save() {
            state.error = Some(format!("保存设备名片设置失败：{error:#}"));
            return;
        }
        state.settings = Some(settings);
        let (tx, rx) = std::sync::mpsc::channel();
        state.pending = Some(rx);
        state.error = None;
        let context = ui.ctx().clone();
        std::thread::spawn(move || {
            let _ = tx.send(reporting::refresh().map_err(|error| format!("{error:#}")));
            context.request_repaint();
        });
    }
}
