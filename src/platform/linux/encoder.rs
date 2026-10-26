//! Linux host encoding: NVENC on NVIDIA, VA-API through libavcodec on AMD and
//! Intel, the Rust H.264 software codec otherwise. As on Windows, hardware comes
//! first and every candidate is proven on the selected desktop by encoding a
//! frame and checking the SPS it produced.
use super::capture::{Desktop, Device, Image};
use super::cuda;
use crate::media::encoding::{Backend, Capability, Codec, Color, Format, QualityTarget, Rate};
use anyhow::{Context, Result, bail, ensure};
use std::os::fd::AsFd;

pub(crate) use crate::media::encoding::{Encoded, FrameTiming};

/// 50 ms 一轮地等第一帧：五秒。
const FIRST_PICTURE_TRIES: u32 = 100;

pub(crate) fn probe(
    desktop: &mut Desktop,
    is_active: impl Fn() -> bool,
) -> Result<Vec<Capability>> {
    // 门户的第一帧可能要等好几秒：流要先协商格式和缓冲，KWin 还可能等画面变化
    // 才发帧。X11、NvFBC 和 wlr-screencopy 是同步应答，这段等待只在真的没有画面
    // 时才花出去；连接被取消会提前退出。
    let mut frame = None;
    for _ in 0..FIRST_PICTURE_TRIES {
        if !is_active() {
            bail!("被控准备已取消");
        }
        if let Some(captured) = desktop.next(50, 1, false, false, (1280, 720))? {
            frame = Some(captured);
            break;
        }
    }
    let frame = match (frame, desktop.waiting()) {
        (Some(frame), _) => frame,
        (None, Some(stage)) => bail!("尚未取得所选桌面画面：{stage}"),
        (None, None) => bail!("尚未取得所选桌面画面"),
    };
    let mut candidates = Vec::new();
    if crate::media::encoding::nvenc::Api::load().is_ok() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            for chroma in [1, 3] {
                let format = Format {
                    codec,
                    chroma,
                    depth: 8,
                };
                if Backend::Nvidia.accepts(format) && super::nvenc::Encoder::accepts(format) {
                    candidates.push((Backend::Nvidia, format));
                }
            }
        }
    }
    if let Some(backend) = super::vaapi::hardware_backend() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let format = Format {
                codec,
                chroma: 1,
                depth: 8,
            };
            if super::vaapi::accepts(format) {
                candidates.push((backend, format));
            }
        }
    }
    candidates.push((Backend::Software, Format::AVC));
    let rate = Rate {
        target: 2_000_000,
        peak: 2_000_000,
        fps: 30,
        quality: 1,
        quality_target: QualityTarget {
            bitrate: 2_000_000,
            fps: 30,
        },
    };
    let mut result = Vec::new();
    for (backend, format) in candidates {
        if !is_active() {
            bail!("被控准备已取消");
        }
        let probe = (|| -> Result<Capability> {
            let size = (frame.width, frame.height);
            let mut encoder = if backend == Backend::Software {
                Encoder::software_format(&desktop.device, size, format, rate)?
            } else {
                Encoder::hardware_format(backend, &desktop.device, size, format, rate, Color::Sdr)?
            };
            let mut output = None;
            for index in 0..20 {
                output = encoder
                    .encode(&frame.image, index * 333_333, true)?
                    .iter()
                    .find_map(|encoded| {
                        crate::media::video_format::parse_stream_format(
                            format.codec.media(),
                            &encoded.data,
                        )
                    });
                if output.is_some() {
                    break;
                }
            }
            let output = output.context("编码器未输出可验证的序列头")?;
            ensure!(
                output.chroma_format_idc == format.chroma
                    && output.bit_depth_luma == format.depth
                    && output.bit_depth_chroma == format.depth
                    && (output.visible_width, output.visible_height) == size,
                "编码器实际码流与请求格式不一致"
            );
            Ok(Capability {
                adapter: desktop.screen.adapter,
                backend,
                format,
                maximum: encoder.maximum_size(),
            })
        })();
        match probe {
            Ok(capability) => {
                tracing::debug!(?capability, "host encoder capability verified from SPS");
                result.push(capability);
            }
            Err(error) => {
                tracing::debug!(?backend, ?format, %error, "host encoder capability rejected")
            }
        }
    }
    ensure!(!result.is_empty(), "所选桌面没有可用的视频编码器");
    Ok(result)
}

/// Per-thread encoder runtime. Neither encoder needs setup.
pub(crate) struct Runtime;
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SwitchCandidate(pub String);
impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }
    /// Windows 那边这个构造器会顺手抢一份 GPU 调度租约（采集和编码挤在同一块卡上
    /// 时给谁优先）；Linux 没有这回事，只为让共享的采集循环两边同一个形状。
    pub(crate) fn with_gpu_compatibility(_compatible: bool) -> Result<Self> {
        Self::new()
    }
    pub(crate) fn begin_gpu_work(&mut self, _compatible: bool) {}
    pub(crate) fn end_gpu_work(&mut self) {}
}

pub(crate) enum Encoder {
    Nvidia(super::nvenc::Encoder),
    /// 同一块 GPU 上的 VA-API 编码：参数集、POC/DPB、GOP 与码率控制都交给
    /// libavcodec。手写那份已经删掉——它回绕一次就让参考帧永久分叉，也没有码率
    /// 控制（见 AGENTS_QA.md）。
    Libavcodec {
        backend: Backend,
        encoder: super::ffmpeg::Encoder,
        /// 零拷贝：dma-buf 的导入、旋转、缩放与转 NV12 全在这一幕里做，它的
        /// 帧上下文就是 `encoder` 的输入。走系统内存那条路时是 `None`。
        scaler: Option<Box<super::ffmpeg::drm::Scaler>>,
    },
    Software(Box<super::software_encoder::Encoder>),
}
impl Encoder {
    pub(crate) fn hardware_format(
        backend: Backend,
        _device: &Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: Color,
    ) -> Result<Self> {
        // The capture backends deliver 8-bit SDR BGRA only.
        ensure!(!color.is_hdr(), "Linux 版暂不支持 HDR 采集");
        match backend {
            Backend::Amd | Backend::Intel => Ok(Self::Libavcodec {
                backend,
                encoder: super::ffmpeg::Encoder::new(size, format, rate)?,
                scaler: None,
            }),
            Backend::Nvidia => {
                ensure!(
                    super::nvenc::Encoder::accepts(format),
                    "NVENC 的 CUDA 输入不支持该格式"
                );
                let context = cuda::display_context()?;
                Ok(Self::Nvidia(super::nvenc::Encoder::new(
                    &context, size, format, rate,
                )?))
            }
            Backend::Software => bail!("软件编码请走 software_format"),
        }
    }
    /// 这个后端能不能直接吃采集留在显存里的帧（零拷贝）。只有 VA-API（走
    /// libavcodec）把 dma-buf 导进 GPU 过：NVENC（CUDA 输入）与软件编码都得先把
    /// 像素拿到系统内存里。
    pub(crate) fn hardware_images(backend: Backend) -> bool {
        matches!(backend, Backend::Amd | Backend::Intel)
    }

    /// 零拷贝：用采集留在显存里的那一帧建编码器。滤镜图的输出帧上下文直接交给
    /// 编码器，于是从合成器的画布到码流之间没有一次 CPU 拷贝。
    ///
    /// 给的帧不是显存帧时说不上零拷贝，直接按普通那条建。
    pub(crate) fn hardware_format_from_frame(
        backend: Backend,
        device: &Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: Color,
        image: &Image,
    ) -> Result<Self> {
        let Some(buffer) = image.dmabuf() else {
            return Self::hardware_format(backend, device, size, format, rate, color);
        };
        ensure!(!color.is_hdr(), "Linux 版暂不支持 HDR 采集");
        ensure!(
            matches!(backend, Backend::Amd | Backend::Intel),
            "这个编码后端吃不了显存帧"
        );
        let transform = match image {
            Image::DmaBuf { transform, .. } => *transform,
            _ => unreachable!(),
        };
        let rotation = super::capture::rotation_of(transform)
            .context("这块屏的旋转没法在 GPU 上还原")?;
        let node = super::capture::dmabuf::card_path()?;
        let scaler = super::ffmpeg::drm::Scaler::new(
            &node,
            &super::ffmpeg::drm::Frame {
                fd: buffer.prime.as_fd(),
                // 滤镜图的输入是合成器自己的画布，尺寸得按缓冲自己的来，
                // 「这块屏看到的尺寸」是旋转之后的事。
                width: buffer.width,
                height: buffer.height,
                stride: buffer.stride,
                format: buffer.format,
            },
            rotation,
            size,
        )?;
        let encoder = super::ffmpeg::Encoder::with_frames(scaler.frames(), size, format, rate)?;
        Ok(Self::Libavcodec {
            backend,
            encoder,
            scaler: Some(Box::new(scaler)),
        })
    }

    /// 只有 Windows 的 QSV 能在不拆驱动会话的前提下换尺寸/格式；Linux 这条路
    /// 一律重建（返回 false 就是「照旧走重建」）。
    pub(crate) fn reconfigure_stream(
        &mut self,
        _device: &Device,
        _size: (u32, u32),
        _format: Format,
        _rate: Rate,
        _color: Color,
    ) -> Result<bool> {
        Ok(false)
    }
    pub(crate) fn software_format(
        _device: &Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        Ok(Self::Software(Box::new(
            super::software_encoder::Encoder::new(size, format, rate)?,
        )))
    }
    /// 这条编码路径的人话说明，给诊断面板看：同一个后端还有零拷贝与走系统内存
    /// 之分，光看后端名字分不出来。
    pub(crate) fn path(&self) -> &'static str {
        match self {
            Self::Nvidia(_) => "NVENC",
            Self::Libavcodec {
                scaler: Some(_), ..
            } => "VA-API · libavcodec · 零拷贝",
            Self::Libavcodec { .. } => "VA-API · libavcodec",
            Self::Software(_) => "软件编码",
        }
    }
    pub(crate) fn implementation(&self) -> i32 {
        match self {
            Self::Nvidia(_) => Backend::Nvidia.implementation(),
            Self::Libavcodec { backend, .. } => backend.implementation(),
            Self::Software(_) => Backend::Software.implementation(),
        }
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        match self {
            Self::Nvidia(encoder) => encoder.maximum_size(),
            Self::Libavcodec { encoder, .. } => {
                super::ffmpeg::Encoder::maximum_size(encoder.format())
            }
            Self::Software(encoder) => encoder.maximum_size(),
        }
    }
    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        match self {
            Self::Nvidia(encoder) => encoder.configure_rate(rate),
            Self::Libavcodec { encoder, .. } => encoder.configure_rate(rate),
            Self::Software(encoder) => encoder.configure(rate),
        }
    }
    pub(crate) fn encode(
        &mut self,
        image: &Image,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        self.encode_cancellable(
            image,
            timestamp,
            keyframe,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }
    pub(crate) fn encode_cancellable(
        &mut self,
        image: &Image,
        timestamp: i64,
        keyframe: bool,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Vec<Encoded>> {
        if keyframe && let Self::Software(e) = self {
            e.request_keyframe();
        }
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "编码已取消"
        );
        match self {
            Self::Nvidia(encoder) => encoder.encode(image, timestamp, keyframe),
            Self::Libavcodec {
                encoder,
                scaler: Some(scaler),
                ..
            } => {
                // 零拷贝：先把这块 dma-buf 摆正、缩放、转成 NV12，都在 GPU 上，
                // 再把那张硬件帧直接递给编码器。
                let buffer = match image {
                    Image::DmaBuf { buffer, .. } => buffer,
                    // 滤镜图是按显存帧建起来的，别的帧到这里就是调用方搞错了。
                    _ => bail!("零拷贝编码器只吃显存帧"),
                };
                let scaled = scaler.scale(
                    &super::ffmpeg::drm::Frame {
                        fd: buffer.prime.as_fd(),
                        width: buffer.width,
                        height: buffer.height,
                        stride: buffer.stride,
                        format: buffer.format,
                    },
                    timestamp,
                )?;
                encoder.encode_hardware(scaled, timestamp, keyframe)
            }
            Self::Libavcodec { encoder, .. } => encoder.encode(image, timestamp, keyframe),
            Self::Software(encoder) => encoder.encode(image, timestamp, keyframe, cancel),
        }
    }
}

#[cfg(test)]
mod tests {
    /// 不需要桌面：给每个 VA-API 编码器灌十帧合成画面，把码流写出来交给
    /// ffmpeg 验。`cargo test --release --lib -- --ignored vaapi_encodes_each_codec`
    #[test]
    #[ignore]
    fn vaapi_encodes_each_codec() {
        use super::super::capture::{Device, Image};
        use crate::media::encoding::{Codec, Color, Format, QualityTarget, Rate};
        use std::sync::Arc;
        let size = (1280, 720);
        let rate = Rate {
            target: 4_000_000,
            peak: 4_000_000,
            fps: 30,
            quality: 1,
            quality_target: QualityTarget {
                bitrate: 4_000_000,
                fps: 30,
            },
        };
        let mut pixels = vec![0u8; (size.0 * size.1 * 4) as usize];
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let x = index as u32 % size.0;
            let y = index as u32 / size.0;
            pixel.copy_from_slice(&[x as u8, y as u8, (x + y) as u8, 255]);
        }
        let image = Image::Cpu {
            pixels: Arc::new(pixels),
            width: size.0,
            height: size.1,
        };
        let backend = super::super::vaapi::hardware_backend().expect("没有 VA-API 后端");
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let format = Format {
                codec,
                chroma: 1,
                depth: 8,
            };
            let mut encoder =
                super::Encoder::hardware_format(backend, &Device, size, format, rate, Color::Sdr)
                    .unwrap();
            let mut bytes = Vec::new();
            for index in 0..10 {
                for encoded in encoder.encode(&image, index * 333_333, index == 0).unwrap() {
                    bytes.extend_from_slice(&encoded.data);
                }
            }
            let signature = crate::media::video_format::parse_stream_format(codec.media(), &bytes);
            println!("{codec:?}: {} 字节，序列头 {signature:?}", bytes.len());
            assert!(signature.is_some(), "{codec:?} 没有可解析的序列头");
            std::fs::write(format!("/tmp/vaapi-{codec:?}.bin"), &bytes).unwrap();
        }
    }

    /// Needs a running X session: `DISPLAY=:0 cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn probe_captures_and_encodes_the_desktop() {
        let screens = super::super::capture::screens().unwrap();
        println!("{screens:#?}");
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend {}", desktop.backend_name());
        let started = std::time::Instant::now();
        let caps = super::probe(&mut desktop, || true).unwrap();
        for cap in &caps {
            println!("{cap:?}");
        }
        println!("probed in {:?}", started.elapsed());
    }
}

#[cfg(test)]
mod throughput_tests {
    /// 上面那条用例的尺寸约定，不需要桌面也不需要 GPU：按画质上限算出来的 `size`
    /// 交给采集当 `maximum`，采集出来的帧就正好是 `size`，不会被再缩一次。这条一断，
    /// 那条吞吐用例又会在 >4K 的屏上撞「采集画面尺寸与编码器不一致」。
    #[test]
    fn the_capture_maximum_is_the_size_the_encoder_is_built_for() {
        use crate::media::geometry::{fit_size, output_size};
        for (width, height) in [(5120, 2160), (3840, 2160), (1920, 1080), (1600, 2560)] {
            let size = output_size(width, height, 4);
            assert_eq!(
                fit_size(size.0, size.1, size),
                size,
                "{width}x{height} 会被再缩一次"
            );
        }
        // 这块 5120x2160 的屏在最高画质下缩到 3840x1620。
        assert_eq!(output_size(5120, 2160, 4), (3840, 1620));
    }

    /// Needs a desktop and a GPU with a hardware encoder: capture and encode
    /// through a session's whole path, at the size a session would use.
    #[test]
    #[ignore]
    fn capture_and_encode_throughput() {
        use crate::media::encoding::{Color, Format, Rate};
        // 最高画质，和官方客户端一样把码流封在 3840x2160。
        const QUALITY: i32 = 4;
        let screens = super::super::capture::screens().unwrap();
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        // 比这个上限大的屏会被采集缩下来，所以编码器要按**缩完之后**的尺寸建：
        // 三个后端都拿 `image.size() == self.size` 卡输入，按屏幕自己的尺寸建会在
        // 第一帧就报「采集画面尺寸与编码器不一致」。生产路径也是这么取的
        // （`features/host/peer/capture_worker.rs` 里从抓回来的那一帧倒推）。
        let size =
            crate::media::geometry::output_size(screens[0].width, screens[0].height, QUALITY);
        let rate = Rate {
            target: 20_000_000,
            peak: 20_000_000,
            fps: 60,
            quality: QUALITY,
            quality_target: crate::media::encoding::QualityTarget {
                bitrate: 20_000_000,
                fps: 60,
            },
        };
        // Whichever hardware encoder this machine has: VA-API where a driver
        // offers it, NVENC on NVIDIA, which has no VA-API encoder to report.
        let backend = super::super::vaapi::hardware_backend()
            .unwrap_or(crate::media::encoding::Backend::Nvidia);
        let mut encoder = super::Encoder::hardware_format(
            backend,
            &desktop.device,
            size,
            Format::AVC,
            rate,
            Color::Sdr,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let (mut frames, mut capture, mut encode, mut bytes) = (
            0u32,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            0usize,
        );
        let mut cached = None;
        while started.elapsed() < std::time::Duration::from_secs(3) {
            let t = std::time::Instant::now();
            // `maximum` 就是 `size`：抓帧这一步再缩放一次的话又会和编码器对不上。
            if let Some(frame) = desktop.next(16, QUALITY, false, false, size).unwrap() {
                cached = Some(frame);
            }
            capture += t.elapsed();
            let Some(frame) = &cached else { continue };
            let t = std::time::Instant::now();
            for out in encoder
                .encode(&frame.image, i64::from(frames) * 166_666, frames == 0)
                .unwrap()
            {
                bytes += out.data.len();
            }
            encode += t.elapsed();
            frames += 1;
        }
        println!(
            "{}: {frames} frames in 3s ({:.0} fps), capture {:.2} ms/frame, encode {:.2} ms/frame, {} KiB",
            desktop.backend_name(),
            f64::from(frames) / 3.0,
            capture.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            encode.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            bytes / 1024
        );
    }
}
