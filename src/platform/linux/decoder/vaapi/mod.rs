//! VA-API hardware decode for Linux.
//!
//! H.264 与 HEVC（Main，8 位 4:2:0）走硬件，前提是驱动为该 profile 提供了
//! VLD 入口；画面一律拷回 NV12，因此 10 位与 4:4:4 不声明。HEVC 没有软件
//! 回退，驱动不支持时整个 H.265 就不在对端的能力清单里。
mod avc;
mod display;
mod hevc;
mod hevc_ref_list;
mod output;
mod ref_list;

use cros_libva::VAProfile;

use crate::media::VideoCodec;
use crate::media::decode_api::{DecodeError, VideoDecoderConfig};

pub(super) use avc::Frame;
pub(crate) use output::{DmaBufFrame, DmaBufPlane};

/// 一帧解码结果：要么已经拷回内存，要么是解码 surface 直接导出的 dma-buf。
pub(super) enum Output {
    Cpu(Frame),
    DmaBuf(output::DmaBufFrame),
}

/// 解码端零拷贝是否开着：偏好（图形界面 / `media-preferences.json`，默认开）
/// 与 `OPENUUYC_DECODER_ZEROCOPY` 环境变量，见 `media::preferences::decoder_zero_copy`。
/// 导出失败一次之后整体不再试：这类失败基本是驱动或格式层面的。
fn zero_copy_enabled() -> bool {
    crate::media::preferences::decoder_zero_copy()
}

fn zero_copy_given_up() -> bool {
    static GIVEN_UP: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    GIVEN_UP.load(std::sync::atomic::Ordering::Acquire)
}

/// 导出失败：这一帧已经退回 CPU 拷贝了，以后不再试。
pub(super) fn zero_copy_available() -> bool {
    zero_copy_enabled() && !zero_copy_given_up()
}

pub(super) fn zero_copy_give_up(error: &DecodeError) {
    static GIVEN_UP: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if !GIVEN_UP.swap(true, std::sync::atomic::Ordering::AcqRel) {
        tracing::info!(?error, "解码端零拷贝放弃，退回 CPU 拷贝");
    }
}

pub(super) struct Session {
    codec: Codec,
    /// The parameter sets from the negotiation, prepended to the first access
    /// unit so the parser can activate an SPS/PPS before the first slice.
    extra: Option<bytes::Bytes>,
}

enum Codec {
    H264(Box<avc::Avc>),
    Hevc(Box<hevc::Hevc>),
}

impl Session {
    pub(super) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        if config.width == 0 || config.height == 0 || config.width > 16384 || config.height > 16384
        {
            return Err(DecodeError::InvalidInput);
        }
        if !config.extra_data.is_empty()
            && !config.extra_data.starts_with(&[0, 0, 1])
            && !config.extra_data.starts_with(&[0, 0, 0, 1])
        {
            return Err(DecodeError::Unsupported);
        }
        match config.codec {
            VideoCodec::H264 => {
                // The profile the stream actually uses is only known once its
                // SPS arrives; this rejects drivers that decode no H.264 at all.
                if !probe(VideoCodec::H264, config.width, config.height, 8, 1) {
                    return Err(DecodeError::Unsupported);
                }
                Ok(Self {
                    codec: Codec::H264(Box::new(avc::Avc::new())),
                    extra: Some(config.extra_data.clone()),
                })
            }
            VideoCodec::H265 => {
                if !probe(VideoCodec::H265, config.width, config.height, 8, 1) {
                    return Err(DecodeError::Unsupported);
                }
                Ok(Self {
                    codec: Codec::Hevc(Box::new(hevc::Hevc::new())),
                    extra: Some(config.extra_data.clone()),
                })
            }
            // AV1 slice parameters are not written for VA-API yet.
            VideoCodec::Av1 => Err(DecodeError::Unsupported),
        }
    }

    pub(super) fn decode(
        &mut self,
        payload: &[u8],
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Output, DecodeError> {
        let extra = self.extra.take().filter(|extra| !extra.is_empty());
        let combined;
        let data = match extra {
            Some(extra) => {
                combined = [extra.as_ref(), payload].concat();
                &combined[..]
            }
            None => payload,
        };
        match &mut self.codec {
            Codec::H264(session) => session.decode(data, cancel),
            Codec::Hevc(session) => session.decode(data, cancel),
        }
    }

    pub(super) fn reset(&mut self) {
        match &mut self.codec {
            Codec::H264(session) => session.reset(),
            Codec::Hevc(session) => session.reset(),
        }
    }
}

/// Whether the local driver decodes this codec at this size in hardware.
pub(super) fn probe(codec: VideoCodec, width: u32, height: u32, depth: u8, chroma: u8) -> bool {
    if depth != 8 || chroma != 1 {
        // 10-bit and 4:4:4 need surface formats this backend does not read back.
        return false;
    }
    let profiles: &[VAProfile::Type] = match codec {
        VideoCodec::H264 => &[
            VAProfile::VAProfileH264High,
            VAProfile::VAProfileH264Main,
            VAProfile::VAProfileH264ConstrainedBaseline,
        ],
        // Main10 的 P010 读不回来，所以只有 Main。
        VideoCodec::H265 => &[VAProfile::VAProfileHEVCMain],
        _ => return false,
    };
    profiles
        .iter()
        .any(|profile| display::supports(*profile, width, height))
}
