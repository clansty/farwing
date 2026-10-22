//! A diagnostic view of this process's device publisher, mirrored by the resident.
use crate::platform::device_profile::Hardware;
use crate::platform::host_service::resident;
use serde::{Deserialize, Serialize};
use std::sync::{LazyLock, Mutex};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub device_id: String,
    pub hardware: Option<Hardware>,
    pub reported_name: String,
    pub client_id: String,
    pub reported_controllable: bool,
    pub server_details: Vec<(String, String)>,
    pub readback: String,
    pub readback_at: Option<i64>,
    pub registration: String,
    pub registered_at: Option<i64>,
    pub wallpaper: String,
    pub wallpaper_file: String,
    pub wallpaper_digest: String,
    pub wallpaper_url: String,
    pub wallpaper_at: Option<i64>,
}
static STATE: LazyLock<Mutex<Snapshot>> = LazyLock::new(|| Mutex::new(Snapshot::default()));
pub(crate) static REFRESH: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);
pub(crate) fn snapshot() -> Snapshot {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
pub(crate) fn update(action: impl FnOnce(&mut Snapshot)) {
    action(&mut STATE.lock().unwrap_or_else(|e| e.into_inner()));
}
pub(crate) fn replace(value: Snapshot) {
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = value;
}

/// 让上报任务立刻重读本机资料与壁纸。常驻服务持有账号时只有它能上报，
/// 就由它代劳；这个调用会阻塞，别放在界面线程里。
pub(crate) fn refresh() -> anyhow::Result<()> {
    if resident::managed() {
        resident::call(resident::Request::RefreshPublication)?;
    } else {
        REFRESH.notify_one();
    }
    Ok(())
}
