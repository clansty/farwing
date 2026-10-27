use super::*;
use crate::account::power::PowerAction;

pub(super) struct PowerConfirmation {
    device: DeviceInfo,
    action: PowerAction,
    generation: u64,
    /// 只有重启会用到：重新上线后自动发起远程连接。
    auto_connect: bool,
}

impl DeviceCenterApp {
    pub(super) fn begin_power(&mut self, device: DeviceInfo, action: PowerAction) {
        if let Err(error) = self.power_available(&device, action) {
            self.status = StatusMessage::warning(error.to_string());
            return;
        }
        self.center_ui.power = Some(PowerConfirmation {
            device,
            action,
            generation: self.login_generation,
            auto_connect: false,
        });
    }

    pub(super) fn power_confirmation(&mut self, ctx: &egui::Context) {
        let Some(mut confirm) = self.center_ui.power.take() else {
            return;
        };
        if confirm.generation != self.login_generation || self.logout_pending {
            return;
        }
        let mut commit = false;
        let mut cancel = false;
        let issue = self.power_available(&confirm.device, confirm.action).err();
        let response = egui::Modal::new(egui::Id::new("device-power-confirmation"))
            .frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(450.0_f32.min(ctx.content_rect().width() - 60.0));
                cancel = crate::ui::controls::dialog_header(ui, &format!("{}这台设备？", confirm.action.label()), crate::ui::controls::DialogIcon::Warning, true);
                ui.add(egui::Label::new(RichText::new(display_alias(&confirm.device)).strong()).wrap());
                ui.label(RichText::new(&confirm.device.device_id).monospace().color(MUTED));
                ui.label(format!("{} · {}", confirm.device.platform_label(), confirm.device.status_label()));
                ui.add_space(12.0);
                match confirm.action {
                    PowerAction::Wake => { ui.label("发送远程唤醒请求，随后等待设备上线。需要远端已配置网络唤醒。"); }
                    PowerAction::Shutdown | PowerAction::Reboot => {
                        ui.colored_label(AMBER, "请确保已保存远端工作；未保存的应用可能被强制关闭，所有远控连接将断开。");
                        if confirm.device.participant_count() > 0 {
                            ui.label(format!("设备当前有 {} 个远控参与者。", confirm.device.participant_count()));
                        }
                        if self.viewer_for_device(&confirm.device.device_id).is_some() {
                            ui.label("本程序会先结束当前观看并释放控制按键，再发送电源请求。");
                        }
                    }
                }
                if confirm.action == PowerAction::Reboot {
                    ui.add_space(10.0);
                    ui.checkbox(&mut confirm.auto_connect, "上线后自动连接");
                    ui.label(RichText::new("观察到设备离线后重新上线，自动发起远程连接。").small().color(MUTED));
                }
                if let Some(issue) = &issue { ui.colored_label(AMBER, issue.to_string()); }
                let (accept, dismiss) = crate::ui::controls::dialog_actions(ui, Some(crate::ui::controls::DialogAction::new(&format!("确认{}", confirm.action.label())).enabled(issue.is_none()).danger(confirm.action != PowerAction::Wake)), Some("取消"));
                commit = accept;
                cancel |= dismiss;
            });
        if commit {
            // 先记下「上线后自动连接」的约定，交不出去（队列满了/后台没了）就作废。
            self.power_auto_connect = confirm
                .auto_connect
                .then(|| confirm.device.device_id.clone());
            self.queue_mutation(DeviceMutation::Power {
                device: confirm.device,
                action: confirm.action,
            });
            if !self.mutation_pending {
                self.power_auto_connect = None;
            }
        } else if !cancel && !response.should_close() {
            self.center_ui.power = Some(confirm);
        }
    }

    pub(super) fn power_results(&mut self, ui: &mut egui::Ui) {
        let mut dismiss = Vec::new();
        for (id, progress) in &self.power_progress {
            let message = if progress.waiting {
                format!(
                    "{} · {}\n已等待 {} 秒",
                    display_alias(&progress.device),
                    progress.message,
                    progress.elapsed()
                )
            } else {
                format!("{} · {}", display_alias(&progress.device), progress.message)
            };
            if crate::ui::controls::observe_notice_action(
                ui.ctx(),
                ("device-power-result", id),
                "设备电源操作",
                if progress.waiting {
                    crate::ui::controls::DialogIcon::Waiting
                } else {
                    crate::ui::controls::DialogIcon::Info
                },
                Some(&message),
                if progress.waiting {
                    "停止等待"
                } else {
                    "知道了"
                },
            )
            .is_some()
            {
                dismiss.push(id.clone());
            }
        }
        for id in dismiss {
            self.power_progress.remove(&id);
            crate::ui::controls::clear_notice(ui.ctx(), ("device-power-result", &id));
        }
    }
}
