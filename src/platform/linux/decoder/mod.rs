//! Linux video decoding: VA-API in hardware, the Rust H.264/AV1 codecs in
//! software.
//! Both paths hand the picture over on the CPU; VA-API surfaces are copied
//! out rather than shared with the renderer, so there is no zero-copy yet.
#![cfg(not(windows))]
use crate::media::VideoCodec;
use crate::media::decode_api::{DecodeError, DecoderMode, DecoderNotification, VideoDecoderConfig};
use bytes::Bytes;

mod vaapi;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuFormat {
    Nv12,
    I444,
}

pub struct CpuVideoFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub format: CpuFormat,
    pub data: Bytes,
}

/// VA-API opens its own display; the decoder shares no device with the
/// renderer, so there is none to pass in.
pub(crate) type GpuDevice = std::convert::Infallible;

pub enum PlatformDecodedFrame {
    Cpu(CpuVideoFrame),
}

/// Whether the local VA-API driver decodes this format in hardware.
pub fn probe_hardware(codec: VideoCodec, width: u32, height: u32, depth: u8, chroma: u8) -> bool {
    vaapi::probe(codec, width, height, depth, chroma)
}

enum Backend {
    /// VA-API, which decodes one access unit per call.
    Hardware(Box<vaapi::Session>),
    Software(Box<openuuyc_codec::decoder::Decoder>),
}

pub struct LinuxVideoDecoder {
    backend: Backend,
    pending: VecDeque<PlatformDecodedFrame>,
    notification: DecoderNotification,
}

fn software_error(error: openuuyc_codec::decoder::Error) -> DecodeError {
    use openuuyc_codec::decoder::Error;
    match error {
        Error::Closed => DecodeError::Closed,
        Error::Unsupported => DecodeError::Unsupported,
        Error::Allocation => DecodeError::Backend,
        Error::InvalidInput => DecodeError::InvalidInput,
        Error::NeedKeyframe => {
            tracing::debug!(%error, "software decoder input requires a new keyframe");
            DecodeError::NeedKeyframe
        }
    }
}

impl LinuxVideoDecoder {
    pub fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        let backend = match config.mode {
            // Hardware surfaces still come back on the CPU.
            DecoderMode::Hardware => Backend::Hardware(Box::new(vaapi::Session::open(config)?)),
            DecoderMode::Software => {
                if config.width == 0
                    || config.height == 0
                    || config.width > 16384
                    || config.height > 16384
                {
                    return Err(DecodeError::InvalidInput);
                }
                let decoder =
                    openuuyc_codec::decoder::Decoder::new(config.codec, &config.extra_data)
                        .map_err(software_error)?;
                Backend::Software(Box::new(decoder))
            }
        };
        Ok(Self {
            backend,
            pending: VecDeque::new(),
            notification: DecoderNotification::default(),
        })
    }

    pub fn probe_format(
        _device: GpuDevice,
        codec: VideoCodec,
        width: u32,
        height: u32,
        depth: u8,
        chroma: u8,
    ) -> bool {
        vaapi::probe(codec, width, height, depth, chroma)
    }

    pub fn poll_owned_frame(&mut self) -> Result<Option<PlatformDecodedFrame>, DecodeError> {
        if self.notification.is_cancelled() {
            self.pending.clear();
            return Err(DecodeError::Closed);
        }
        Ok(self.pending.pop_front())
    }

    pub fn poll_dropped_token(&mut self) -> Option<i64> {
        None
    }

    pub fn reset_for_keyframe(&mut self) -> Result<(), DecodeError> {
        self.pending.clear();
        match &mut self.backend {
            Backend::Hardware(session) => {
                session.reset();
                Ok(())
            }
            Backend::Software(decoder) => decoder.reset().map_err(software_error),
        }
    }
}

impl LinuxVideoDecoder {
    pub fn set_notification(&mut self, notification: DecoderNotification) {
        if let Backend::Software(decoder) = &mut self.backend {
            decoder.set_cancellation(notification.shared_cancellation());
        }
        self.notification = notification;
    }

    pub fn push_packet(&mut self, payload: &[u8], token: i64) -> Result<(), DecodeError> {
        if payload.len() > i32::MAX as usize {
            return Err(DecodeError::InvalidInput);
        }
        let decoder = match &mut self.backend {
            Backend::Hardware(session) => {
                match session.decode(payload, self.notification.cancellation())? {
                    vaapi::Output::Cpu(frame) => {
                        self.pending
                            .push_back(PlatformDecodedFrame::Cpu(CpuVideoFrame {
                                pts: token,
                                width: frame.width,
                                height: frame.height,
                                format: CpuFormat::Nv12,
                                data: Bytes::from(frame.data),
                            }));
                    }
                    // 解码端的零拷贝这一半已经就绪，渲染端（纹理导入与采样子矩形）
                    // 还没接上；眼下唯一能开它的方式是 `OPENUUYC_DECODER_ZEROCOPY=1`，
                    // 那就明确报错，不要悄悄丢掉一帧。
                    vaapi::Output::DmaBuf(_) => {
                        tracing::error!("解码端零拷贝的渲染端尚未接上");
                        return Err(DecodeError::Unsupported);
                    }
                }
                return Ok(());
            }
            Backend::Software(decoder) => decoder,
        };
        for frame in decoder.push(payload, token).map_err(software_error)? {
            if self.notification.is_cancelled() {
                self.pending.clear();
                return Err(DecodeError::Closed);
            }
            let format = match frame.format {
                openuuyc_codec::PixelFormat::Nv12 => CpuFormat::Nv12,
                openuuyc_codec::PixelFormat::I444 => CpuFormat::I444,
                // 10-bit and packed 4:4:4 AV1 are never advertised here.
                _ => return Err(DecodeError::Unsupported),
            };
            // AV1 pads odd NV12 sizes to even ones, repeating the edge pixels;
            // the packed planes are laid out at the coded size.
            self.pending
                .push_back(PlatformDecodedFrame::Cpu(CpuVideoFrame {
                    pts: frame.pts,
                    width: frame.coded_width,
                    height: frame.coded_height,
                    format,
                    data: frame.data,
                }));
        }
        Ok(())
    }
}

impl Drop for LinuxVideoDecoder {
    fn drop(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把 annex-B 码流切成访问单元：`first_slice_segment_in_pic_flag`（H.264 那边是
    /// `first_mb_in_slice` 的第一个比特）为 1 的 VCL NAL 起新的一张；硬件后端一次
    /// 只吃一个访问单元。H.264 的 NAL 头只有一字节、HEVC 是两字节，切片头随之错开。
    fn access_units(codec: VideoCodec, stream: &[u8]) -> Vec<Vec<u8>> {
        let mut units = Vec::new();
        let mut current = Vec::new();
        // 参数集与 SEI 要跟它们之后的切片待在同一个访问单元里，所以只有已经收过
        // 切片的单元才会被新图像的首片切开。
        let mut saw_slice = false;
        for nal in openuuyc_codec::syntax::h264::nal::AnnexBSplitter::new(stream) {
            let Some(&head) = nal.first() else {
                continue;
            };
            let (kind, vcl, slice) = match codec {
                VideoCodec::H265 => ((head >> 1) & 0x3f, true, 2usize),
                _ => (head & 0x1f, (1..=5).contains(&(head & 0x1f)), 1usize),
            };
            let vcl = vcl && (codec != VideoCodec::H265 || kind <= 31);
            let first = nal.get(slice).is_some_and(|byte| byte & 0x80 != 0);
            if vcl && first && saw_slice {
                units.push(std::mem::take(&mut current));
                saw_slice = false;
            }
            // 起始码要还回去：解码器自己也要按 annex-B 再切一遍。
            current.extend_from_slice(&[0, 0, 0, 1]);
            current.extend_from_slice(nal);
            saw_slice |= vcl;
        }
        if !current.is_empty() {
            units.push(current);
        }
        units
    }

    /// 用硬件后端把 fixture 跑完，返回 (帧数, 首帧尺寸)。本地没有该 codec 的
    /// VA-API 解码时返回 None（没有 GPU 的机器跳过而不是失败）。
    fn decode_fixture(
        codec: VideoCodec,
        stream: &[u8],
        requested: (u32, u32),
    ) -> Option<(usize, (u32, u32))> {
        let config = VideoDecoderConfig {
            codec,
            width: requested.0,
            height: requested.1,
            mode: DecoderMode::Hardware,
            gpu_device: None,
            extra_data: Bytes::new(),
        };
        let mut decoder = match LinuxVideoDecoder::open(&config) {
            Ok(decoder) => decoder,
            Err(error) => {
                eprintln!("跳过 {codec:?} 硬件解码：{error:?}");
                return None;
            }
        };
        let mut size = (0, 0);
        let mut frames = 0usize;
        for unit in access_units(codec, stream) {
            if let Err(error) = decoder.push_packet(&unit, frames as i64) {
                panic!("{codec:?} 第 {frames} 帧之后解码失败：{error:?}");
            }
            while let Ok(Some(frame)) = decoder.poll_owned_frame() {
                let PlatformDecodedFrame::Cpu(frame) = frame;
                assert_eq!(frame.format, CpuFormat::Nv12);
                assert_eq!(
                    frame.data.len(),
                    (frame.width * frame.height * 3 / 2) as usize,
                    "{codec:?} 的 NV12 长度不对"
                );
                if frames == 0 {
                    size = (frame.width, frame.height);
                }
                frames += 1;
            }
        }
        Some((frames, size))
    }

    #[test]
    fn decodes_h264_and_hevc_frames_in_hardware() {
        let h264 = include_bytes!("../../../media/decoder/fixtures/matrix/h264_1280x720.annexb");
        let hevc = include_bytes!("../../../media/decoder/fixtures/matrix/hevc_1280x720.annexb");
        for (codec, stream) in [
            (VideoCodec::H264, h264.as_slice()),
            (VideoCodec::H265, hevc.as_slice()),
        ] {
            let Some((frames, size)) = decode_fixture(codec, stream, (1280, 720)) else {
                continue;
            };
            assert!(frames > 0, "{codec:?} 一帧都没解出来");
            assert_eq!(size, (1280, 720), "{codec:?} 出图尺寸不对");
            eprintln!("{codec:?} 硬件解码 {frames} 帧 @{size:?}");
        }
    }

    #[test]
    fn decodes_multi_frame_fixtures_in_hardware() {
        // 多帧样本里才有 P/B 片，参考图像列表与 DPB 才算真的被走到。
        let h264 = include_bytes!("../../../media/decoder/fixtures/h264.annexb");
        let hevc = include_bytes!("../../../media/decoder/fixtures/hevc.annexb");
        for (codec, stream) in [
            (VideoCodec::H264, h264.as_slice()),
            (VideoCodec::H265, hevc.as_slice()),
        ] {
            let Some((frames, size)) = decode_fixture(codec, stream, (1920, 1080)) else {
                continue;
            };
            assert!(frames > 1, "{codec:?} 只解出 {frames} 帧，多帧码流没走完");
            eprintln!("{codec:?} 硬件解码 {frames} 帧，首帧 {size:?}");
        }
    }
}
