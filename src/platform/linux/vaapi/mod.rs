//! 选择驱动显示器的那块 GPU、打开 VA display，并向协议申报 AMD / Intel 后端与
//! 编解码能力。
//!
//! 编码本身已经交给 libavcodec（见 `super::ffmpeg`），这里只剩设备与能力探测：
//! 哪块 GPU 在驱动画面、这个驱动接不接受某个格式。`vaInitialize` 贵且 display
//! 绑定打开它的线程，所以每个线程各留一份。
use crate::media::encoding::{Backend, Codec, Format};
use anyhow::Result;
use cros_libva::{Display, VAEntrypoint, VAProfile};
use std::cell::RefCell;
use std::rc::Rc;

/// The PCI vendors whose VA-API encoder is what the protocol already calls
/// `Amd` and `Intel`. Any other vendor stays unclaimed rather than mislabelled.
const PCI_VENDOR_AMD: u16 = 0x1002;
const PCI_VENDOR_INTEL: u16 = 0x8086;
/// NVIDIA 的 VA 驱动不由 libva 从内核驱动推断出来，需要显式点名（见 `open_display`）。
const PCI_VENDOR_NVIDIA: u16 = 0x10de;

thread_local! {
    /// `vaInitialize` is expensive and a display is bound to the thread that
    /// opened it, so every thread that encodes keeps its own. The error case is
    /// cached too: a machine without VA-API must not retry per candidate.
    static DEVICE: RefCell<Option<Result<(Rc<Display>, u16), String>>> = const { RefCell::new(None) };
}

/// The PCI slots of the DRM cards with a connected output.
fn connected_slots() -> Vec<String> {
    std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
        })
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let card = name.split('-').next()?.to_owned();
            let uevent =
                std::fs::read_to_string(format!("/sys/class/drm/{card}/device/uevent")).ok()?;
            uevent
                .lines()
                .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                .map(|slot| slot.to_ascii_lowercase())
        })
        .collect()
}

/// The render node of the GPU that drives the display, with its PCI vendor.
/// Matched through the PCI address of the card whose output is connected, so a
/// machine with both an integrated and a discrete GPU encodes on the one whose
/// picture is being captured. The first node that answers is used when sysfs
/// cannot tell.
pub(crate) fn display_node() -> Option<(String, u16)> {
    let connected = connected_slots();
    let mut fallback = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("renderD") {
            continue;
        }
        let device = entry.path().join("device");
        let vendor = std::fs::read_to_string(device.join("vendor"))
            .ok()
            .and_then(|value| u16::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok());
        let Some(vendor) = vendor else {
            continue;
        };
        let candidate = (format!("/dev/dri/{name}"), vendor);
        let slot = std::fs::read_to_string(device.join("uevent"))
            .ok()
            .and_then(|uevent| {
                uevent
                    .lines()
                    .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                    .map(|slot| slot.to_ascii_lowercase())
            });
        if slot.is_some_and(|slot| connected.contains(&slot)) {
            return Some(candidate);
        }
        fallback.get_or_insert(candidate);
    }
    fallback
}

fn open_display() -> Option<Rc<Display>> {
    // 显示节点可能是 NVIDIA，下面会 dlopen 出 nvidia VA 驱动；先把 CUDA 初始化
    // 走完，免得驱动构造器里的首次 cuInit 和 NvFBC/NVENC 互等（见 cuda.rs）。
    super::cuda::initialize_before_va();
    if let Some((path, vendor)) = display_node() {
        if let Ok(display) = Display::open_drm_display(&path) {
            return Some(display);
        }
        // libva 2.15 以前不把 `nvidia-drm` 内核驱动映射到 nvidia-vaapi-driver，
        // 于是这个节点的 `vaInitialize` 失败；点名驱动再试一次，等价于用户
        // 自己设 `LIBVA_DRIVER_NAME=nvidia`。
        if vendor == PCI_VENDOR_NVIDIA
            && let Ok(named) = Display::open_drm_display_with_driver(&path, Some(c"nvidia"))
        {
            tracing::debug!(path, "VA-API 按 NVIDIA 驱动名重试成功");
            return Some(named);
        }
    }
    Display::open()
}

fn device() -> Result<(Rc<Display>, u16)> {
    DEVICE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let opened = slot.get_or_insert_with(|| {
            let vendor = display_node().map_or(0, |(_, vendor)| vendor);
            match open_display() {
                Some(display) => Ok((display, vendor)),
                None => Err("没有可用的 VA-API 设备".to_owned()),
            }
        });
        match opened {
            Ok((display, vendor)) => Ok((Rc::clone(display), *vendor)),
            Err(message) => anyhow::bail!("{message}"),
        }
    })
}

/// The wire backend for this machine's VA-API encoder, or `None` when the GPU
/// is from a vendor the protocol has no id for.
pub(crate) fn hardware_backend() -> Option<Backend> {
    match device().ok()?.1 {
        PCI_VENDOR_AMD => Some(Backend::Amd),
        PCI_VENDOR_INTEL => Some(Backend::Intel),
        _ => None,
    }
}

/// The VA profile a format is encoded with. 8 位 4:2:0 只对应各编码的最基础档；
/// 驱动没有对应 profile 时 `accepts()` 直接拒掉，由 `encoder.rs` 的试编再做一次
/// 实证。
fn profile(format: Format) -> Option<i32> {
    match format.codec {
        Codec::H264 => Some(VAProfile::VAProfileH264ConstrainedBaseline),
        Codec::H265 => Some(VAProfile::VAProfileHEVCMain),
        Codec::Av1 => Some(VAProfile::VAProfileAV1Profile0),
    }
}

/// The slice entrypoint for a profile. Both are accepted when the driver
/// offers them; `EncSliceLP` alone is Intel's low-power path and AMD rejects
/// it, so the ordinary entrypoint is preferred.
fn entrypoint(display: &Display, profile: i32) -> Option<u32> {
    let entrypoints = display.query_config_entrypoints(profile).ok()?;
    [
        VAEntrypoint::VAEntrypointEncSlice,
        VAEntrypoint::VAEntrypointEncSliceLP,
    ]
    .into_iter()
    .find(|candidate| entrypoints.contains(candidate))
}

fn profile_entrypoint(format: Format) -> Option<(Rc<Display>, i32, u32)> {
    let display = device().map(|(display, _)| display).ok()?;
    let profile = profile(format)?;
    let entrypoint = entrypoint(&display, profile)?;
    Some((display, profile, entrypoint))
}

/// Whether this GPU declares support for `format`. Only the profile and
/// entrypoint are checked; whether a usable stream comes out is decided by the
/// probe in `encoder.rs`, which encodes a frame and parses its SPS.
pub(crate) fn accepts(format: Format) -> bool {
    // The capture backends deliver 8-bit BGRA; any other input format would
    // need a different surface type and conversion here.
    format.chroma == 1 && format.depth == 8 && profile_entrypoint(format).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cros_libva::{VAConfigAttrib, VAConfigAttribType, VA_RT_FORMAT_RGB32, VA_RT_FORMAT_YUV420};

    /// VPP（`VAEntrypointVideoProc`）能不能吃 RGB32、最大能到多大：GPU 缩放这条路的
    /// 前提。`cargo test --release --lib -- --ignored reports_video_proc --nocapture`
    #[test]
    #[ignore]
    fn reports_video_proc() {
        let (display, _, _) = profile_entrypoint(Format {
            codec: Codec::H264,
            chroma: 1,
            depth: 8,
        })
        .unwrap();
        let Ok(entrypoints) = display.query_config_entrypoints(VAProfile::VAProfileNone) else {
            println!("没有 VAProfileNone");
            return;
        };
        println!(
            "VAProfileNone 支持：{}",
            if entrypoints.contains(&VAEntrypoint::VAEntrypointVideoProc) {
                "VideoProc"
            } else {
                "（没有 VideoProc）"
            }
        );
        let mut attrs = vec![
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribRTFormat,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribMaxPictureWidth,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribMaxPictureHeight,
                value: 0,
            },
        ];
        match display.get_config_attributes(
            VAProfile::VAProfileNone,
            VAEntrypoint::VAEntrypointVideoProc,
            &mut attrs,
        ) {
            Ok(()) => {
                let formats = attrs[0].value;
                println!(
                    "RTFormat 位图 {formats:#x}：YUV420 {}，RGB32 {}",
                    formats & VA_RT_FORMAT_YUV420 != 0,
                    formats & VA_RT_FORMAT_RGB32 != 0
                );
                println!("最大尺寸 {}x{}", attrs[1].value, attrs[2].value);
            }
            Err(error) => println!("get_config_attributes: {error:?}"),
        }
    }
}
