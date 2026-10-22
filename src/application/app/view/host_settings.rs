use super::*;

impl DeviceCenterApp {
    pub(super) fn notification_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, "远程访问通知");
        let mut mode = self.notifications.mode;
        form_row(
            ui,
            "通知方式",
            "连接与文件传输状态；Windows 通知受系统通知和勿扰设置影响",
            |ui| {
                egui::ComboBox::from_id_salt("remote-notification-mode")
                    .width(238.)
                    .selected_text(mode.label())
                    .show_ui(ui, |ui| {
                        for item in super::super::notifications::Mode::ALL {
                            ui.selectable_value(&mut mode, item, item.label());
                        }
                    });
            },
        );
        if mode != self.notifications.mode {
            self.notifications.set_mode(mode);
        }
        if let Some(error) = &self.notifications.error {
            ui.label(egui::RichText::new(error).color(theme::AMBER));
            if ui.button("重试通知").clicked() {
                self.notifications.set_mode(mode);
            }
        }
        if ui
            .add_enabled(
                self.notifications.available(),
                crate::ui::controls::secondary("查看当前通知"),
            )
            .clicked()
        {
            self.notifications.reopen();
        }
    }
    pub(super) fn save_host_settings(&mut self) {
        if self
            .worker
            .commands
            .send(GuiCommand::SaveHostSettings {
                generation: self.login_generation,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设置服务已停止，请重新打开程序后确认被控设置");
        }
    }
    pub(super) fn host_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_enabled_ui(!self.maintenance_busy(), |ui| {
            self.host_settings_controls(ui)
        });
        self.wol_setup_dialog(ui.ctx());
    }
    fn host_access_settings(&mut self, ui: &mut egui::Ui, host: &crate::features::host::Handle) {
        let guest = host.is_guest();
        let mut files = host.file_transfer_allowed();
        let mut ports = host.port_mapping_allowed();
        let mut power = host.power_allowed();
        let mut wol = host.wol_allowed();
        // 只有这台机器真能开 shell 时才允许；否则开关保留但禁用，并给出原因。
        let terminal_refusal = crate::features::host::terminal::unavailable();
        let terminal_supported = terminal_refusal.is_none();
        let mut terminal = host.terminal_allowed();
        let mut terminal_local = host.terminal_local_allowed();
        let mut clipboard = host.clipboard_settings();
        let before = (
            files,
            ports,
            power,
            wol,
            terminal,
            clipboard,
            terminal_local,
        );
        let enabled = usize::from(clipboard.enabled)
            + usize::from(clipboard.enabled && clipboard.files)
            + if guest {
                0
            } else {
                [
                    files,
                    ports,
                    power,
                    wol,
                    terminal && terminal_supported,
                    terminal_local && terminal_supported,
                ]
                .into_iter()
                .filter(|v| *v)
                .count()
            };
        let total = if guest {
            2
        } else {
            6 + 2 * usize::from(terminal_supported)
        };
        let id = egui::Id::new(("host-access-settings", self.login_generation));
        let mut open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
        form_row(
            ui,
            "访问权限",
            &format!("已开启 {enabled} / {total} 项"),
            |ui| {
                if ui
                    .add(crate::ui::controls::secondary(if open {
                        "收起"
                    } else {
                        "展开"
                    }))
                    .clicked()
                {
                    open = !open;
                }
            },
        );
        ui.data_mut(|data| data.insert_temp(id, open));
        if open {
            crate::ui::controls::section_frame()
                .inner_margin(theme::DIALOG_MARGIN)
                .show(ui, |ui| {
                    ui.columns(if guest { 1 } else { 2 }, |columns| {
                        let ui = &mut columns[0];
                        ui.label(RichText::new("文件与剪贴板").strong());
                        if !guest {
                            ui.checkbox(&mut files, "文件浏览与传输");
                        }
                        ui.checkbox(&mut clipboard.enabled, "剪贴板同步");
                        ui.add_enabled_ui(clipboard.enabled, |ui| {
                            ui.checkbox(&mut clipboard.files, "文件复制粘贴");
                        });
                        if !guest {
                            let ui = &mut columns[1];
                            ui.label(RichText::new("系统与网络").strong());
                            ui.checkbox(&mut power, "关机和重启");
                            ui.checkbox(&mut ports, "端口转发");
                            ui.checkbox(&mut wol, "局域网唤醒协助");
                            if let Some(reason) = terminal_refusal {
                                ui.add_enabled(false, egui::Checkbox::new(&mut false, "终端"));
                                ui.label(RichText::new(reason).small().color(MUTED));
                            } else {
                                ui.checkbox(&mut terminal, "终端");
                                ui.label(
                                    RichText::new("主控设备可以以当前用户身份在本机执行任意命令")
                                        .small()
                                        .color(MUTED),
                                );
                                // 终端下的第二道授权：只有终端本身开着时可用，
                                // 终端一关就随之失效。
                                ui.indent("host-terminal-local", |ui| {
                                    let mut shown = terminal_local && before.4;
                                    ui.add_enabled(
                                        before.4 && terminal,
                                        egui::Checkbox::new(&mut shown, "允许主控接入本机终端会话"),
                                    );
                                    if before.4 && terminal {
                                        terminal_local = shown;
                                    }
                                    ui.label(
                                        RichText::new(if before.4 {
                                            "主控能看到并接管你在本机「终端会话」页或命令行里打开的 shell"
                                        } else {
                                            "先允许终端，才能打开这一项"
                                        })
                                        .small()
                                        .color(MUTED),
                                    );
                                });
                                let running = crate::features::host::terminal::running();
                                if running > 0 {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(format!("终端会话：{running} 个运行中"))
                                                .small(),
                                        );
                                        if ui
                                            .add(crate::ui::controls::secondary("结束全部"))
                                            .clicked()
                                        {
                                            crate::features::host::terminal::end_all();
                                        }
                                    });
                                }
                            }
                        }
                    });
                });
            ui.add_space(theme::MENU_GROUP_GAP);
        }
        // A shell is the widest grant on this page: turning it on asks first,
        // and only the confirmation applies it.
        let confirm_id = egui::Id::new(("host-terminal-confirm", self.login_generation));
        if terminal && !before.4 {
            terminal = false;
            ui.data_mut(|data| data.insert_temp(confirm_id, true));
        }
        if ui.data(|data| data.get_temp::<bool>(confirm_id).unwrap_or(false)) {
            match terminal_confirmation(ui.ctx()) {
                Some(true) => {
                    terminal = true;
                    ui.data_mut(|data| data.remove::<bool>(confirm_id));
                }
                Some(false) => ui.data_mut(|data| data.remove::<bool>(confirm_id)),
                None => {}
            }
        }
        // 让主控进本机用户自己的 shell 也要先问一次。
        let local_confirm_id =
            egui::Id::new(("host-terminal-local-confirm", self.login_generation));
        if terminal_local && !before.6 {
            terminal_local = false;
            ui.data_mut(|data| data.insert_temp(local_confirm_id, true));
        }
        if ui.data(|data| data.get_temp::<bool>(local_confirm_id).unwrap_or(false)) {
            match terminal_local_confirmation(ui.ctx()) {
                Some(true) => {
                    terminal_local = true;
                    ui.data_mut(|data| data.remove::<bool>(local_confirm_id));
                }
                Some(false) => ui.data_mut(|data| data.remove::<bool>(local_confirm_id)),
                None => {}
            }
        }
        if !terminal {
            terminal_local = false;
        }
        let results = [
            (files != before.0).then(|| host.set_file_transfer_allowed(files)),
            (ports != before.1).then(|| host.set_port_mapping_allowed(ports)),
            (power != before.2).then(|| host.set_power_allowed(power)),
            (wol != before.3).then(|| host.set_wol_allowed(wol)),
            (terminal != before.4).then(|| host.set_terminal_allowed(terminal)),
            (terminal_local != before.6).then(|| host.set_terminal_local_allowed(terminal_local)),
            (clipboard != before.5).then(|| host.set_clipboard_settings(clipboard)),
        ];
        let mut changed = false;
        for result in results.into_iter().flatten() {
            match result {
                Ok(()) => changed = true,
                Err(error) => self.status = StatusMessage::error(error.to_string()),
            }
        }
        if changed {
            self.save_host_settings();
        }
        let status = host.status();
        if let Some(error) = status.clipboard.error {
            ui.colored_label(RED, error);
        }
        if let Some(message) = status.power_message {
            ui.label(RichText::new(message).small().color(MUTED));
        }
    }
    /// 同账号设备的单独许可：总开关是默认值，这里只记住偏离它的选择。
    fn host_device_policies(&mut self, ui: &mut egui::Ui, host: &crate::features::host::Handle) {
        use crate::features::host::policy::DevicePolicy;
        let Some(devices) = self.devices.as_ref() else {
            return;
        };
        let policies = host.device_policies();
        let own = devices.current_device.device_id.clone();
        // 能当主控的不止「我的设备」：账号里的手机、平板、电视都会来连本机，
        // 设备详情页给出的也是同一份全集，这里不能只列一半。
        let mut seen = std::collections::BTreeSet::new();
        let mut candidates: Vec<(String, String)> = Vec::new();
        {
            let mut push = |device: &DeviceInfo| {
                if device.device_id != own && seen.insert(device.device_id.clone()) {
                    candidates.push((device.device_id.clone(), display_alias(device).to_owned()));
                }
            };
            for (_, device) in all_devices(devices) {
                push(device);
            }
            if let Some(catalog) = self.catalog.as_ref() {
                for (_, device) in catalog.groups.entries() {
                    push(device);
                }
            }
        }
        // 表里可能还留着已经不在账号里的设备，至少让它看得见、取消得掉。
        for device in policies.keys() {
            if seen.insert(device.clone()) {
                candidates.push((device.clone(), format!("已移除的设备 {device}")));
            }
        }
        if candidates.is_empty() {
            return;
        }
        let summary = if policies.is_empty() {
            "全部跟随上面的开关".to_owned()
        } else {
            format!("{} 台单独设置", policies.len())
        };
        let id = egui::Id::new(("host-device-policies", self.login_generation));
        let mut open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
        form_row(ui, "设备权限", &summary, |ui| {
            if ui
                .add(crate::ui::controls::secondary(if open {
                    "收起"
                } else {
                    "展开"
                }))
                .clicked()
            {
                open = !open;
            }
        });
        ui.data_mut(|data| data.insert_temp(id, open));
        // 总开关关着时这里就是白名单：只有被放行的设备能连，本机因此保持在线。
        if !host.allowed() {
            let allowed = policies
                .values()
                .filter(|p| **p == DevicePolicy::Allow)
                .count();
            ui.label(
                RichText::new(if allowed > 0 {
                    format!(
                        "「允许被控」已关闭：只有这 {allowed} 台设备能连本机，本机保持在线可被控"
                    )
                } else {
                    "「允许被控」已关闭：本机不上线，任何设备都连不进来".into()
                })
                .small()
                .color(MUTED),
            );
        }
        if !open {
            return;
        }
        crate::ui::controls::section_frame()
            .inner_margin(theme::DIALOG_MARGIN)
            .show(ui, |ui| {
                for (device, alias) in candidates {
                    let current = policies.get(&device).copied().unwrap_or_default();
                    let mut selected = current;
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(alias).truncate())
                            .on_hover_text(&device);
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            egui::ComboBox::from_id_salt(("host-device-policy", &device))
                                .width(160.0)
                                .selected_text(selected.label())
                                .show_ui(ui, |ui| {
                                    for item in DevicePolicy::ALL {
                                        ui.selectable_value(&mut selected, item, item.label());
                                    }
                                });
                        });
                    });
                    if selected != current {
                        match host.set_device_policy(&device, selected) {
                            Ok(()) => self.save_host_settings(),
                            Err(error) => self.status = StatusMessage::error(error.to_string()),
                        }
                    }
                }
            });
    }
    fn host_settings_controls(&mut self, ui: &mut egui::Ui) {
        section(ui, "被控设置");
        let host = self.host.clone();
        let mut allowed = host.as_ref().is_some_and(|h| h.allowed());
        form_row(
            ui,
            "允许被控",
            "新设备默认是否可以控制本机；关掉后只有下方单独设为「允许」的设备能连",
            |ui| {
                if ui
                    .add_enabled_ui(host.is_some(), |ui| {
                        crate::ui::controls::switch(ui, &mut allowed)
                    })
                    .inner
                    .changed()
                    && let Some(host) = host.as_ref()
                {
                    host.set_allowed(allowed);
                    self.save_host_settings();
                }
            },
        );
        if let Some(host) = host.as_ref() {
            self.host_device_policies(ui, host);
            if host.request_audio_devices()
                && self
                    .worker
                    .commands
                    .send(GuiCommand::RefreshHostAudioDevices {
                        generation: self.login_generation,
                    })
                    .is_err()
            {
                host.audio_devices_failed();
            }
            self.host_access_settings(ui, host);
            if !host.is_guest() {
                self.wol_setup_entry(ui);
            }
            section(ui, "画面与声音");
            let inventory = host.audio_devices();
            let mut quality = host.audio_quality();
            let mut quality_changed = false;
            form_row(
                ui,
                "桌面声音音质",
                "控制本机发送的桌面声音，修改立即生效",
                |ui| {
                    quality_changed = crate::ui::controls::audio_quality(ui, &mut quality);
                },
            );
            if quality_changed {
                match host.set_audio_quality(quality) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            let original_defaults = host.audio_defaults();
            let mut selected_defaults = original_defaults;
            form_row(
                ui,
                "默认音频设备",
                "调整为虚拟设备，断开后恢复；修改立即生效",
                |ui| {
                    egui::ComboBox::from_id_salt("host-audio-defaults")
                        .width(238.0)
                        .selected_text(selected_defaults.label())
                        .show_ui(ui, |ui| {
                            for choice in crate::features::host::audio::DefaultDevices::ALL {
                                ui.selectable_value(&mut selected_defaults, choice, choice.label());
                            }
                        });
                },
            );
            if selected_defaults != original_defaults {
                match host.set_audio_defaults(selected_defaults) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            if let Some(error) = host.status().microphone.routing_error {
                ui.colored_label(RED, error);
            }
            let original_device = host.audio_device();
            let mut selected_device = original_device.clone();
            let selected_name = match original_device.as_ref() {
                None => "跟随系统默认".to_owned(),
                Some(device) => inventory
                    .devices
                    .iter()
                    .find(|d| d.id == device.id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| {
                        if inventory.updated.is_some() && inventory.error.is_none() {
                            format!("{}（未连接）", device.name)
                        } else {
                            device.name.clone()
                        }
                    }),
            };
            form_row(ui, "声音采集设备", "切换后立即生效", |ui| {
                egui::ComboBox::from_id_salt("host-audio-device")
                    .width(238.0)
                    .truncate()
                    .selected_text(&selected_name)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut selected_device, None, "跟随系统默认");
                        for device in &inventory.devices {
                            ui.push_id(&device.id, |ui| {
                                let chosen =
                                    selected_device.as_ref().is_some_and(|d| d.id == device.id);
                                if ui.selectable_label(chosen, &device.name).clicked() {
                                    selected_device = Some(device.clone());
                                }
                            });
                        }
                        if let Some(device) = original_device.as_ref()
                            && !inventory.devices.iter().any(|d| d.id == device.id)
                        {
                            ui.add_enabled(false, egui::Label::new(&selected_name));
                        }
                        if let Some(error) = &inventory.error {
                            ui.colored_label(RED, error);
                        } else if inventory.pending && inventory.updated.is_none() {
                            ui.label("正在读取设备…");
                        } else if inventory.devices.is_empty() {
                            ui.label("暂无可用播放设备");
                        }
                    });
            });
            if selected_device != original_device {
                match host.set_audio_device(selected_device) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            use crate::features::host::{EncoderCodec, EncoderMode, EncodingSettings};
            let original = host.encoding_settings();
            let mut selected = original;
            let capabilities = host.capabilities();
            let available = |settings: EncodingSettings| {
                settings.validate().is_ok()
                    && capabilities
                        .as_ref()
                        .is_none_or(|caps| caps.codecs.iter().any(|cap| settings.accepts(cap)))
            };
            form_row(
                ui,
                "编码格式",
                if selected.codec == EncoderCodec::Automatic {
                    "主控未指定时使用；重连后生效"
                } else {
                    "已固定，可在高级限制中调整"
                },
                |ui| {
                    if selected.codec != EncoderCodec::Automatic {
                        ui.label(selected.codec.label());
                    } else {
                        egui::ComboBox::from_id_salt("host-codec-preferred")
                            .width(238.)
                            .selected_text(selected.preferred_codec.label())
                            .show_ui(ui, |ui| {
                                for codec in crate::media::CodecPreference::PREFERRED {
                                    ui.selectable_value(
                                        &mut selected.preferred_codec,
                                        codec,
                                        codec.label(),
                                    );
                                }
                            });
                    }
                },
            );
            form_row(
                ui,
                "编码器",
                "不可用时自动回退；重连后生效",
                |ui| {
                    if selected.mode == EncoderMode::Software {
                        ui.label("仅软件编码");
                    } else {
                        self.media_inventory
                            .draw(ui, "host-encoder-gpu", &mut selected.gpu, true);
                    }
                },
            );
            form_row(
                ui,
                "GPU 调度",
                if selected.gpu_compatibility {
                    "遇到显卡驱动冻结时使用；重连后生效"
                } else {
                    "改善游戏满载时的串流流畅度；重连后生效"
                },
                |ui| {
                    egui::ComboBox::from_id_salt("host-gpu-scheduling")
                        .width(238.)
                        .selected_text(if selected.gpu_compatibility {
                            "兼容优先"
                        } else {
                            "流畅优先（推荐）"
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut selected.gpu_compatibility,
                                false,
                                "流畅优先（推荐）",
                            );
                            ui.selectable_value(
                                &mut selected.gpu_compatibility,
                                true,
                                "兼容优先",
                            )
                            .on_hover_text("降低 GPU 调度优先级以兼容部分显卡驱动，游戏满载时可能降低串流流畅度。");
                        });
                },
            );
            let restriction = super::super::media_preferences::limits_title(
                selected.mode,
                (selected.codec != EncoderCodec::Automatic).then(|| selected.codec.label()),
            );
            egui::CollapsingHeader::new(restriction)
                .id_salt("host-codec-limits")
                .show(ui, |ui| {
                    form_row(ui, "编码方式", "", |ui| {
                        egui::ComboBox::from_id_salt("host-encoder-mode")
                            .width(238.)
                            .selected_text(selected.mode.label())
                            .show_ui(ui, |ui| {
                                for mode in EncoderMode::ALL {
                                    ui.add_enabled_ui(
                                        available(EncodingSettings { mode, ..selected }),
                                        |ui| {
                                            ui.selectable_value(
                                                &mut selected.mode,
                                                mode,
                                                mode.label(),
                                            );
                                        },
                                    );
                                }
                            });
                    });
                    form_row(ui, "格式限制", "", |ui| {
                        egui::ComboBox::from_id_salt("host-encoder-codec")
                            .width(238.)
                            .selected_text(if selected.codec == EncoderCodec::Automatic {
                                "不限制"
                            } else {
                                selected.codec.label()
                            })
                            .show_ui(ui, |ui| {
                                for codec in EncoderCodec::ALL {
                                    ui.add_enabled_ui(
                                        available(EncodingSettings { codec, ..selected }),
                                        |ui| {
                                            ui.selectable_value(
                                                &mut selected.codec,
                                                codec,
                                                if codec == EncoderCodec::Automatic {
                                                    "不限制"
                                                } else {
                                                    codec.label()
                                                },
                                            );
                                        },
                                    );
                                }
                            });
                    });
                });
            if selected != original {
                match host.set_encoding_settings(selected) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
        }
        let status = host.as_ref().map(|host| host.status()).unwrap_or_default();

        let message = if status.message.is_empty() {
            "已禁止被控"
        } else {
            &status.message
        };
        let message = if status.saving {
            format!("{message} · 正在保存设置…")
        } else {
            message.to_owned()
        };
        let action = if status.session_active {
            Some("断开连接")
        } else if allowed && !status.ready && status.error.is_some() {
            Some("重试")
        } else {
            None
        };
        if crate::ui::controls::status_row(
            ui,
            &message,
            if status.connected { GREEN } else { MUTED },
            action,
        ) && let Some(host) = host
        {
            if status.session_active {
                host.disconnect();
            } else {
                host.retry();
            }
        }
        for (source, error) in [
            ("host-settings-error", status.settings_error),
            ("host-session-error", status.error),
        ] {
            // Service maintenance intentionally stops the resident endpoint;
            // its temporary polling failure is not a settings failure.
            if self.maintenance_busy() {
                crate::ui::controls::clear_notice(ui.ctx(), (source, self.login_generation));
                continue;
            }
            crate::ui::controls::observe_notice(
                ui.ctx(),
                (source, self.login_generation),
                "被控设置",
                crate::ui::controls::DialogIcon::Error,
                error.as_deref(),
            );
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

/// Asks before the controller may run a shell here: `Some(true)` to allow,
/// `Some(false)` to leave it off, `None` while the dialog is still open.
///
/// 「允许主控接入本机终端会话」的确认，形式与 [`terminal_confirmation`] 相同。
fn terminal_local_confirmation(ctx: &egui::Context) -> Option<bool> {
    let mut decision = None;
    let response = egui::Modal::new(egui::Id::new("host-terminal-local-confirm"))
        .frame(crate::ui::controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(410.0);
            if crate::ui::controls::dialog_header(
                ui,
                "允许主控接入本机终端会话？",
                crate::ui::controls::DialogIcon::Warning,
                true,
            ) {
                decision = Some(false);
            }
            ui.label(
                "主控设备能看到并接管你在本机打开的终端会话，包括屏幕内容和之后输入的一切，\
                 也能在这些会话里输入命令、改名或结束它们。只在信任的设备上开启。",
            );
            let (accept, dismiss) = crate::ui::controls::dialog_actions(
                ui,
                Some(crate::ui::controls::DialogAction::new("允许接入").danger(true)),
                Some("取消"),
            );
            if accept {
                decision = Some(true);
            } else if dismiss {
                decision = Some(false);
            }
        });
    if decision.is_none() && response.should_close() {
        decision = Some(false);
    }
    decision
}

fn terminal_confirmation(ctx: &egui::Context) -> Option<bool> {
    let mut decision = None;
    let response = egui::Modal::new(egui::Id::new("host-terminal-confirm"))
        .frame(crate::ui::controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(410.0);
            if crate::ui::controls::dialog_header(
                ui,
                "允许终端？",
                crate::ui::controls::DialogIcon::Warning,
                true,
            ) {
                decision = Some(false);
            }
            ui.label(
                "已授权的主控设备将可以以当前用户身份在本机打开命令行，执行任意命令，\
                 读取和修改这个用户能访问的所有文件。只在信任的设备上开启。",
            );
            let (accept, dismiss) = crate::ui::controls::dialog_actions(
                ui,
                Some(crate::ui::controls::DialogAction::new("允许终端").danger(true)),
                Some("取消"),
            );
            if accept {
                decision = Some(true);
            } else if dismiss {
                decision = Some(false);
            }
        });
    if decision.is_none() && response.should_close() {
        decision = Some(false);
    }
    decision
}
