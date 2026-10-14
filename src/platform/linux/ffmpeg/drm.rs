//! 零拷贝：合成器画进显存的那块 dma-buf 直接进编码器。
//!
//! 链路是 `buffer(DRM PRIME) → hwmap(derive_device=vaapi) → scale_vaapi → buffersink`：
//! 导入、缩放、BGRA→NV12 全在 GPU 上，CPU 只递一个 fd 和若干结构体。缩放走 VPP
//! 是必须的——编码器最大 4096 宽，而这块屏是 5120。
//!
//! 之所以这里用 libavfilter 而 CPU 那条路不用：没有 GPU 缩放就没有零拷贝，
//! 而 VPP 的输入必须是 VA-API surface，把这步接起来是 FFmpeg 的强项。
//!
//! 输出自己的旋转也在这里做（`transpose_vaapi`）：合成器画的是它自己的画布，
//! 竖屏的屏上那是一张躺着的画面，而 VPP 转得动它，于是旋转屏也能零拷贝。
use anyhow::{Context as _, Result, bail, ensure};
use crate::platform::linux::capture::Rotation;
use ffmpeg_sys_next as ff;
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr;

/// `AVERROR(EAGAIN)`。
const EAGAIN: i32 = -ff::EAGAIN;
/// `AVERROR_EOF`。
const EOF: i32 = -0x2046_4f45;

fn check(code: i32, what: &str) -> Result<()> {
    ensure!(code >= 0, "{what}失败（libavcodec {code}）");
    Ok(())
}

/// 一帧 dma-buf 的描述：合成器画进去的那块缓冲。
pub(crate) struct Frame<'a> {
    pub fd: BorrowedFd<'a>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// DRM fourcc，和合成器报价的那个一致。
    pub format: u32,
}

/// 把 dma-buf 导入、缩放并转成 NV12 的一幕。
pub(crate) struct Scaler {
    graph: *mut ff::AVFilterGraph,
    source: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    /// 编码器要用的硬件帧上下文（取自 buffersink，`hw_frames_ctx` 必须同源）。
    frames: *mut ff::AVBufferRef,
    /// 导入用的 DRM 设备，帧描述符挂在它的 frames ctx 上。
    device: *mut ff::AVBufferRef,
    drm_frames: *mut ff::AVBufferRef,
    source_size: (u32, u32),
    target_size: (u32, u32),
    format: u32,
    /// 推进滤镜图的那张帧。它 `buf[0]` 上挂着当前这块 dma-buf 的描述符，每帧
    /// 换缓冲时换掉——采集那边是两块缓冲轮着用的。
    frame: *mut ff::AVFrame,
    output: *mut ff::AVFrame,
}

impl Scaler {
    /// `node` 是驱动显示器的 `card` 节点（dma-buf 就是在它上面分配的）。
    /// `rotation` 是这块屏的旋转：画面进缩放之前先在这里摆正，出来的就是这块屏
    /// 看到的形状，后面按显示尺寸算出来的目标尺寸才对得上。
    /// `target_size` 是摆正之后要缩到的尺寸。
    pub(crate) fn new(
        node: &str,
        frame: &Frame,
        rotation: Rotation,
        target_size: (u32, u32),
    ) -> Result<Self> {
        let (source_size, format) = ((frame.width, frame.height), frame.format);
        let node = CString::new(node)?;
        // Safe because libavcodec owns everything it creates here.
        unsafe {
            let mut device: *mut ff::AVBufferRef = ptr::null_mut();
            check(
                ff::av_hwdevice_ctx_create(
                    &mut device,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                    node.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "打开 DRM 设备",
            )?;
            let drm_frames = ff::av_hwframe_ctx_alloc(device);
            ensure!(!drm_frames.is_null(), "分配 DRM 帧上下文失败");
            {
                let context = (*drm_frames).data as *mut ff::AVHWFramesContext;
                (*context).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
                (*context).sw_format =
                    software_format(format).context("合成器报的这个像素格式没有对应的软件格式")?;
                (*context).width = source_size.0 as i32;
                (*context).height = source_size.1 as i32;
                (*context).initial_pool_size = 0;
            }
            check(ff::av_hwframe_ctx_init(drm_frames), "初始化 DRM 帧上下文")?;

            let graph = ff::avfilter_graph_alloc();
            ensure!(!graph.is_null(), "分配滤镜图失败");
            // 输入必须「先设参数、后初始化」：buffer 的 init 会检查硬件格式有没有
            // 配套的 hw_frames_ctx，而 avfilter_graph_create_filter 是边建边 init 的，
            // 那样这个检查永远过不去。
            let source = ff::avfilter_graph_alloc_filter(
                graph,
                ff::avfilter_get_by_name(c"buffer".as_ptr()),
                c"in".as_ptr(),
            );
            ensure!(!source.is_null(), "建立输入失败");
            let parameters = ff::av_buffersrc_parameters_alloc();
            ensure!(!parameters.is_null(), "分配输入参数失败");
            (*parameters).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*parameters).width = source_size.0 as i32;
            (*parameters).height = source_size.1 as i32;
            (*parameters).time_base = ff::AVRational { num: 1, den: 30 };
            (*parameters).frame_rate = ff::AVRational { num: 30, den: 1 };
            (*parameters).sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
            (*parameters).hw_frames_ctx = ff::av_buffer_ref(drm_frames);
            check(
                ff::av_buffersrc_parameters_set(source, parameters),
                "设置输入参数",
            )?;
            ff::av_free(parameters.cast());
            check(
                ff::avfilter_init_str(source, ptr::null()),
                "初始化输入",
            )?;

            let mut map = ptr::null_mut();
            let args = CString::new("derive_device=vaapi:mode=read")?;
            check(
                ff::avfilter_graph_create_filter(
                    &mut map,
                    ff::avfilter_get_by_name(c"hwmap".as_ptr()),
                    c"map".as_ptr(),
                    args.as_ptr(),
                    ptr::null_mut(),
                    graph,
                ),
                "建立硬件映射",
            )?;
            // VPP 上先摆正再缩放：转的是原始画布，缩放的输入已经是这块屏看到的
            // 那张画面，于是 `target_size` 可以直接按显示尺寸来算。
            let mut rotate = ptr::null_mut();
            let rotated = match rotation {
                Rotation::None => false,
                Rotation::Clockwise | Rotation::CounterClockwise => true,
            };
            if rotated {
                let direction = match rotation {
                    Rotation::CounterClockwise => "cclock",
                    _ => "clock",
                };
                let args = CString::new(format!("dir={direction}"))?;
                check(
                    ff::avfilter_graph_create_filter(
                        &mut rotate,
                        ff::avfilter_get_by_name(c"transpose_vaapi".as_ptr()),
                        c"rotate".as_ptr(),
                        args.as_ptr(),
                        ptr::null_mut(),
                        graph,
                    ),
                    "建立旋转",
                )?;
            }
            let mut scale = ptr::null_mut();
            let args = CString::new(format!(
                "w={}:h={}:format=nv12",
                target_size.0, target_size.1
            ))?;
            check(
                ff::avfilter_graph_create_filter(
                    &mut scale,
                    ff::avfilter_get_by_name(c"scale_vaapi".as_ptr()),
                    c"scale".as_ptr(),
                    args.as_ptr(),
                    ptr::null_mut(),
                    graph,
                ),
                "建立缩放",
            )?;
            let mut sink = ptr::null_mut();
            check(
                ff::avfilter_graph_create_filter(
                    &mut sink,
                    ff::avfilter_get_by_name(c"buffersink".as_ptr()),
                    c"out".as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    graph,
                ),
                "建立输出",
            )?;
            check(ff::avfilter_link(source, 0, map, 0), "连接输入与映射")?;
            if rotated {
                check(ff::avfilter_link(map, 0, rotate, 0), "连接映射与旋转")?;
                check(ff::avfilter_link(rotate, 0, scale, 0), "连接旋转与缩放")?;
            } else {
                check(ff::avfilter_link(map, 0, scale, 0), "连接映射与缩放")?;
            }
            check(ff::avfilter_link(scale, 0, sink, 0), "连接缩放与输出")?;
            check(ff::avfilter_graph_config(graph, ptr::null_mut()), "配置滤镜图")?;
            let frames = ff::av_buffersink_get_hw_frames_ctx(sink);
            ensure!(!frames.is_null(), "buffersink 没有给出硬件帧上下文");

            let frame_buffer = ff::av_frame_alloc();
            (*frame_buffer).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*frame_buffer).width = source_size.0 as i32;
            (*frame_buffer).height = source_size.1 as i32;
            (*frame_buffer).hw_frames_ctx = ff::av_buffer_ref(drm_frames);

            Ok(Self {
                graph,
                source,
                sink,
                frames,
                device,
                drm_frames,
                source_size,
                target_size,
                format,
                frame: frame_buffer,
                output: ff::av_frame_alloc(),
            })
        }
    }

    pub(crate) fn frames(&self) -> *mut ff::AVBufferRef {
        self.frames
    }

    /// 推一块 dma-buf 进去（内容由合成器写在里面），取出对应的一帧 NV12 硬件帧。
    /// 返回的帧由本结构体持有，下一次调用会被覆盖；编码器在返回之前已经读过它，
    /// 所以采集那边可以放心把这块缓冲交给合成器画下一帧。
    pub(crate) fn scale(&mut self, frame: &Frame, pts: i64) -> Result<*mut ff::AVFrame> {
        ensure!(
            (frame.width, frame.height, frame.stride, frame.format)
                == (
                    self.source_size.0,
                    self.source_size.1,
                    frame.stride,
                    self.format
                ),
            "换上来的是另一块形状不同的缓冲"
        );
        // Safe because every pointer below belongs to this scaler and stays
        // valid until it is dropped.
        unsafe {
            // 描述符挂在帧自己身上：换缓冲就是把旧的放掉、照新的再写一份。
            // 里面存的是借来的 fd——缓冲归采集那边所有，寿命比这个编码器长。
            ff::av_buffer_unref(&mut (*self.frame).buf[0]);
            let descriptor = ff::av_mallocz(size_of::<ff::AVDRMFrameDescriptor>())
                as *mut ff::AVDRMFrameDescriptor;
            ensure!(!descriptor.is_null(), "分配帧描述符失败");
            (*descriptor).nb_objects = 1;
            (*descriptor).objects[0].fd = frame.fd.as_raw_fd();
            (*descriptor).objects[0].size = frame.stride as usize * frame.height as usize;
            (*descriptor).objects[0].format_modifier = 0;
            (*descriptor).nb_layers = 1;
            (*descriptor).layers[0].format = frame.format;
            (*descriptor).layers[0].nb_planes = 1;
            (*descriptor).layers[0].planes[0].object_index = 0;
            (*descriptor).layers[0].planes[0].offset = 0;
            (*descriptor).layers[0].planes[0].pitch = frame.stride as isize;
            (*self.frame).data[0] = descriptor.cast();
            (*self.frame).buf[0] = ff::av_buffer_create(
                descriptor.cast(),
                size_of::<ff::AVDRMFrameDescriptor>(),
                Some(free_descriptor),
                ptr::null_mut(),
                0,
            );
            (*self.frame).pts = pts;
            // 同一块 dma-buf 每帧都推同一个 AVFrame，所以让滤镜图自己留引用，
            // 别把我们的帧取走（否则下一次推的就是一张空帧）。
            check(
                ff::av_buffersrc_add_frame_flags(
                    self.source,
                    self.frame,
                    ff::AV_BUFFERSRC_FLAG_KEEP_REF as i32,
                ),
                "提交 dma-buf",
            )?;
            // `av_buffersink_get_frame` 不会先放掉传进去的那张帧，而这里从头到尾
            // 用同一张 `output` 接结果：不显式放开，上一帧那块 VA surface 的引用
            // 就被覆盖掉了，永远回不到池子里。实测每帧漏 20 MB 显存——这块屏
            // 30 fps 下十几秒就能把 16 GB 显存吃干净，剩下的全溢出到系统内存，
            // 整台机器跟着卡死。编码器那边 `avcodec_send_frame` 自己留了引用，
            // 所以这里放掉不影响它已经拿去的那一帧。
            ff::av_frame_unref(self.output);
            let result = ff::av_buffersink_get_frame(self.sink, self.output);
            if result == EAGAIN || result == EOF {
                bail!("滤镜图没有给出画面（{result}）");
            }
            check(result, "取出缩放结果")?;
            ensure!(
                (*self.output).width == self.target_size.0 as i32
                    && (*self.output).height == self.target_size.1 as i32,
                "缩放结果的尺寸不对"
            );
            Ok(self.output)
        }
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // Safe because every handle was created here and is released once.
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.output);
            ff::avfilter_graph_free(&mut self.graph);
            ff::av_buffer_unref(&mut self.drm_frames);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

/// 帧释放时回调：`av_buffer_create` 要求我们给出释放函数。
unsafe extern "C" fn free_descriptor(_opaque: *mut libc::c_void, data: *mut u8) {
    // Safe because the buffer was created over an `av_mallocz`ed descriptor.
    unsafe { ff::av_free(data.cast()) };
}

/// DRM fourcc 对应的软件像素格式（ffmpeg 的 DRM 帧上下文认这一组配对）。
fn software_format(format: u32) -> Option<ff::AVPixelFormat> {
    // DRM_FORMAT_XRGB8888 / ARGB8888 / XBGR8888 / ABGR8888
    let pairs = [
        (0x3432_5258u32, ff::AVPixelFormat::AV_PIX_FMT_BGR0),
        (0x3432_5241, ff::AVPixelFormat::AV_PIX_FMT_BGRA),
        (0x3432_4258, ff::AVPixelFormat::AV_PIX_FMT_0BGR),
        (0x3432_4241, ff::AVPixelFormat::AV_PIX_FMT_ABGR),
    ];
    pairs
        .into_iter()
        .find(|(fourcc, _)| *fourcc == format)
        .map(|(_, pix)| pix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::linux::capture::dmabuf::{card_path, Buffer};
    use std::os::fd::AsFd;

    /// 测试图案：平坦底子 + 一块方块 + 一段锯齿。与别处的探针一致。
    fn pattern(size: (u32, u32)) -> Vec<u8> {
        let mut pixels = vec![0u8; size.0 as usize * size.1 as usize * 4];
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let (x, y) = (index as u32 % size.0, index as u32 / size.0);
            let value: u8 = if (300..=1100).contains(&x) && (200..=900).contains(&y) {
                40
            } else {
                (200 + (x / 32 % 40) as u8).saturating_sub((y / 512) as u8)
            };
            pixel.copy_from_slice(&[value, value.wrapping_add(20), value.wrapping_add(40), 255]);
        }
        pixels
    }

    /// 整条零拷贝链路：dma-buf → 导入 → GPU 缩放转 NV12 → **直接编码**，
    /// 全程 CPU 不碰像素。码流留在 /tmp 供 ffmpeg 复核。
    /// `cargo test --release --lib -- --ignored encodes_a_dmabuf_without_the_cpu --nocapture`
    #[test]
    #[ignore]
    fn encodes_a_dmabuf_without_the_cpu() {
        use crate::media::encoding::{Codec, Format, QualityTarget, Rate};
        let source = (5120u32, 2160u32);
        let target = (3840u32, 1620u32);
        let buffer =
            Buffer::allocate_format(source.0, source.1, 0x3432_5258).expect("分配显存缓冲失败");
        buffer.write_pattern(&pattern(source)).expect("写入图案失败");
        let node = card_path().unwrap();
        let frame = Frame {
            fd: buffer.prime.as_fd(),
            width: buffer.width,
            height: buffer.height,
            stride: buffer.stride,
            format: buffer.format,
        };
        let mut scaler = Scaler::new(&node, &frame, Rotation::None, target).expect("建立零拷贝链路失败");
        let format = Format {
            codec: Codec::H264,
            chroma: 1,
            depth: 8,
        };
        let rate = Rate {
            target: 24_000_000,
            peak: 24_000_000,
            fps: 30,
            quality: 4,
            quality_target: QualityTarget {
                bitrate: 24_000_000,
                fps: 30,
            },
        };
        let mut encoder =
            super::super::Encoder::with_frames(scaler.frames(), target, format, rate)
                .expect("用滤镜图的帧上下文建编码器失败");
        let started = std::time::Instant::now();
        let mut stream = Vec::new();
        let mut sizes = Vec::new();
        for index in 0..90i64 {
            let scaled = scaler.scale(&frame, index * 333_333).expect("缩放失败");
            let encoded = encoder
                .encode_hardware(scaled, index * 333_333, index == 0)
                .expect("编码失败");
            for output in &encoded {
                stream.extend_from_slice(&output.data);
                sizes.push((output.data.len(), output.keyframe));
            }
        }
        let elapsed = started.elapsed();
        println!(
            "90 帧（采集之外的一切：导入 + 缩放 + 转 NV12 + 编码）耗时 {elapsed:?} = {:.2} ms/帧",
            elapsed.as_secs_f64() * 1000.0 / 90.0
        );
        println!("前 6 帧 {:?}", &sizes[..sizes.len().min(6)]);
        println!("码流 {} 字节", stream.len());
        std::fs::write("/tmp/openuuyc-zerocopy.h264", &stream).unwrap();
    }

    /// 显存里的画布是合成器自己的方向：竖屏的屏上那是一张躺着的画面，得在
    /// VPP 上摆正。四个象限各涂一个亮度，转完位置对不对一眼就能断言。
    /// `cargo test --release --lib -- --ignored rotates_a_dmabuf_on_the_gpu --nocapture`
    #[test]
    #[ignore]
    fn rotates_a_dmabuf_on_the_gpu() {
        let (width, height) = (640u32, 400u32);
        // 左上 / 右上 / 左下 / 右下，四个差得很开的亮度。
        let values = [[40u8, 40, 40], [120, 120, 120], [200, 200, 200], [250, 250, 250]];
        let buffer = Buffer::allocate_format(width, height, 0x3432_5258).unwrap();
        let mut pattern = vec![0u8; width as usize * height as usize * 4];
        for (index, pixel) in pattern.chunks_exact_mut(4).enumerate() {
            let (x, y) = (index as u32 % width, index as u32 / width);
            let quadrant = usize::from(x >= width / 2) + 2 * usize::from(y >= height / 2);
            let [r, g, b] = values[quadrant];
            pixel.copy_from_slice(&[b, g, r, 255]);
        }
        buffer.write_pattern(&pattern).unwrap();

        // 顺时针之后画面变成 H×W：左下的象限转到左上，左上的转到右上。
        // `orient` 的表就是这么写的（`_90 => (y, last_y - x)`），这里要它一致。
        let clockwise = [2usize, 0, 3, 1];
        let counter = [1usize, 3, 0, 2];
        for (rotation, order) in [
            (Rotation::Clockwise, clockwise),
            (Rotation::CounterClockwise, counter),
        ] {
            let target = (height / 2 * 2, width / 2 * 2);
            let frame = Frame {
                fd: buffer.prime.as_fd(),
                width,
                height,
                stride: buffer.stride,
                format: buffer.format,
            };
            let node = card_path().unwrap();
            let mut scaler = Scaler::new(&node, &frame, rotation, target).unwrap();
            let scaled = scaler.scale(&frame, 0).unwrap();
            let mut sampled = [0u8; 4];
            // Safe because the frame belongs to the scaler and this software
            // frame is allocated right here.
            unsafe {
                let software = ff::av_frame_alloc();
                (*software).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                (*software).width = target.0 as i32;
                (*software).height = target.1 as i32;
                check(ff::av_frame_get_buffer(software, 0), "分配对照帧").unwrap();
                check(
                    ff::av_hwframe_transfer_data(software, scaled, 0),
                    "下载缩放结果",
                )
                .unwrap();
                let stride = (*software).linesize[0] as usize;
                let plane = (*software).data[0];
                for (quadrant, (x, y)) in [
                    (target.0 / 4, target.1 / 4),
                    (target.0 * 3 / 4, target.1 / 4),
                    (target.0 / 4, target.1 * 3 / 4),
                    (target.0 * 3 / 4, target.1 * 3 / 4),
                ]
                .into_iter()
                .enumerate()
                {
                    sampled[quadrant] =
                        *plane.add(y as usize * stride + x as usize);
                }
                ff::av_frame_free(&mut { software });
            }
            println!("{rotation:?} 四个象限（左上/右上/左下/右下）的亮度 {sampled:?}");
            let mut ranked: Vec<usize> = (0..4).collect();
            ranked.sort_by_key(|index| sampled[*index]);
            let mut expected: Vec<usize> = (0..4).collect();
            expected.sort_by_key(|index| values[order[*index]][0]);
            assert_eq!(
                ranked, expected,
                "{rotation:?} 的象限位置不对：画面转错了方向"
            );
        }
    }

    /// 零拷贝核心：dma-buf → 导入 → GPU 缩放 → NV12，全程不碰 CPU 像素。
    /// 用自己写进显存的确定图案与 CPU 那条路对照（两者都在同一份内容上跑）。
    /// `cargo test --release --lib -- --ignored scales_a_dmabuf_without_the_cpu --nocapture`
    #[test]
    #[ignore]
    fn scales_a_dmabuf_without_the_cpu() {
        let source = (5120u32, 2160u32);
        let target = (3840u32, 1620u32);
        let buffer = Buffer::allocate_format(source.0, source.1, 0x3432_5258)
            .expect("分配显存缓冲失败");
        // 确定图案：平坦底子 + 移动的方块 + 渐变，与别处的探针一致。
        let mut pattern = vec![0u8; source.0 as usize * source.1 as usize * 4];
        for (index, pixel) in pattern.chunks_exact_mut(4).enumerate() {
            let (x, y) = (index as u32 % source.0, index as u32 / source.0);
            let value: u8 = if (300..=1100).contains(&x) && (200..=900).contains(&y) {
                40
            } else {
                (200 + (x / 32 % 40) as u8).saturating_sub((y / 512) as u8)
            };
            pixel.copy_from_slice(&[value, value.wrapping_add(20), value.wrapping_add(40), 255]);
        }
        buffer.write_pattern(&pattern).expect("写入图案失败");

        let node = card_path().unwrap();
        let frame = Frame {
            fd: buffer.prime.as_fd(),
            width: buffer.width,
            height: buffer.height,
            stride: buffer.stride,
            format: buffer.format,
        };
        let mut scaler = Scaler::new(&node, &frame, Rotation::None, target).expect("建立零拷贝链路失败");
        println!("滤镜图建立成功，输出帧上下文 {:?}", scaler.frames());

        let started = std::time::Instant::now();
        let mut luma = Vec::new();
        let mut frames = 0;
        while frames < 30 {
            let Ok(scaled) = scaler.scale(&frame, frames * 333_333) else {
                continue;
            };
            luma.clear();
            // 只为验证：下载回内存看一眼（真实路径不需要这一步）。
            // Safe because the frame belongs to the scaler and this software
            // frame is allocated right here.
            unsafe {
                let software = ff::av_frame_alloc();
                (*software).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                (*software).width = target.0 as i32;
                (*software).height = target.1 as i32;
                check(ff::av_frame_get_buffer(software, 0), "分配对照帧").unwrap();
                match ff::av_hwframe_transfer_data(software, scaled, 0) {
                    0 => {
                        let stride = (*software).linesize[0] as usize;
                        let plane = (*software).data[0];
                        for row in (0..target.1 as usize).step_by(8) {
                            let line = std::slice::from_raw_parts(plane.add(row * stride), target.0 as usize);
                            for column in (0..target.0 as usize).step_by(8) {
                                luma.push(line[column]);
                            }
                        }
                    }
                    code => println!("下载失败 {code}"),
                }
                ff::av_frame_free(&mut { software });
            }
            frames += 1;
        }
        println!("{frames} 帧（含每帧下载），耗时 {:?}", started.elapsed());

        // CPU 那条路：同一份图案，先缩放再转 NV12。
        let started = std::time::Instant::now();
        let scaled_cpu = crate::platform::linux::capture::scale_for_probe(&pattern, source, target);
        println!("CPU 缩放一次 {:?}", started.elapsed());
        let (width, height) = (target.0 as usize, target.1 as usize);
        let mut plane = vec![0u8; width * height * 3 / 2];
        let (y, uv) = plane.split_at_mut(width * height);
        let (u, v) = uv.split_at_mut(width * height / 4);
        yuv::bgra_to_yuv420(
            &mut yuv::YuvPlanarImageMut {
                y_plane: yuv::BufferStoreMut::Borrowed(y),
                y_stride: target.0,
                u_plane: yuv::BufferStoreMut::Borrowed(u),
                u_stride: target.0 / 2,
                v_plane: yuv::BufferStoreMut::Borrowed(v),
                v_stride: target.0 / 2,
                width: target.0,
                height: target.1,
            },
            &scaled_cpu,
            target.0 * 4,
            yuv::YuvRange::Limited,
            yuv::YuvStandardMatrix::Bt601,
            yuv::YuvConversionMode::Balanced,
        )
        .unwrap();
        let mut reference = Vec::new();
        for row in (0..height).step_by(8) {
            for column in (0..width).step_by(8) {
                reference.push(y[row * width + column]);
            }
        }
        let count = luma.len().min(reference.len());
        let squared: f64 = luma[..count]
            .iter()
            .zip(&reference[..count])
            .map(|(a, b)| f64::from(a.abs_diff(*b)).powi(2))
            .sum();
        println!(
            "与 CPU 那条路（同内容、不同缩放核）的 Y 平面 RMSE：{:.2}",
            (squared / count as f64).sqrt()
        );
        std::fs::write("/tmp/openuuyc-zerocopy-luma.raw", &luma).unwrap();
        std::fs::write("/tmp/openuuyc-cpu-luma.raw", &reference).unwrap();
    }

    /// 显存不许漏：同一块 dma-buf 一直过滤镜图（可选再喂编码器），显存必须是平的。
    ///
    /// 这条以前真的漏过：`av_buffersink_get_frame` 不会先放掉传进去的帧，而这里
    /// 从头到尾用同一张 `output` 接结果，于是每帧漏 20 MB——这块屏 30 fps 下十几秒
    /// 就把 16 GB 显存吃干净，剩下的溢出到系统内存，整台机器卡死。
    ///
    /// 上限是硬的：超了就直接失败退出，绝不会像上次那样把显存吃光。
    /// `cargo test --release --lib -- --ignored the_zerocopy_path_leaks_no_vram --nocapture`
    #[test]
    #[ignore]
    fn the_zerocopy_path_leaks_no_vram() {
        use crate::media::encoding::{Codec, QualityTarget};
        use crate::platform::linux::ffmpeg::{Encoder, Format, Rate};
        let encoding = std::env::var("OPENUUYC_PROBE_LEAK").is_ok_and(|v| v == "encode");
        let source = (5120u32, 2160u32);
        let target = (3840u32, 1620u32);
        let buffer =
            Buffer::allocate_format(source.0, source.1, 0x3432_5258).expect("分配显存缓冲失败");
        // 有点内容的图案：编码器真出码流，不是一屏纯色。
        let mut pattern = vec![0u8; source.0 as usize * source.1 as usize * 4];
        for (index, pixel) in pattern.chunks_exact_mut(4).enumerate() {
            let (x, y) = (index as u32 % source.0, index as u32 / source.0);
            let value = ((x / 7) ^ (y / 5)) as u8;
            pixel.copy_from_slice(&[value, value.wrapping_add(37), value.wrapping_add(91), 255]);
        }
        buffer.write_pattern(&pattern).expect("写入图案失败");

        let frame = Frame {
            fd: buffer.prime.as_fd(),
            width: buffer.width,
            height: buffer.height,
            stride: buffer.stride,
            format: buffer.format,
        };
        let node = card_path().unwrap();
        let mut scaler =
            Scaler::new(&node, &frame, Rotation::None, target).expect("建立零拷贝链路失败");
        let rate = Rate {
            target: 24_000_000,
            peak: 24_000_000,
            fps: 30,
            quality: 4,
            quality_target: QualityTarget {
                bitrate: 24_000_000,
                fps: 30,
            },
        };
        let mut encoder = encoding.then(|| {
            Encoder::with_frames(
                scaler.frames(),
                target,
                Format {
                    codec: Codec::H264,
                    chroma: 1,
                    depth: 8,
                },
                rate,
            )
            .unwrap()
        });

        let start = vram_mb();
        // 池子、滤镜图、编码器自己的 surface 都在最初几帧里建起来，那部分不算漏。
        let mut run = |round: u32, scaler: &mut Scaler, encoder: &mut Option<Encoder>| {
            let scaled = scaler.scale(&frame, i64::from(round) * 333_333).unwrap();
            if let Some(encoder) = encoder.as_mut() {
                encoder
                    .encode_hardware(scaled, i64::from(round) * 333_333, round == 0)
                    .unwrap();
            }
            // 真的漏起来是每秒几百 MB，别等它把机器拖垮。
            assert!(vram_mb() < start + 600, "显存涨过头了，停下来");
        };
        for round in 0..10 {
            run(round, &mut scaler, &mut encoder);
        }
        let settled = vram_mb();
        for round in 10..60 {
            run(round, &mut scaler, &mut encoder);
        }
        let after = vram_mb();
        println!(
            "{}：跑 10 帧安定到 {settled} MB，再跑 50 帧到 {after} MB（共涨 {} MB）",
            if encoding { "缩放+编码" } else { "只缩放  " },
            after as i64 - settled as i64,
        );
        // 60 帧里漏 20 MB/帧就是 1 GB，阈值 32 MB 足够灵敏也不会误报。
        assert!(
            after < settled + 32,
            "50 帧里显存涨了 {} MB，又在漏",
            after - settled
        );
    }

    fn vram_mb() -> u64 {
        let mut total = 0u64;
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path().join("device/mem_info_vram_used");
            if let Ok(value) = std::fs::read_to_string(&path)
                && let Ok(value) = value.trim().parse::<u64>()
            {
                total += value;
            }
        }
        total / 1_048_576
    }

}
