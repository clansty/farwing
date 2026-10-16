//! Persistent defaults for this user's controller; no account secrets.
use super::ConnectionMediaOptions;
use anyhow::{Context, Result};
use std::io::Write;
fn path() -> Result<std::path::PathBuf> {
    Ok(crate::platform::paths::require_local_app_data()?
        .join("OpenUUYC")
        .join("media-preferences.json"))
}
pub fn load() -> Result<ConnectionMediaOptions> {
    load_from(&path()?)
}
/// 解码端零拷贝的进程内开关。图形界面的复选框与偏好文件走 [`set_decoder_zero_copy`]；
/// `OPENUUYC_DECODER_ZEROCOPY` 环境变量优先，用来排查（`0`/`false` 关，`1`/`true` 开）。
/// 解码器那边发现 surface 导不出来时会自己退回 CPU 拷贝，跟这个开关无关。
static DECODER_ZERO_COPY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub(crate) fn decoder_zero_copy() -> bool {
    static OVERRIDE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    match OVERRIDE.get_or_init(|| match std::env::var("OPENUUYC_DECODER_ZEROCOPY").as_deref() {
        Ok("0") | Ok("false") => Some(false),
        Ok("1") | Ok("true") => Some(true),
        _ => None,
    }) {
        Some(forced) => *forced,
        None => DECODER_ZERO_COPY.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// 图形界面改了偏好之后立刻生效（下一次打开解码器就读到新值）。
/// `pub` 是因为二进制（`main.rs`）在另一个 crate 里，读设置时也要同步一次。
pub fn set_decoder_zero_copy(enabled: bool) {
    DECODER_ZERO_COPY.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn load_from(path: &std::path::Path) -> Result<ConnectionMediaOptions> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes).context("读取编解码首选设置失败")
}
pub(crate) fn save(options: ConnectionMediaOptions) -> Result<()> {
    save_to(&path()?, options)
}
pub(super) fn save_to(path: &std::path::Path, options: ConnectionMediaOptions) -> Result<()> {
    options.validate()?;
    let parent = path.parent().context("媒体设置目录无效")?;
    std::fs::create_dir_all(parent)?;
    let options = ConnectionMediaOptions {
        audio_only: false,
        muted: false,
        ..options
    };
    let temp = parent.join(format!(".media-preferences-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(&options)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.context("保存编解码首选设置失败")
}
