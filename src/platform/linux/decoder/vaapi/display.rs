//! VA-API display, decode session and surface pool.
//!
//! One display per render node, opened once and shared: `vaInitialize` talks to
//! the driver and is far too expensive to repeat per session.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cros_libva::{
    Config, Context, Display, Surface, UsageHint, VAConfigAttrib, VAConfigAttribType, VAEntrypoint,
    VAImageFormat, VAProfile, VASurfaceID,
};

use crate::media::decode_api::DecodeError;

/// NV12 (8-bit 4:2:0) is the only layout this backend reads back today.
const RT_FORMAT_YUV420: u32 = cros_libva::VA_RT_FORMAT_YUV420;

/// Open the first render node whose driver can decode, and keep it.
pub(super) fn display() -> Result<Rc<Display>, DecodeError> {
    thread_local! {
        static DISPLAY: RefCell<Option<Rc<Display>>> = const { RefCell::new(None) };
    }
    DISPLAY.with(|cell| {
        if let Some(display) = cell.borrow().as_ref() {
            return Ok(Rc::clone(display));
        }
        let display = open_display()?;
        *cell.borrow_mut() = Some(Rc::clone(&display));
        Ok(display)
    })
}

fn open_display() -> Result<Rc<Display>, DecodeError> {
    // 这里的 `Display::open` 会扫到 NVIDIA 节点并 dlopen 出 nvidia VA 驱动，
    // 先把 CUDA 初始化走完，免得驱动构造器里的首次 cuInit 和 NvFBC/NVENC 互等。
    crate::platform::linux::cuda::initialize_before_va();
    // `Display::open` walks the render nodes itself; the explicit loop is the
    // fallback for systems where the first node is not the decoding one.
    if let Some(display) = Display::open()
        && !decode_profiles(&display).is_empty()
    {
        log_vendor(&display);
        return Ok(display);
    }
    for index in 128..136u32 {
        let path = format!("/dev/dri/renderD{index}");
        let Some(display) = open_render_node(&path) else {
            continue;
        };
        if decode_profiles(&display).is_empty() {
            continue;
        }
        tracing::debug!(path, "VA-API display opened");
        log_vendor(&display);
        return Ok(display);
    }
    Err(DecodeError::Unsupported)
}

/// 打开一个 render node。libva 2.15 以前不把 `nvidia-drm` 内核驱动映射到
/// nvidia-vaapi-driver，NVIDIA 节点上的 `vaInitialize` 因此失败；这时点名驱动
/// 再试一次，等价于用户自己设 `LIBVA_DRIVER_NAME=nvidia`。
fn open_render_node(path: &str) -> Option<Rc<Display>> {
    match Display::open_drm_display(path) {
        Ok(display) => Some(display),
        Err(cros_libva::OpenDrmDisplayError::VaInitialize(_)) if nvidia_node(path) => {
            let display = Display::open_drm_display_with_driver(path, Some(c"nvidia")).ok()?;
            tracing::debug!(path, "VA-API 按 NVIDIA 驱动名重试成功");
            Some(display)
        }
        Err(_) => None,
    }
}

/// `/sys/class/drm/renderD128/device/vendor` 为 0x10de 即 NVIDIA。
fn nvidia_node(path: &str) -> bool {
    let node = path.rsplit('/').next().unwrap_or_default();
    std::fs::read_to_string(format!("/sys/class/drm/{node}/device/vendor"))
        .is_ok_and(|vendor| vendor.trim() == "0x10de")
}

fn log_vendor(display: &Rc<Display>) {
    if let Ok(vendor) = display.query_vendor_string() {
        tracing::info!(vendor, "VA-API driver");
    }
}

/// Profiles the driver decodes, i.e. that expose the VLD entrypoint.
fn decode_profiles(display: &Rc<Display>) -> Vec<VAProfile::Type> {
    let Ok(profiles) = display.query_config_profiles() else {
        return Vec::new();
    };
    profiles
        .into_iter()
        .filter(|profile| {
            display
                .query_config_entrypoints(*profile)
                .is_ok_and(|entrypoints| entrypoints.contains(&VAEntrypoint::VAEntrypointVLD))
        })
        .collect()
}

/// Whether this driver can decode `profile` at `width` x `height`.
pub(super) fn supports(profile: VAProfile::Type, width: u32, height: u32) -> bool {
    let Ok(display) = display() else {
        return false;
    };
    if !decode_profiles(&display).contains(&profile) {
        return false;
    }
    let mut attributes = [
        attribute(VAConfigAttribType::VAConfigAttribRTFormat),
        attribute(VAConfigAttribType::VAConfigAttribMaxPictureWidth),
        attribute(VAConfigAttribType::VAConfigAttribMaxPictureHeight),
    ];
    if display
        .get_config_attributes(profile, VAEntrypoint::VAEntrypointVLD, &mut attributes)
        .is_err()
    {
        return false;
    }
    let value = |kind: VAConfigAttribType::Type| {
        attributes
            .iter()
            .find(|attribute| attribute.type_ == kind)
            .map(|attribute| attribute.value)
            .filter(|value| *value != cros_libva::VA_ATTRIB_NOT_SUPPORTED)
    };
    if value(VAConfigAttribType::VAConfigAttribRTFormat)
        .is_some_and(|formats| formats & RT_FORMAT_YUV420 == 0)
    {
        return false;
    }
    // A driver that does not report a limit accepts whatever it is given.
    let within = |kind: VAConfigAttribType::Type, wanted: u32| {
        value(kind).is_none_or(|limit| limit == 0 || limit >= wanted)
    };
    within(VAConfigAttribType::VAConfigAttribMaxPictureWidth, width)
        && within(VAConfigAttribType::VAConfigAttribMaxPictureHeight, height)
}

const fn attribute(kind: VAConfigAttribType::Type) -> VAConfigAttrib {
    VAConfigAttrib {
        type_: kind,
        value: 0,
    }
}

/// A decode context plus the surfaces its pictures are written into.
pub(super) struct Pool {
    pub(super) context: Rc<Context>,
    pub(super) width: u32,
    pub(super) height: u32,
    /// The NV12 image format used to copy pictures out of their surfaces.
    pub(super) nv12: VAImageFormat,
    /// 这池子的 surface 是否按「要导出成 dma-buf」分配（带上 `USAGE_HINT_EXPORT`，
    /// 驱动据此避开 DCC 这类导出去也没法采样的布局），以及解码后是否真的导出。
    pub(super) zero_copy: bool,
    surfaces: Vec<Option<Surface<()>>>,
    ids: Vec<VASurfaceID>,
    /// Surfaces still needed as references, by index.
    reserved: Vec<bool>,
    /// 已经借给渲染器、还没还回来的 surface。`Surface` 不是 `Send`（里头是
    /// `Rc<Display>`），所以它一步都不离开解码线程；跨线程走的只有租约里的释放标志。
    checked_out: Vec<(usize, Surface<()>, Arc<LeaseState>)>,
    #[allow(dead_code, reason = "Owns the config for the context's lifetime.")]
    config: Config,
}

/// 一帧借出去的 surface 的租约。
///
/// 解码器把 surface 导成 dma-buf 交给渲染器之后，**不能**立刻把它还给池子：下一帧
/// 可能就解到同一张 surface 上，正在显示的那一帧会跟着变。渲染器握着这把租约，直到
/// 那一帧被换掉才放手；解码线程在下一次解码前用 [`Pool::reclaim`] 收回来。
#[derive(Debug)]
pub(super) struct SurfaceLease(Arc<LeaseState>);

#[derive(Debug)]
struct LeaseState {
    released: AtomicBool,
}

impl Drop for SurfaceLease {
    fn drop(&mut self) {
        self.0.released.store(true, Ordering::Release);
    }
}

impl Pool {
    pub(super) fn new(
        profile: VAProfile::Type,
        width: u32,
        height: u32,
        count: usize,
        zero_copy: bool,
    ) -> Result<Self, DecodeError> {
        let display = display()?;
        let config = display
            .create_config(
                vec![VAConfigAttrib {
                    type_: VAConfigAttribType::VAConfigAttribRTFormat,
                    value: RT_FORMAT_YUV420,
                }],
                profile,
                VAEntrypoint::VAEntrypointVLD,
            )
            .map_err(|error| {
                tracing::debug!(%error, "VA-API config unavailable");
                DecodeError::Unsupported
            })?;
        // EXPORT 这一位只在真要导出时才带：驱动（Mesa）据此避开 DCC 这类布局，
        // 但也可能因此少一点解码优化，不导出时没必要付这份代价。
        let hint = if zero_copy {
            UsageHint::USAGE_HINT_DECODER | UsageHint::USAGE_HINT_EXPORT
        } else {
            UsageHint::USAGE_HINT_DECODER
        };
        let surfaces = display
            .create_surfaces(RT_FORMAT_YUV420, None, width, height, Some(hint), vec![(); count])
            .map_err(|error| {
                tracing::debug!(%error, count, "VA-API surface allocation failed");
                DecodeError::HardwareFailure
            })?;
        let context = display
            .create_context(&config, width, height, Some(&surfaces), true)
            .map_err(|error| {
                tracing::debug!(%error, "VA-API context creation failed");
                DecodeError::HardwareFailure
            })?;
        let nv12 = display
            .query_image_formats()
            .map_err(|error| {
                tracing::debug!(%error, "VA-API image formats unavailable");
                DecodeError::HardwareFailure
            })?
            .into_iter()
            .find(|format| format.fourcc == cros_libva::VA_FOURCC_NV12)
            .ok_or_else(|| {
                tracing::debug!("VA-API driver offers no NV12 image format");
                DecodeError::Unsupported
            })?;
        let ids = surfaces.iter().map(Surface::id).collect();
        Ok(Self {
            context,
            width,
            height,
            nv12,
            zero_copy,
            checked_out: Vec::new(),
            reserved: vec![false; surfaces.len()],
            surfaces: surfaces.into_iter().map(Some).collect(),
            ids,
            config,
        })
    }

    pub(super) fn id(&self, index: usize) -> VASurfaceID {
        self.ids[index]
    }

    /// Take a surface that is neither in flight nor holding a reference frame.
    pub(super) fn take(&mut self) -> Result<(usize, Surface<()>), DecodeError> {
        let index = self
            .surfaces
            .iter()
            .enumerate()
            .position(|(index, surface)| surface.is_some() && !self.reserved[index])
            .ok_or(DecodeError::Backend)?;
        let surface = self.surfaces[index].take().ok_or(DecodeError::Backend)?;
        Ok((index, surface))
    }

    pub(super) fn put(&mut self, index: usize, surface: Surface<()>) {
        self.surfaces[index] = Some(surface);
    }

    /// 把渲染器已经放掉的 surface 收回来。每次解码前调一次。
    pub(super) fn reclaim(&mut self) {
        for (index, surface, lease) in std::mem::take(&mut self.checked_out) {
            if lease.released.load(Ordering::Acquire) {
                self.surfaces[index] = Some(surface);
            } else {
                self.checked_out.push((index, surface, lease));
            }
        }
    }

    /// 借出一张 surface，返回给渲染器跟着的那把租约。
    pub(super) fn check_out(&mut self, index: usize, surface: Surface<()>) -> SurfaceLease {
        let state = Arc::new(LeaseState {
            released: AtomicBool::new(false),
        });
        self.checked_out.push((index, surface, Arc::clone(&state)));
        SurfaceLease(state)
    }

    /// Mark exactly the surfaces the reference lists still name.
    pub(super) fn reserve_only(&mut self, indices: &[usize]) {
        for (index, reserved) in self.reserved.iter_mut().enumerate() {
            *reserved = indices.contains(&index);
        }
    }

    pub(super) fn release_all(&mut self) {
        self.reserved
            .iter_mut()
            .for_each(|reserved| *reserved = false);
    }
}

#[cfg(test)]
mod dmabuf_probe {
    use super::*;
    use cros_libva::{Image, VA_FOURCC_NV12};

    /// 观看端零拷贝唯一的未知数：解出来的 surface 导出成 dma-buf 之后，能不能按
    /// 「单平面 R8 / R8G8 + 显式平面布局」重新导进 Vulkan，导出来的纹素又对不对。
    /// surface 上的修饰符是驱动自己挑的（AMD 上是带 DCC 的），radv 未必肯按同一个
    /// 修饰符把单平面视图导回来，而导错了的样子是花屏不是报错，所以要真的比像素。
    ///
    /// `cargo test --release --lib -- --ignored --nocapture vaapi_surface_exports_into_wgpu`
    #[test]
    #[ignore = "需要 render node 上与 wgpu 同一张卡，且驱动支持 VK_EXT_image_drm_format_modifier"]
    fn vaapi_surface_exports_into_wgpu() {
        const WIDTH: u32 = 256;
        const HEIGHT: u32 = 128;
        let display = display().expect("VA-API 打不开");
        let format = display
            .query_image_formats()
            .expect("查不到图像格式")
            .into_iter()
            .find(|format| format.fourcc == VA_FOURCC_NV12)
            .expect("驱动不提供 NV12");
        let surfaces = display
            .create_surfaces(
                RT_FORMAT_YUV420,
                None,
                WIDTH,
                HEIGHT,
                // 解码器现在只报 DECODER。EXPORT 这一位是给 Mesa 看的：带上它驱动
                // 就不会分配 DCC 这类导出去也没法采样的布局。
                Some(UsageHint::USAGE_HINT_DECODER | UsageHint::USAGE_HINT_EXPORT),
                vec![(); 1],
            )
            .expect("分配 surface 失败");
        let surface = &surfaces[0];

        // 图案同时依赖 x 与 y：行距猜错、原点偏移都会读出来不对。
        let expected: Vec<u8> = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| (x.wrapping_mul(3).wrapping_add(y)) as u8))
            .collect();
        {
            let mut image = Image::create_from(surface, format, (WIDTH, HEIGHT), (WIDTH, HEIGHT))
                .expect("vaCreateImage 失败");
            let info = *image.image();
            let (offset, pitch) = (info.offsets[0] as usize, info.pitches[0] as usize);
            let (chroma_offset, chroma_pitch) = (info.offsets[1] as usize, info.pitches[1] as usize);
            let data = image.as_mut();
            for y in 0..HEIGHT as usize {
                let row = offset + y * pitch;
                data[row..row + WIDTH as usize]
                    .copy_from_slice(&expected[y * WIDTH as usize..][..WIDTH as usize]);
            }
            for y in 0..(HEIGHT as usize).div_ceil(2) {
                let row = chroma_offset + y * chroma_pitch;
                data[row..row + WIDTH as usize].fill(0x80);
            }
        }

        let prime = surface.export_prime().expect("导出 dma-buf 失败");
        eprintln!(
            "导出：fourcc=0x{:08x} {}x{} 对象={} 层={}",
            prime.fourcc,
            prime.width,
            prime.height,
            prime.objects.len(),
            prime.layers.len()
        );
        for (index, object) in prime.objects.iter().enumerate() {
            eprintln!(
                "  对象 {index}：修饰符 0x{:x}，{} 字节",
                object.drm_format_modifier, object.size
            );
        }
        let layer = &prime.layers[0];
        eprintln!(
            "  层：drm_format=0x{:08x} 平面数={}",
            layer.drm_format, layer.num_planes
        );
        for plane in 0..layer.num_planes as usize {
            eprintln!(
                "    平面 {plane}：偏移 {} 行距 {}",
                layer.offset[plane], layer.pitch[plane]
            );
        }

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("没有 Vulkan 适配器");
        eprintln!("适配器：{:?}", adapter.get_info());
        let features = adapter.features() & wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
        eprintln!("导入特性：{features:?}");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("dmabuf-probe"),
            required_features: features,
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }))
        .expect("创建 Vulkan 设备失败");

        // 一块 dma-buf 可以导出多次，每次导入都会接管 fd，所以要一份一份 dup。
        let import = |plane: usize,
                      size: (u32, u32),
                      format: wgpu::TextureFormat|
         -> Result<wgpu::Texture, String> {
            let object = &prime.objects[layer.object_index[plane] as usize];
            let descriptor = wgpu::hal::TextureDescriptor {
                label: Some("dmabuf-plane"),
                size: wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUses::COPY_SRC,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            };
            let handle = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }
                .ok_or("设备不是 Vulkan")?;
            let hal = unsafe {
                handle.texture_from_dmabuf_fd(
                    object.fd.try_clone().map_err(|e| e.to_string())?,
                    &descriptor,
                    object.drm_format_modifier,
                    layer.pitch[plane] as u64,
                    layer.offset[plane] as u64,
                )
            }
            .map_err(|error| format!("texture_from_dmabuf_fd: {error:?}"))?;
            Ok(unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                    hal,
                    &wgpu::TextureDescriptor {
                        label: Some("dmabuf-plane"),
                        size: descriptor.size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    },
                    // 内容由 VA-API 写好了、布局也已经是对的，跳过首次转换以免
                    // wgpu 按「未初始化」把这一份丢掉。
                    wgpu::TextureUses::COPY_SRC,
                )
            })
        };

        let luma = import(0, (WIDTH, HEIGHT), wgpu::TextureFormat::R8Unorm)
            .unwrap_or_else(|error| panic!("Y 平面导入失败：{error}"));
        eprintln!("Y 平面导入成功：R8");
        match import(
            1,
            (WIDTH / 2, HEIGHT / 2),
            wgpu::TextureFormat::Rg8Unorm,
        ) {
            Ok(_) => eprintln!("色度平面导入成功：R8G8"),
            Err(error) => eprintln!("色度平面导入失败（Y 能导就还能走单平面的路）：{error}"),
        }

        let bytes_per_row = WIDTH.div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(bytes_per_row) * u64::from(HEIGHT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &luma,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(HEIGHT),
                },
            },
            wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("等待 GPU 失败");
        let mapped = slice.get_mapped_range().expect("映射回读缓冲失败");
        let wrong = (0..HEIGHT as usize)
            .filter(|y| {
                mapped[y * bytes_per_row as usize..][..WIDTH as usize]
                    != expected[y * WIDTH as usize..][..WIDTH as usize]
            })
            .count();
        eprintln!("读回 Y 平面：{} 行与图案不符（共 {HEIGHT} 行）", wrong);
        assert_eq!(wrong, 0, "导入的纹理内容不对，说明平面布局或修饰符没对上");
    }
}
