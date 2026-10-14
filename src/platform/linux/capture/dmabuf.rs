//! 采集用的显存缓冲：合成器把画面直接画进去，CPU 一个像素都不碰。
//!
//! 走的是 DRM 的 dumb buffer（线性排布，所有驱动都认）加 `zwp_linux_dmabuf_v1`：
//! 前者给出 fd 与行距，后者把它包成一个 `wl_buffer` 交给 `zwlr_screencopy`。
//! 本机实测共享内存那条路每帧要 CPU 读 44 MB（5K），dma-buf 这条不需要。
use anyhow::{Context as _, Result, bail, ensure};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use wayland_client::protocol::wl_buffer;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

/// `DRM_FORMAT_ARGB8888`，也就是小端的 BGRA —— VA-API 的 `VA_FOURCC_BGRA` 认它。
pub(crate) const ARGB8888: u32 = 0x3432_5241;
/// 线性排布。dumb buffer 一定是它。
const LINEAR: u64 = 0;

/// ioctl 的请求号：`_IOWR('d', nr, type)`，其中 type 的长度决定 size 那一段。
const fn request(number: u64, size: u64) -> libc::c_ulong {
    const WRITE: u64 = 1;
    const READ: u64 = 2;
    ((READ | WRITE) << 30) | (size << 16) | ((b'd' as u64) << 8) | number
}

#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct DestroyDumb {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

const CREATE_DUMB: libc::c_ulong = request(0xb2, size_of::<CreateDumb>() as u64);
const MAP_DUMB: libc::c_ulong = request(0xb3, size_of::<MapDumb>() as u64);
const DESTROY_DUMB: libc::c_ulong = request(0xb4, size_of::<DestroyDumb>() as u64);
const HANDLE_TO_FD: libc::c_ulong = request(0x2d, size_of::<PrimeHandle>() as u64);

/// 一块交给合成器画的显存缓冲，以及引用它需要的那个 fd。
pub(crate) struct Buffer {
    /// 由 `PRIME_HANDLE_TO_FD` 得到，交给 `wl_buffer` 之后合成器自己持有引用。
    pub prime: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub modifier: u64,
    pub size: u64,
    handle: u32,
    card: OwnedFd,
}

impl Buffer {
    /// 在这块 GPU 上分配一块 `width`x`height` 的 ARGB8888 线性缓冲。
    pub(crate) fn allocate(width: u32, height: u32) -> Result<Self> {
        Self::allocate_format(width, height, ARGB8888)
    }

    /// 同上，但指定 fourcc（合成器在 `linux_dmabuf` 事件里报的就是它要的那个）。
    pub(crate) fn allocate_format(width: u32, height: u32, format: u32) -> Result<Self> {
        let card = card_node()?;
        let mut create = CreateDumb {
            height,
            width,
            bpp: 32,
            ..Default::default()
        };
        // Safe because `card` is an open DRM node and `create` is a plain
        // struct of the layout that ioctl expects.
        let result = unsafe { libc::ioctl(card.as_raw_fd(), CREATE_DUMB, &mut create) };
        if result < 0 {
            bail!(
                "分配显存缓冲失败：{}",
                std::io::Error::last_os_error()
            );
        }
        ensure!(create.handle != 0 && create.size != 0, "驱动没有给出缓冲句柄");
        let mut prime = PrimeHandle {
            handle: create.handle,
            flags: 0,
            fd: -1,
        };
        // Safe because the handle came from this node and the struct layout is
        // the one the ioctl expects.
        let result = unsafe { libc::ioctl(card.as_raw_fd(), HANDLE_TO_FD, &mut prime) };
        if result < 0 || prime.fd < 0 {
            let error = std::io::Error::last_os_error();
            destroy(card.as_raw_fd(), create.handle);
            bail!("导出显存缓冲的文件描述符失败：{error}");
        }
        // Safe because the kernel just created this descriptor for us and it is
        // not owned by anything else.
        let prime = unsafe { OwnedFd::from_raw_fd(prime.fd) };
        Ok(Self {
            prime,
            width,
            height,
            stride: create.pitch,
            format,
            modifier: LINEAR,
            size: create.size,
            handle: create.handle,
            card,
        })
    }

    /// 把它包成合成器认的 `wl_buffer`。
    pub(crate) fn wrap<S>(&self, dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, queue: &QueueHandle<S>) -> wl_buffer::WlBuffer
    where
        S: wayland_client::Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>
            + 'static,
    {
        let params = dmabuf.create_params(queue, ());
        params.add(
            self.prime.as_fd(),
            0,
            0,
            self.stride,
            (self.modifier >> 32) as u32,
            (self.modifier & 0xffff_ffff) as u32,
        );
        // `create_immed` 是同步的那一支：直接拿到 wl_buffer，不用等 created 事件。
        let buffer = params.create_immed(
            self.width as i32,
            self.height as i32,
            self.format,
            zwp_linux_buffer_params_v1::Flags::empty(),
            queue,
            (),
        );
        params.destroy();
        buffer
    }

    /// 只给探针用：把内容映射回内存看一眼。生产路径不需要这个（也不需要 CPU 读）。
    #[cfg(test)]
    pub(crate) fn read_back(&self) -> Result<Vec<u8>> {
        let mut map = MapDumb {
            handle: self.handle,
            ..Default::default()
        };
        // Safe because the handle is this buffer's and the struct matches.
        let result = unsafe { libc::ioctl(self.card.as_raw_fd(), MAP_DUMB, &mut map) };
        ensure!(result >= 0, "映射显存缓冲失败");
        // Safe because the driver gave us this offset for this buffer.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.card.as_raw_fd(),
                map.offset as i64,
            )
        };
        ensure!(pointer != libc::MAP_FAILED, "mmap 显存缓冲失败");
        // Safe because the mapping is `size` bytes and live until munmap.
        let pixels = unsafe {
            std::slice::from_raw_parts(pointer as *const u8, self.size as usize).to_vec()
        };
        // Safe because the mapping came from mmap above.
        unsafe { libc::munmap(pointer, self.size as usize) };
        Ok(pixels)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        destroy(self.card.as_raw_fd(), self.handle);
    }
}

fn destroy(card: RawFd, handle: u32) {
    if handle == 0 {
        return;
    }
    let mut request = DestroyDumb { handle, pad: 0 };
    // Safe because the handle is ours and the struct matches the ioctl.
    unsafe { libc::ioctl(card, DESTROY_DUMB, &mut request) };
}

/// 驱动着显示器的那块 GPU 的 `card` 节点。dumb buffer 只能在 `cardN` 上分配
/// （`renderD*` 上带 `DRM_AUTH` 的 ioctl 会 EACCES）。
fn card_node() -> Result<OwnedFd> {
    let render = crate::platform::linux::vaapi::display_node()
        .map(|(node, _)| node)
        .unwrap_or_else(|| "/dev/dri/renderD128".to_owned());
    let name = render.rsplit('/').next().unwrap_or_default().to_owned();
    let target = std::fs::canonicalize(format!("/sys/class/drm/{name}/device")).ok();
    for entry in std::fs::read_dir("/sys/class/drm")
        .context("读取 /sys/class/drm 失败")?
        .flatten()
    {
        let candidate = entry.file_name().to_string_lossy().into_owned();
        if !candidate.starts_with("card") || candidate.contains('-') {
            continue;
        }
        let device = std::fs::canonicalize(entry.path().join("device")).ok();
        if target.is_some() && device != target {
            continue;
        }
        let path = format!("/dev/dri/{candidate}");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("打开 {path} 失败"));
        match file {
            Ok(file) => return Ok(file.into()),
            Err(error) => {
                tracing::debug!(%error, path, "这个 card 节点打不开，换一个");
            }
        }
    }
    bail!("没有可用的 DRM card 节点")
}
