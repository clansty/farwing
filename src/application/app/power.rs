use super::*;
use crate::account::power::{PowerAction, PowerReceipt};

pub(super) struct AcceptedPower {
    pub device: DeviceInfo,
    pub action: PowerAction,
    pub receipt: PowerReceipt,
}

pub(super) struct PowerProgress {
    pub device: DeviceInfo,
    pub action: PowerAction,
    pub message: String,
    pub waiting: bool,
    /// 用户勾了「上线后自动连接」，观察到重新上线就要替他发起远程连接。
    auto_connect: bool,
    started: Instant,
    saw_offline: bool,
}

pub(super) struct PendingPower {
    pub id: String,
    pub action: PowerAction,
    pub saw_offline: bool,
}

impl PowerProgress {
    /// 返回 true 表示设备刚刚回到在线，且用户约好了这一轮要自动连接（只会返回一次）。
    fn observe(&mut self, devices: &DeviceList) -> bool {
        if !self.waiting {
            return false;
        }
        let Some(device) = devices
            .my_binded_devices
            .iter()
            .find(|d| d.device_id == self.device.device_id)
        else {
            return false;
        };
        self.saw_offline |= device.status == "DISCONNECTED";
        let observed = match self.action {
            PowerAction::Wake => device.is_connected(),
            PowerAction::Shutdown => device.status == "DISCONNECTED",
            PowerAction::Reboot => self.saw_offline && device.is_connected(),
        };
        if observed {
            self.waiting = false;
            self.message = match self.action {
                PowerAction::Wake => "设备已上线",
                PowerAction::Shutdown => "设备已离线",
                PowerAction::Reboot => "已观察到设备离线后重新上线",
            }
            .into();
            let auto_connect = self.auto_connect && self.action == PowerAction::Reboot;
            self.auto_connect = false;
            return auto_connect;
        }
        false
    }

    pub fn elapsed(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

impl DeviceCenterApp {
    pub(super) fn power_available(&self, device: &DeviceInfo, action: PowerAction) -> Result<()> {
        if self.mutation_pending || self.logout_pending || self.login_running {
            anyhow::bail!("正在处理账号或设备操作");
        }
        if self
            .power_progress
            .get(&device.device_id)
            .is_some_and(|p| p.waiting)
        {
            anyhow::bail!("正在等待该设备的电源操作结果");
        }
        let catalog = self.catalog.as_ref().context("等待完整设备清单")?;
        if device.device_id == catalog.groups.current_device_id
            || !catalog
                .groups
                .desktop_devices
                .iter()
                .any(|d| d.device_id == device.device_id)
        {
            anyhow::bail!("仅支持本账号绑定的远端电脑");
        }
        action.check(device, &catalog.features)
    }

    pub(super) fn accept_power(&mut self, accepted: AcceptedPower) {
        let saw_offline = self.pending_power.take().is_some_and(|p| {
            p.id == accepted.device.device_id && p.action == accepted.action && p.saw_offline
        });
        let auto_connect = self
            .power_auto_connect
            .take()
            .is_some_and(|id| id == accepted.device.device_id);
        let message = accepted.receipt.summary(accepted.action);
        self.power_progress.insert(
            accepted.device.device_id.clone(),
            PowerProgress {
                device: accepted.device,
                action: accepted.action,
                message,
                waiting: true,
                auto_connect,
                started: Instant::now(),
                saw_offline,
            },
        );
        // A device push may arrive before the HTTP acknowledgement.
        if let Some(devices) = &self.devices {
            for progress in self.power_progress.values_mut() {
                progress.observe(devices);
            }
        }
    }

    /// 收集这一轮设备清单里「刚刚上线且约好了自动连接」的设备。
    pub(super) fn observe_power(&mut self, devices: &DeviceList) -> Vec<String> {
        if let Some(pending) = &mut self.pending_power {
            pending.saw_offline |= devices
                .my_binded_devices
                .iter()
                .any(|d| d.device_id == pending.id && d.status == "DISCONNECTED");
        }
        let mut back_online = Vec::new();
        for progress in self.power_progress.values_mut() {
            if progress.observe(devices) {
                back_online.push(progress.device.device_id.clone());
            }
        }
        back_online
    }

    /// 重启完成后替用户发起连接；设备清单刚更新过，选中项可以安全改写。
    pub(super) fn auto_connect_after_power(&mut self, back_online: &[String]) {
        for id in back_online {
            self.selected_device_id = Some(id.clone());
            self.start_viewer_mode(false);
            // 接管确认一次只能问一台，剩下的等这一轮处理完再说。
            if self.takeover_confirmation.is_some() {
                break;
            }
        }
    }

    pub(super) fn tick_power(&mut self) {
        // G F50210 only advances local waiting/timeout state. Device-list
        // updates drive observe_power; waiting must not add HTTP polling.
        for progress in self.power_progress.values_mut() {
            if progress.waiting && progress.elapsed() >= 120 {
                progress.waiting = false;
                // 等待都超时了，别再惦记着替他连。
                progress.auto_connect = false;
                progress.message = match progress.action {
                    PowerAction::Wake => "等待超时，尚未确认上线；请检查远端电源及唤醒条件",
                    PowerAction::Shutdown => "等待超时，尚未确认设备离线；请检查远端状态",
                    PowerAction::Reboot => "尚未确认离线再上线，不能据此判定重启成功或失败",
                }
                .into();
            }
        }
    }
}
