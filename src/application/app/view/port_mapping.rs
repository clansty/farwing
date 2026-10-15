use super::*;
impl DeviceCenterApp {
    pub(in crate::application::app) fn open_file_transfer(&mut self, id: String) {
        if self.logout_pending {
            return;
        }
        let Some(device) = self
            .devices
            .as_ref()
            .and_then(|list| list.my_binded_devices.iter().find(|d| d.device_id == id))
            .cloned()
        else {
            return;
        };
        if self
            .worker
            .commands
            .send(GuiCommand::Files {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
    /// 用 FUSE 把远端文件系统挂到本地临时目录（仅 Linux）。
    #[cfg(target_os = "linux")]
    pub(in crate::application::app) fn mount_file_transfer_fuse(&mut self, id: String) {
        if self.logout_pending {
            return;
        }
        let Some(device) = self
            .devices
            .as_ref()
            .and_then(|list| list.my_binded_devices.iter().find(|d| d.device_id == id))
            .cloned()
        else {
            return;
        };
        self.fuse_busy = true;
        if self
            .worker
            .commands
            .send(GuiCommand::FuseMount {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.fuse_busy = false;
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
    /// 结束当前的 FUSE 挂载（仅 Linux）。非模态：点了就直接卸。
    #[cfg(target_os = "linux")]
    pub(in crate::application::app) fn unmount_file_transfer_fuse(&mut self) {
        self.fuse_busy = true;
        if self
            .worker
            .commands
            .send(GuiCommand::FuseUnmount {
                generation: self.login_generation,
            })
            .is_err()
        {
            self.fuse_busy = false;
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
    pub(in crate::application::app) fn open_port_mapping(&mut self, id: String) {
        if self.logout_pending {
            return;
        }
        let Some(device) = self
            .devices
            .as_ref()
            .and_then(|list| list.my_binded_devices.iter().find(|d| d.device_id == id))
            .cloned()
        else {
            return;
        };
        if self
            .worker
            .commands
            .send(GuiCommand::Ports {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
}
