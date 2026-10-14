//! 用 libavcodec 做 VA-API 编码。
//!
//! 这一层原本是我们手写的（`vaapi/` 下：参数集、POC/DPB、GOP、码率控制）。手写的
//! 代价是驱动的不透明行为——它自己决定条带里写哪些字段、按我们给的模数决定用几个
//! 参考帧，而 `frame_num` / `pic_order_cnt_lsb` 回绕一次就会让参考帧永久分叉
//! （见 AGENTS_QA.md）；HEVC 至今打不开也是同一件事。这些在 libavcodec 里都有成熟
//! 实现，顺带白拿 HEVC 与 AV1。
//!
//! 色彩转换也交给库（BGRA 软件帧 → `av_hwframe_transfer_data` → NV12 硬件帧），
//! 于是我们不再自己写 NV12 交织与上传。
//!
//! `drm` 那条路是零拷贝：合成器画进显存的那块 dma-buf 直接喂进编码器，
//! CPU 一个像素都不碰（缩放与色彩转换在那条路上走 VPP，见 `drm.rs`）。
pub(crate) mod drm;
use crate::media::encoding::{Codec, Color, Encoded, Format, Rate};
use crate::platform::linux::capture::Image;
use anyhow::{Context as _, Result, bail, ensure};
use ffmpeg_sys_next as ff;
use std::ffi::CString;
use std::ptr;

/// `AVERROR(EAGAIN)`：这一次没有数据可取，下一帧再试。
const EAGAIN: i32 = -ff::EAGAIN;
/// `AVERROR_EOF`：`FFERRTAG('E','O','F',' ')`，绑定里没生成这个宏。
const EOF: i32 = -0x2046_4f45;
/// VA-API 编码器 `rc_mode` 的 VBR。这个枚举只在编码器的 C 源码里，头文件里没有，
/// 所以只能写字面量（见 `ffmpeg -h encoder=h264_vaapi`）。
const VBR: i64 = 3;

fn check(code: i32, what: &str) -> Result<()> {
    ensure!(code >= 0, "{what}失败（libavcodec {code}）");
    Ok(())
}

/// 用 libavcodec 的 VA-API 编码器编一路画面。
pub(crate) struct Encoder {
    context: *mut ff::AVCodecContext,
    frames: *mut ff::AVBufferRef,
    /// 编码器的输入：先把采集到的 BGRA 填进这张系统内存帧。
    scratch: *mut ff::AVFrame,
    /// 硬件帧：由 `av_hwframe_transfer_data` 填，也就是转换 + 上传那一趟。
    hardware: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    size: (u32, u32),
    format: Format,
    rate: Rate,
    frame_rate: crate::media::encoding::rate::Controller,
    force_keyframe: bool,
}

/// 支持的格式：8 位 4:2:0，其余的交给它自己的探测决定。
pub(crate) fn accepts(format: Format) -> bool {
    format.depth == 8 && format.chroma == 1 && codec_name(format.codec).is_some()
}

fn codec_name(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264 => Some("h264_vaapi"),
        Codec::H265 => Some("hevc_vaapi"),
        Codec::Av1 => Some("av1_vaapi"),
    }
}

impl Encoder {
    pub(crate) fn new(size: (u32, u32), format: Format, rate: Rate) -> Result<Self> {
        Self::create(None, size, format, rate)
    }

    /// `frames` 给定时用它（零拷贝那条路：帧来自滤镜图的 buffersink，编码器必须
    /// 和那些帧共用同一个帧上下文），否则自己建一个并准备一块软件输入帧。
    fn create(
        external: Option<*mut ff::AVBufferRef>,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        ensure!(accepts(format), "libavcodec 的 VA-API 不支持该格式");
        let name = CString::new(codec_name(format.codec).context("未知编码格式")?)?;
        // Safe because every pointer below is either null-checked here or owned
        // by this encoder afterwards, and libavcodec is initialised lazily by
        // these very calls.
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(name.as_ptr());
            ensure!(!codec.is_null(), "libavcodec 里没有这个 VA-API 编码器");

            let frames = match external {
                Some(frames) => ff::av_buffer_ref(frames),
                None => {
                    let device = device();
                    let frames = ff::av_hwframe_ctx_alloc(device);
                    ensure!(!frames.is_null(), "分配硬件帧上下文失败");
                    {
                        let context = (*frames).data as *mut ff::AVHWFramesContext;
                        (*context).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                        (*context).sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                        (*context).width = size.0 as i32;
                        (*context).height = size.1 as i32;
                        (*context).initial_pool_size = 4;
                    }
                    check(ff::av_hwframe_ctx_init(frames), "初始化硬件帧上下文")?;
                    frames
                }
            };

            let context = ff::avcodec_alloc_context3(codec);
            ensure!(!context.is_null(), "分配编码上下文失败");
            (*context).width = size.0 as i32;
            (*context).height = size.1 as i32;
            (*context).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*context).time_base = ff::AVRational {
                num: 1,
                den: rate.fps.max(1) as i32,
            };
            (*context).framerate = ff::AVRational {
                num: rate.fps.max(1) as i32,
                den: 1,
            };
            // 一个 GOP 就是一秒：库自己会在该发 IDR 的时候发，不用我们数。
            (*context).gop_size = rate.fps.max(1) as i32;
            (*context).max_b_frames = 0;
            (*context).bit_rate = i64::from(rate.target.max(1));
            (*context).hw_frames_ctx = ff::av_buffer_ref(frames);
            rate_control(context, rate);
            // 私有选项走字典：编码器自己的 priv_data 要到打开时才建立。
            let mut options: *mut ff::AVDictionary = ptr::null_mut();
            let name = CString::new("rc_mode")?;
            // 这个选项是整数枚举：auto 0 / CQP 1 / CBR 2 / VBR 3 / ICQ 4 / QVBR 5 /
            // AVBR 6。命令行里的 `-rc_mode vbr` 是 ffmpeg 自己把名字解成数字的。
            check(
                ff::av_dict_set_int(&mut options, name.as_ptr(), VBR, 0),
                "设置码率控制",
            )?;
            let opened = ff::avcodec_open2(context, codec, &mut options);
            ff::av_dict_free(&mut options);
            if opened < 0 {
                ff::avcodec_free_context(&mut { context });
                bail!("打开 VA-API 编码器失败（libavcodec {opened}）");
            }

            // 自己建帧上下文的那条路（CPU 喂进来）才需要一块软件输入帧。
            let scratch = if external.is_none() {
                let scratch = ff::av_frame_alloc();
                (*scratch).format = ff::AVPixelFormat::AV_PIX_FMT_BGRA as i32;
                (*scratch).width = size.0 as i32;
                (*scratch).height = size.1 as i32;
                check(ff::av_frame_get_buffer(scratch, 0), "分配输入帧")?;
                scratch
            } else {
                ptr::null_mut()
            };

            let hardware = if external.is_none() {
                let hardware = ff::av_frame_alloc();
                (*hardware).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
                (*hardware).width = size.0 as i32;
                (*hardware).height = size.1 as i32;
                (*hardware).hw_frames_ctx = ff::av_buffer_ref(frames);
                hardware
            } else {
                ptr::null_mut()
            };

            Ok(Self {
                context,
                frames,
                scratch,
                hardware,
                packet: ff::av_packet_alloc(),
                size,
                format,
                rate,
                frame_rate: crate::media::encoding::rate::Controller::new(rate),
                force_keyframe: true,
            })
        }
    }

    pub(crate) fn format(&self) -> Format {
        self.format
    }

    /// 用外部给的硬件帧上下文建编码器：零拷贝那条路上，帧来自滤镜图的
    /// buffersink，编码器的 `hw_frames_ctx` 必须和那些帧同源。
    pub(crate) fn with_frames(
        frames: *mut ff::AVBufferRef,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        Self::create(Some(frames), size, format, rate)
    }

    /// 编一帧已经在显存里的画面（零拷贝）。`frame` 由调用方持有。
    pub(crate) fn encode_hardware(
        &mut self,
        frame: *mut ff::AVFrame,
        timestamp_100ns: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        let keyframe = keyframe || self.force_keyframe;
        self.force_keyframe = false;
        // Safe because the frame comes from a filter graph whose frames context
        // this encoder was built on, and it stays alive for the call.
        unsafe {
            (*frame).pts = timestamp_100ns;
            if keyframe {
                (*frame).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
            }
            check(ff::avcodec_send_frame(self.context, frame), "提交硬件帧")?;
            self.packets()
        }
    }

    /// 和 `vaapi::Encoder` 同一份契约：驱动决定实际上限，这里只给一个保守的天花板。
    pub(crate) fn maximum_size(format: Format) -> (u32, u32) {
        match format.codec {
            Codec::H264 | Codec::H265 => (4096, 4096),
            Codec::Av1 => (4096, 4096),
        }
    }

    /// 返回 `true` 表示质量档位变了，调用方要补一个关键帧。
    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        let key = self.rate.quality != rate.quality;
        self.rate = update.rate;
        self.frame_rate.commit(update);
        // Safe because the context is a live libavcodec context of this encoder.
        // Safe because the context is live and belongs to this encoder.
        unsafe {
            (*self.context).bit_rate = i64::from(self.rate.target.max(1));
            rate_control(self.context, self.rate);
            let name = CString::new("rc_mode")?;
            let code = ff::av_opt_set_int(
                self.context.cast(),
                name.as_ptr(),
                VBR,
                ff::AV_OPT_SEARCH_CHILDREN,
            );
            check(code, "更新码率控制")?;
        }
        Ok(key)
    }

    /// 一帧 BGRA 进去，若干包出来。
    pub(crate) fn encode(
        &mut self,
        image: &Image,
        timestamp_100ns: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        ensure!(image.size() == self.size, "采集画面尺寸与编码器不一致");
        ensure!(!self.scratch.is_null(), "这个编码器只用零拷贝那条路");
        let pixels = image.pixels()?;
        let keyframe = keyframe || self.force_keyframe;
        self.force_keyframe = false;
        // Safe because every handle below belongs to this encoder, and the
        // input slice is exactly one frame of BGRA.
        unsafe {
            check(ff::av_frame_make_writable(self.scratch), "输入帧不可写")?;
            let stride = (*self.scratch).linesize[0] as usize;
            ensure!(
                stride * self.size.1 as usize <= pixels.len() || stride >= self.size.0 as usize * 4,
                "输入帧的行距不合理"
            );
            let rows = (self.size.1 as usize).min(pixels.len() / (self.size.0 as usize * 4));
            for row in 0..rows {
                ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(row * self.size.0 as usize * 4),
                    (*self.scratch).data[0].add(row * stride),
                    self.size.0 as usize * 4,
                );
            }
            check(ff::av_hwframe_get_buffer(self.frames, self.hardware, 0), "取硬件帧")?;
            check(
                ff::av_hwframe_transfer_data(self.hardware, self.scratch, 0),
                "转换并上传到硬件帧",
            )?;
            (*self.hardware).pts = timestamp_100ns;
            if keyframe {
                (*self.hardware).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
            }
            (*self.hardware).flags |= ff::AV_FRAME_FLAG_KEY as i32;
            check(ff::avcodec_send_frame(self.context, self.hardware), "提交帧")?;
            ff::av_frame_unref(self.hardware);
            (*self.hardware).hw_frames_ctx = ff::av_buffer_ref(self.frames);
            self.packets()
        }
    }

    /// 把编码器已经产出的包全部取出来。
    ///
    /// Safe as long as the context is open and belongs to this encoder.
    unsafe fn packets(&mut self) -> Result<Vec<Encoded>> {
        let mut encoded = Vec::new();
        // Safe because the context and the packet belong to this encoder.
        unsafe {
            loop {
                let result = ff::avcodec_receive_packet(self.context, self.packet);
                if result == EAGAIN || result == EOF {
                    break;
                }
                check(result, "取码流")?;
                let data =
                    std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize)
                        .to_vec();
                // 包里的 pts 是 `time_base` 单位（1/fps）；下游一律按 100ns 记账。
                let timestamp_100ns = ff::av_rescale_q(
                    (*self.packet).pts,
                    (*self.context).time_base,
                    ff::AVRational {
                        num: 1,
                        den: 10_000_000,
                    },
                );
                encoded.push(Encoded {
                    data,
                    keyframe: (*self.packet).flags & (ff::AV_PKT_FLAG_KEY as i32) != 0,
                    timestamp_100ns,
                    is_new: true,
                    timing: None,
                    format: self.format,
                    color: Color::Sdr.space(None),
                });
                ff::av_packet_unref(self.packet);
            }
        }
        Ok(encoded)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Safe because every handle was created by this encoder and is released
        // exactly once, in an order that leaves no borrowed child behind.
        unsafe {
            ff::av_frame_free(&mut self.scratch);
            ff::av_frame_free(&mut self.hardware);
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.context);
            ff::av_buffer_unref(&mut self.frames);
        }
    }
}

/// 本线程共用一个 VA-API 设备。哪块 GPU 驱动着显示器由 `vaapi` 那边决定，
/// 这里沿用同一个判断，免得编码跑到另一块卡上。
fn device() -> *mut ff::AVBufferRef {
    use std::cell::RefCell;
    thread_local! {
        static DEVICE: RefCell<Option<usize>> = const { RefCell::new(None) };
    }
    DEVICE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(pointer) = *slot {
            return pointer as *mut ff::AVBufferRef;
        }
        let node = crate::platform::linux::vaapi::display_node()
            .map(|(node, _)| node)
            .unwrap_or_else(|| "/dev/dri/renderD128".to_owned());
        let node = CString::new(node).unwrap_or_default();
        let mut device: *mut ff::AVBufferRef = ptr::null_mut();
        // Safe because libavcodec owns the device it creates here and does not
        // keep the string.
        let code = unsafe {
            ff::av_hwdevice_ctx_create(
                &mut device,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        assert!(code >= 0 && !device.is_null(), "打开 VA-API 设备失败：{code}");
        *slot = Some(device as usize);
        device
    })
}

/// 码率控制：平均按 `target`、峰值按 `peak`，与 NVENC 那一侧同一个说法。
///
/// Safe as long as `context` is a live context of this encoder.
fn rate_control(context: *mut ff::AVCodecContext, rate: Rate) {
    unsafe {
        (*context).bit_rate = i64::from(rate.target.max(1));
        (*context).rc_max_rate = i64::from(rate.peak.max(rate.target));
        (*context).rc_buffer_size =
            (i64::from(rate.target) * 5 / i64::from(rate.fps.max(1))).max(i64::from(rate.target) / 8)
                as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::encoding::{Codec, QualityTarget};

    /// 真机采一路交给 libavcodec 编码，逐帧留下「编码器看到的亮度」与码流：
    /// ffmpeg 解回来逐帧对比编码器输入，就能看出参考帧会不会漂。
    /// `OPENUUYC_PROBE_OUTPUT=DP-1 cargo test --release --lib -- --ignored reports_the_libavcodec_stream --nocapture`
    #[test]
    #[ignore]
    fn reports_the_libavcodec_stream() {
        let screens = crate::platform::linux::capture::screens().unwrap();
        let wanted = std::env::var("OPENUUYC_PROBE_OUTPUT").unwrap_or_default();
        let screen = screens
            .iter()
            .find(|screen| screen.device_name == wanted)
            .unwrap_or(&screens[0])
            .clone();
        let (quality, maximum) = (4, (3840, 2160));
        let mut desktop = crate::platform::linux::capture::Desktop::open_selected(&screen).unwrap();
        let format = Format {
            codec: Codec::H264,
            chroma: 1,
            depth: 8,
        };
        let rate = Rate {
            target: 24_000_000,
            peak: 24_000_000,
            fps: 30,
            quality,
            quality_target: QualityTarget {
                bitrate: 24_000_000,
                fps: 30,
            },
        };
        let mut encoder = None;
        let (mut stream, mut luma) = (Vec::new(), Vec::new());
        let started = std::time::Instant::now();
        let mut index = 0i64;
        let (mut frames, mut keyframes, mut bytes) = (0, 0, 0usize);
        let mut sizes = Vec::new();
        while started.elapsed() < std::time::Duration::from_secs(15) {
            let Ok(Some(frame)) = desktop.next(33, quality, false, false, maximum) else {
                continue;
            };
            let size = (frame.width, frame.height);
            let encoder = match encoder.as_mut() {
                Some(encoder) => encoder,
                None => {
                    println!("编码尺寸 {size:?}（源 {:?}）", (screen.width, screen.height));
                    encoder = Some(Encoder::new(size, format, rate).unwrap());
                    encoder.as_mut().unwrap()
                }
            };
            let Ok(encoded) = encoder.encode(&frame.image, index * 333_333, index == 0) else {
                continue;
            };
            let pixels = frame.image.pixels().unwrap();
            let (width, height) = (size.0 as usize, size.1 as usize);
            let mut plane = vec![0u8; width * height * 3 / 2];
            let (y, uv) = plane.split_at_mut(width * height);
            let (u, v) = uv.split_at_mut(width * height / 4);
            yuv::bgra_to_yuv420(
                &mut yuv::YuvPlanarImageMut {
                    y_plane: yuv::BufferStoreMut::Borrowed(y),
                    y_stride: size.0,
                    u_plane: yuv::BufferStoreMut::Borrowed(u),
                    u_stride: size.0 / 2,
                    v_plane: yuv::BufferStoreMut::Borrowed(v),
                    v_stride: size.0 / 2,
                    width: size.0,
                    height: size.1,
                },
                &pixels,
                size.0 * 4,
                yuv::YuvRange::Limited,
                yuv::YuvStandardMatrix::Bt601,
                yuv::YuvConversionMode::Balanced,
            )
            .unwrap();
            for row in (0..height).step_by(8) {
                for column in (0..width).step_by(8) {
                    luma.push(y[row * width + column]);
                }
            }
            for output in &encoded {
                stream.extend_from_slice(&output.data);
                bytes += output.data.len();
                sizes.push((output.data.len(), output.keyframe));
                if output.keyframe {
                    keyframes += 1;
                }
            }
            frames += 1;
            index += 1;
        }
        println!("{frames} 帧，{keyframes} 个关键帧，{bytes} 字节");
        println!("前 8 帧 {:?}", sizes.iter().take(8).collect::<Vec<_>>());
        std::fs::write("/tmp/openuuyc-ffmpeg.h264", &stream).unwrap();
        std::fs::write("/tmp/openuuyc-ffmpeg-luma.raw", &luma).unwrap();
    }
}
