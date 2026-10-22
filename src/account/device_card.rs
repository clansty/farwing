//! 上报给账号的设备名片（壁纸卡片）长什么样。这是本机的呈现偏好，
//! 既不进账号资料也不进被控设置：常驻服务与界面进程各读同一份文件。
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 卡片上叠不叠程序图标。默认叠，与加入这个开关之前的行为一致。
    pub logo: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self { logo: true }
    }
}
impl Settings {
    fn path() -> Result<std::path::PathBuf> {
        Ok(crate::platform::paths::require_local_app_data()?
            .join("OpenUUYC")
            .join("device-card.json"))
    }
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }
    pub fn save(self) -> Result<()> {
        self.save_to(&Self::path()?)
    }
    fn load_from(path: &std::path::Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("读取设备名片设置失败"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("读取设备名片设置失败"),
        }
    }
    fn save_to(self, path: &std::path::Path) -> Result<()> {
        let parent = path.parent().context("设备名片设置目录无效")?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".device-card-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&self)?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.context("保存设备名片设置失败")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("openuuyc-card-{name}-{}", std::process::id()))
    }

    #[test]
    fn missing_file_keeps_the_logo() {
        let path = scratch("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
        assert!(Settings::default().logo);
    }

    #[test]
    fn round_trips_without_logo() {
        let path = scratch("roundtrip");
        let _ = std::fs::remove_file(&path);
        Settings { logo: false }.save_to(&path).unwrap();
        assert_eq!(
            Settings::load_from(&path).unwrap(),
            Settings { logo: false }
        );
        let _ = std::fs::remove_file(&path);
    }
}
