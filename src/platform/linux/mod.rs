//! Linux OS and GPU backends: VA-API decoding, wgpu presentation, X11 capture
//! and display topology, software encoding for the host role. No
//! account/session ownership in this layer.
pub(crate) mod audio_thread;
pub(crate) mod capture;
pub(crate) mod cuda;
pub(crate) mod cursor_shape;
pub(crate) mod deadline;
pub(crate) mod decoder;
pub(crate) mod device_profile;
pub(crate) mod display;
pub(crate) mod display_hdr;
pub(crate) mod drag_drop;
pub(crate) mod encoder;
pub(crate) mod ffmpeg;
pub(crate) mod file_locations;
pub(crate) mod graphics;
pub(crate) mod host_service;
pub(crate) mod input;
pub(crate) mod loopback;
pub(crate) mod notifications;
pub(crate) mod nvenc;
pub(crate) mod power_request;
mod software_encoder;
pub(crate) mod startup_notify;
pub(crate) mod surface;
pub(crate) mod system_power;
pub(crate) mod transfer;
pub(crate) mod vaapi;
mod video_layer;
pub(crate) mod virtual_audio;
pub(crate) mod virtual_display;
pub(crate) mod wol;

/// FUSE 卷的文件系统子类型，写进挂载表就是 `fuse.<子类型>`。
///
/// 故意不叫 `openuuyc`：GLib 判断一个文件系统算不算「远端」只用一张写死的名单
/// （`is_remote_fs_type()`：nfs、nfs4、cifs、smb、smb2、fuse.sshfs），名单里唯一
/// 能由 FUSE 挂出来的就是 `fuse.sshfs`。而我们的卷本来就是远端——文件管理器据此
/// 走远端的那套规则，最要紧的一条是 GNOME 在 `local-only` 偏好下**不生成缩略图**：
/// Nautilus 生成缩略图会把每个文件整份读一遍，落到我们这个卷上就是整份下载（实测
/// 看一眼 8 个文件的目录要拉 14 MB）。不伪装成远端的话，`filesystem::remote` 是
/// FALSE，任何 FUSE 卷都被当成本地盘。
pub(crate) const REMOTE_FS_SUBTYPE: &str = "sshfs";
