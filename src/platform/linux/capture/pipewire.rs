//! A PipeWire video stream consumer, loaded at run time.
//!
//! `libpipewire-0.3.so.0` is opened with dlopen rather than linked: every
//! desktop with the ScreenCast portal has it, but its development headers (and
//! the Rust bindings generated from them) track newer releases than long-term
//! distributions ship. The few entry points used here have been stable since
//! PipeWire 0.3, and the SPA format descriptions are written by hand.
//!
//! Frames are requested as linear DMA-BUF when the compositor offers one, and
//! as shared memory otherwise. A shared-memory frame is copied into BGRA here
//! and handed to the capture thread; a DMA-BUF frame is imported as it is (the
//! fd is `dup`ed) and read by the encoder straight off the GPU. A DMA-BUF is
//! only offered when the encoder on this machine can import one — asking for
//! it otherwise would leave the capture holding frames nobody can read.
use super::Picture;
use super::dmabuf;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

// --- SPA / PipeWire ABI (spa-0.2, pipewire-0.3) --------------------------------

#[repr(C)]
struct SpaList {
    next: *mut c_void,
    prev: *mut c_void,
}
#[repr(C)]
struct SpaCallbacks {
    funcs: *const c_void,
    data: *mut c_void,
}
#[repr(C)]
struct SpaHook {
    link: SpaList,
    cb: SpaCallbacks,
    removed: Option<unsafe extern "C" fn(*mut SpaHook)>,
    private: *mut c_void,
}
type Callback = Option<unsafe extern "C" fn(*mut c_void)>;
#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Callback,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    process: Callback,
    drained: Callback,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Callback,
}
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
}
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut SpaMeta,
    datas: *mut SpaData,
}
#[repr(C)]
struct SpaMeta {
    kind: u32,
    size: u32,
    data: *mut c_void,
}
#[repr(C)]
struct SpaData {
    /// `spa_data_type`。门户交付的缓冲带的是单一类型，不是请求时用的位掩码，
    /// 所以这里直接和 `DATA_MEM_DMABUF` 比。
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}
#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}
#[repr(C)]
struct SpaMetaRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

const STREAM_EVENTS_VERSION: u32 = 2;
const DIRECTION_INPUT: u32 = 0;
const FLAG_AUTOCONNECT: u32 = 1 << 0;
// `FLAG_MAP_BUFFERS` maps every buffer except DMA-BUF, which is exactly the
// split wanted: shared memory is mapped for the CPU copy, and DMA-BUF is left
// as a file descriptor for the encoder to import.
const FLAG_MAP_BUFFERS: u32 = 1 << 2;
const STATE_ERROR: c_int = -1;
const STATE_UNCONNECTED: c_int = 0;
const STATE_STREAMING: c_int = 3;

/// `spa_pod_prop` 的标志位。`DONT_FIXATE` 加在 modifier 上就是「这是一串候选，
/// 你自己挑」——门户正是看到它才会考虑 DMA-BUF。
const SPA_POD_PROP_FLAG_DONT_FIXATE: u32 = 1 << 4;

const TYPE_ID: u32 = 3;
const TYPE_INT: u32 = 4;
const TYPE_LONG: u32 = 5;
const TYPE_RECTANGLE: u32 = 10;
const TYPE_FRACTION: u32 = 11;
const TYPE_OBJECT: u32 = 15;
const TYPE_CHOICE: u32 = 19;
const OBJECT_FORMAT: u32 = 0x40003;
const OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const OBJECT_PARAM_META: u32 = 0x40005;
const PARAM_ENUM_FORMAT: u32 = 3;
const PARAM_FORMAT: u32 = 4;
const PARAM_BUFFERS: u32 = 5;
const PARAM_META: u32 = 6;
const FORMAT_MEDIA_TYPE: u32 = 1;
const FORMAT_MEDIA_SUBTYPE: u32 = 2;
const FORMAT_VIDEO_FORMAT: u32 = 0x20001;
/// DRM 的格式修饰符。SPA 文档说它「只用于 DMA-BUF，别的缓冲类型不要带」——这
/// 就是全部的开关含义：不带这个键，门户无论如何都协商共享内存。
const FORMAT_VIDEO_MODIFIER: u32 = 0x20002;
const FORMAT_VIDEO_SIZE: u32 = 0x20003;
const FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const MEDIA_TYPE_VIDEO: u32 = 2;
const MEDIA_SUBTYPE_RAW: u32 = 1;
const VIDEO_RGBX: u32 = 7;
const VIDEO_BGRX: u32 = 8;
const VIDEO_RGBA: u32 = 11;
const VIDEO_BGRA: u32 = 12;

/// `DRM_FORMAT_MOD_LINEAR`。Scaler 只会按线性导入，所以只列它。
const DRM_MOD_LINEAR: u64 = 0;

/// `DRM_FORMAT_XRGB8888`：ARGB8888 的无 alpha 版（同样是小端 BGRA 排布）。
/// ARGB8888 本身复用 `dmabuf::ARGB8888`，不再重定义一遍。
const XRGB8888: u32 = 0x3432_5258;

const BUFFERS_COUNT: u32 = 1;
const BUFFERS_DATA_TYPE: u32 = 6;
const META_TYPE: u32 = 1;
const META_SIZE: u32 = 2;
const META_VIDEO_CROP: u32 = 2;
const DATA_MEM_PTR: u32 = 1;
const DATA_MEM_FD: u32 = 2;
/// `SPA_DATA_DmaBuf` 是枚举里的第四个，位号是 3。直接拿枚举值当位号会要成
/// `SPA_DATA_MemId`，合成器不提供，两边的掩码没有交集，链接直接
/// `error alloc buffers: Invalid argument`。
const DATA_MEM_DMABUF: u32 = 3;
const CHOICE_RANGE: u32 = 1;
const CHOICE_ENUM: u32 = 3;
const CHOICE_FLAGS: u32 = 4;

struct Api {
    _library: libloading::Library,
    thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    context_destroy: unsafe extern "C" fn(*mut c_void),
    context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    properties_new: unsafe extern "C" fn(*const c_char, ...) -> *mut c_void,
    stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    stream_add_listener:
        unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const StreamEvents, *mut c_void),
    stream_connect:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const c_void, u32) -> c_int,
    stream_update_params: unsafe extern "C" fn(*mut c_void, *mut *const c_void, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    stream_destroy: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn load() -> Result<Api> {
    // SAFETY: loading the system PipeWire client library; the symbols below
    // are declared with their C signatures from pipewire-0.3.
    unsafe {
        let library = libloading::Library::new("libpipewire-0.3.so.0")
            .context("没有找到 PipeWire（libpipewire-0.3.so.0）")?;
        macro_rules! symbol {
            ($name:literal) => {
                *library
                    .get(concat!($name, "\0").as_bytes())
                    .with_context(|| format!("PipeWire 缺少 {}", $name))?
            };
        }
        let init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) = symbol!("pw_init");
        init(std::ptr::null_mut(), std::ptr::null_mut());
        Ok(Api {
            thread_loop_new: symbol!("pw_thread_loop_new"),
            thread_loop_get_loop: symbol!("pw_thread_loop_get_loop"),
            thread_loop_start: symbol!("pw_thread_loop_start"),
            thread_loop_stop: symbol!("pw_thread_loop_stop"),
            thread_loop_destroy: symbol!("pw_thread_loop_destroy"),
            thread_loop_lock: symbol!("pw_thread_loop_lock"),
            thread_loop_unlock: symbol!("pw_thread_loop_unlock"),
            context_new: symbol!("pw_context_new"),
            context_destroy: symbol!("pw_context_destroy"),
            context_connect_fd: symbol!("pw_context_connect_fd"),
            core_disconnect: symbol!("pw_core_disconnect"),
            properties_new: symbol!("pw_properties_new"),
            stream_new: symbol!("pw_stream_new"),
            stream_add_listener: symbol!("pw_stream_add_listener"),
            stream_connect: symbol!("pw_stream_connect"),
            stream_update_params: symbol!("pw_stream_update_params"),
            stream_dequeue_buffer: symbol!("pw_stream_dequeue_buffer"),
            stream_queue_buffer: symbol!("pw_stream_queue_buffer"),
            stream_disconnect: symbol!("pw_stream_disconnect"),
            stream_destroy: symbol!("pw_stream_destroy"),
            _library: library,
        })
    }
}

// --- SPA pod encoding ------------------------------------------------------------

/// Builds one SPA pod in an 8-byte aligned buffer, the layout `spa_pod_builder`
/// produces: every pod is `{u32 size, u32 type}` then its body, padded to 8.
#[derive(Default)]
struct Pod(Vec<u8>);
impl Pod {
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }
    fn pad(&mut self) {
        while self.0.len() % 8 != 0 {
            self.0.push(0);
        }
    }
    fn value(&mut self, kind: u32, body: &[u32]) {
        self.u32((body.len() * 4) as u32);
        self.u32(kind);
        for word in body {
            self.u32(*word);
        }
        self.pad();
    }
    /// A choice of `Long` values, the form SPA uses for DRM modifiers.
    fn choice_long(&mut self, choice: u32, values: &[u64]) {
        self.u32((16 + values.len() * 8) as u32);
        self.u32(TYPE_CHOICE);
        self.u32(choice);
        self.u32(0);
        self.u32(8);
        self.u32(TYPE_LONG);
        for value in values {
            self.u64(*value);
        }
        self.pad();
    }
    /// A choice of `values`, each `child_words` 32-bit words of `child_type`.
    fn choice(&mut self, choice: u32, child_type: u32, child_words: usize, values: &[u32]) {
        self.u32((16 + values.len() * 4) as u32);
        self.u32(TYPE_CHOICE);
        self.u32(choice);
        self.u32(0);
        self.u32((child_words * 4) as u32);
        self.u32(child_type);
        for word in values {
            self.u32(*word);
        }
        self.pad();
    }
    /// An object with properties written by `props`, each started with `key`.
    fn object(kind: u32, id: u32, props: impl FnOnce(&mut Self)) -> Self {
        let mut pod = Self::default();
        pod.u32(0);
        pod.u32(TYPE_OBJECT);
        pod.u32(kind);
        pod.u32(id);
        props(&mut pod);
        let size = (pod.0.len() - 8) as u32;
        pod.0[..4].copy_from_slice(&size.to_ne_bytes());
        pod
    }
    fn key(&mut self, key: u32) {
        self.key_flags(key, 0);
    }
    /// A property key with `spa_pod_prop` flags. `DONT_FIXATE` is how a client
    /// says "here is a list, you choose" -- the portal looks for exactly that on
    /// the modifier before it will consider DMA-BUF.
    fn key_flags(&mut self, key: u32, flags: u32) {
        self.u32(key);
        self.u32(flags);
    }
    /// The bytes, copied into 8-byte aligned storage as SPA requires.
    fn aligned(&self) -> Vec<u64> {
        let mut words = vec![0u64; self.0.len().div_ceil(8)];
        // SAFETY: `words` has at least as many bytes as the pod.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.0.as_ptr(),
                words.as_mut_ptr().cast::<u8>(),
                self.0.len(),
            );
        }
        words
    }
}

/// `dmabuf` 为真时，这里额外带上 `SPA_FORMAT_VIDEO_modifier`——门户只有在格式
/// 里看到修饰符时才肯谈 DMA-BUF，不带就一律协商共享内存。修饰符只列线性一种：
/// 非线性的画面我们导不进 Scaler，谈成了也是花屏。
fn enum_format(dmabuf: bool) -> Pod {
    Pod::object(OBJECT_FORMAT, PARAM_ENUM_FORMAT, |p| {
        p.key(FORMAT_MEDIA_TYPE);
        p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
        p.key(FORMAT_MEDIA_SUBTYPE);
        p.value(TYPE_ID, &[MEDIA_SUBTYPE_RAW]);
        if dmabuf {
            p.key_flags(FORMAT_VIDEO_MODIFIER, SPA_POD_PROP_FLAG_DONT_FIXATE);
            p.choice_long(CHOICE_ENUM, &[DRM_MOD_LINEAR]);
        }
        // 格式用的是 SPA 的 `spa_video_format` 枚举，不是 DRM fourcc：门户自己
        // 在两边做映射 (`pwFromDrmFourcc`)，这里塞 fourcc 永远匹配不上。
        // 显存那条路只列 BGRX/BGRA：只有它们对应能导进 Scaler 的 AR24/XR24
        // （VA-API 只认这两个 32 位 BGRA 排布），谈成 RGBA/RGBX 会被 `read_dmabuf`
        // 逐帧拒掉。共享内存那条路上 RGBX/RGBA 有意义（`read_shm` 会把红蓝换回来），
        // 所以照旧列全，别把这条路也收窄了。
        p.key(FORMAT_VIDEO_FORMAT);
        let formats: &[u32] = if dmabuf {
            &[VIDEO_BGRX, VIDEO_BGRA]
        } else {
            &[VIDEO_BGRX, VIDEO_BGRX, VIDEO_BGRA, VIDEO_RGBX, VIDEO_RGBA]
        };
        p.choice(CHOICE_ENUM, TYPE_ID, 1, formats);
        p.key(FORMAT_VIDEO_SIZE);
        p.choice(
            CHOICE_RANGE,
            TYPE_RECTANGLE,
            2,
            &[1920, 1080, 1, 1, 16384, 16384],
        );
        p.key(FORMAT_VIDEO_FRAMERATE);
        p.choice(CHOICE_RANGE, TYPE_FRACTION, 2, &[60, 1, 0, 1, 1000, 1]);
    })
}

/// The buffer types to offer, which must agree with the negotiated format.
///
/// libpipewire 会把多位掩码折叠成单一类型，而 `MemPtr` 胜出：把 `DmaBuf` 和
/// `MemPtr` 放一起，链接会按 CPU 可映射的内存协商，但门户那边已经认定了
/// DMA-BUF 格式，分配随即 `error alloc buffers: Invalid argument` 失败。所以
/// 显存那支只报 DMA-BUF，共享内存那支照旧报 MemPtr|MemFd，由协商出来的格式
/// （有没有修饰符）决定用哪支——这个回退是门户自己做的，我们只负责答对。
fn buffers(dmabuf: bool) -> Pod {
    Pod::object(OBJECT_PARAM_BUFFERS, PARAM_BUFFERS, |p| {
        // 显存帧要留到编码器用完才还回去，池子浅了会被抽干（合成器报
        // `Out of buffers`，然后整条流重来、反复重fixate 修饰符，最后退回共享
        // 内存），所以显存那支要深一点。写成范围让合成器自己定上限。
        p.key(BUFFERS_COUNT);
        let count = if dmabuf { 8 } else { 4 };
        p.choice(CHOICE_RANGE, TYPE_INT, 1, &[count, 2, 32]);
        p.key(BUFFERS_DATA_TYPE);
        let mask = if dmabuf {
            1 << DATA_MEM_DMABUF
        } else {
            (1 << DATA_MEM_PTR) | (1 << DATA_MEM_FD)
        };
        p.choice(CHOICE_FLAGS, TYPE_INT, 1, &[mask]);
    })
}

fn crop_meta() -> Pod {
    Pod::object(OBJECT_PARAM_META, PARAM_META, |p| {
        p.key(META_TYPE);
        p.value(TYPE_ID, &[META_VIDEO_CROP]);
        p.key(META_SIZE);
        p.value(TYPE_INT, &[size_of::<SpaMetaRegion>() as u32]);
    })
}

/// The negotiated format from a Format object.
///
/// `modifier` 只有在合成器同意 DMA-BUF 时才有；纯共享内存的流不带它。真正决定
/// 一块缓冲走哪条路的是**缓冲自己的** `spa_data.type`，不是这里的修饰符：门户
/// 谈崩退回共享内存之后，格式里的修饰符还留着。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NegotiatedFormat {
    format: u32,
    width: u32,
    height: u32,
    modifier: Option<u64>,
}

/// The negotiated format from a Format object.
///
/// # Safety
/// `pod` must point at a valid SPA pod.
unsafe fn parse_format(pod: *const c_void) -> Option<NegotiatedFormat> {
    let read =
        |offset: usize| unsafe { pod.cast::<u8>().add(offset).cast::<u32>().read_unaligned() };
    let read64 =
        |offset: usize| unsafe { pod.cast::<u8>().add(offset).cast::<u64>().read_unaligned() };
    let size = read(0) as usize;
    if read(4) != TYPE_OBJECT || size < 8 {
        return None;
    }
    let end = 8 + size;
    let mut offset = 16;
    let (mut format, mut width, mut height) = (None, None, None);
    let mut modifier = None;
    while offset + 16 <= end {
        let key = read(offset);
        let value_size = read(offset + 8) as usize;
        let mut value_type = read(offset + 12);
        let mut value = offset + 16;
        if value_type == TYPE_CHOICE {
            // A fixed property may still come wrapped in a None choice; its
            // first element is the value.
            value_type = read(value + 12);
            value += 16;
        }
        match (key, value_type) {
            (FORMAT_VIDEO_FORMAT, TYPE_ID) => format = Some(read(value)),
            (FORMAT_VIDEO_MODIFIER, TYPE_LONG) => modifier = Some(read64(value)),
            (FORMAT_VIDEO_SIZE, TYPE_RECTANGLE) => {
                width = Some(read(value));
                height = Some(read(value + 4));
            }
            _ => {}
        }
        offset += 16 + value_size.next_multiple_of(8);
    }
    Some(NegotiatedFormat {
        format: format?,
        width: width?,
        height: height?,
        modifier,
    })
}

// --- Stream ----------------------------------------------------------------------

/// One captured frame as the stream delivered it.
pub(super) enum Delivered {
    /// 系统内存里紧密打包的 BGRA。
    Shm {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// 合成器递过来的显存缓冲，已经 `dup` 成我们自己的 fd，编码器直接导入。
    Dmabuf(Arc<dmabuf::Buffer>),
}

impl Delivered {
    /// 系统内存里的像素；显存画面就地映射回来读一遍。只给「零拷贝那条路走不通」
    /// 的后备路径用：一帧几十毫秒，正常编解码不会走到这里。
    pub(super) fn into_cpu(self) -> Result<Picture> {
        match self {
            Delivered::Shm {
                width,
                height,
                pixels,
            } => Ok(Picture {
                width,
                height,
                pixels,
            }),
            Delivered::Dmabuf(buffer) => {
                let raw = buffer.read_back()?;
                let row = buffer.width as usize * 4;
                let mut pixels = Vec::with_capacity(row * buffer.height as usize);
                for line in raw
                    .chunks_exact(buffer.stride as usize)
                    .take(buffer.height as usize)
                {
                    pixels.extend_from_slice(&line[..row]);
                }
                Ok(Picture {
                    width: buffer.width,
                    height: buffer.height,
                    pixels,
                })
            }
        }
    }
}

/// 门户那块缓冲的租约：内容还在合成器的池子里，编码器读完之前不能还回去。
///
/// `pw_stream_queue_buffer` 文档保证可从别的线程调用，所以归还挂在 `Drop` 上，
/// 由真正用完这帧的那条线程（采集线程）执行。租约持有 `Session` 而不是裸句柄，
/// 于是一帧活过 [`Stream`]（被缓存、或换后端时还留在手上）也能还给活着的流。
struct BufferLease {
    session: Arc<Session>,
    /// 门户交出来的 `pw_buffer`，**不是**它里面的 `spa_buffer`：`queue_buffer`
    /// 靠前者查自己的账，给错的指针只会拿到 `-EINVAL`。
    buffer: *mut PwBuffer,
}

// SAFETY: 归还只发生在 Drop 里一次，且 Session 保证活到那时。
unsafe impl Send for BufferLease {}
unsafe impl Sync for BufferLease {}

impl Drop for BufferLease {
    fn drop(&mut self) {
        unsafe { (self.session.api.stream_queue_buffer)(self.session.stream, self.buffer) };
    }
}

#[derive(Default)]
struct State {
    picture: Option<Delivered>,
    sequence: u64,
    format: Option<NegotiatedFormat>,
    streaming: bool,
    error: Option<String>,
    /// 上一次问过的缓冲类型：`Some(true)` 是显存，`Some(false)` 是系统内存，
    /// 第一次之前是 `None`。
    buffers_requested: Option<bool>,
    /// 跑回调的那条线程（PipeWire 的 loop 线程），第一个回调跑起来时记下。
    /// `Session::drop` 靠它判断「是不是正在自己的 loop 线程上拆自己」。
    loop_thread: Option<std::thread::ThreadId>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// 流的句柄，好让回调发出的租约能把它留住。用 `Weak`：`Session` 拥有跑回调
    /// 的那个线程，强引用就成了环。
    session: Mutex<Weak<Session>>,
}

/// What the loop thread's callbacks reach through their `data` pointer.
struct Callbacks {
    api: &'static Api,
    stream: *mut c_void,
    shared: Arc<Shared>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// 记下跑回调的线程。所有回调（`on_*`）都跑在同一个 loop 线程上，第一个跑起来
/// 时记一次就够；之后 `Session::drop` 用它避开「在自己的 loop 线程上停自己」。
fn mark_loop_thread(state: &mut State) {
    if state.loop_thread.is_none() {
        state.loop_thread = Some(std::thread::current().id());
    }
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let mut guard = lock(&callbacks.shared.state);
    mark_loop_thread(&mut guard);
    match state {
        STATE_STREAMING => guard.streaming = true,
        STATE_ERROR => {
            let text = if error.is_null() {
                "未知错误".to_owned()
            } else {
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            guard.error = Some(format!("PipeWire 画面流出错：{text}"));
        }
        STATE_UNCONNECTED if guard.streaming => {
            guard.error = Some("屏幕共享已结束".into());
        }
        _ => {}
    }
    drop(guard);
    callbacks.shared.changed.notify_all();
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    if param.is_null() || id != PARAM_FORMAT {
        return;
    }
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let format = unsafe { parse_format(param) };
    let mut state = lock(&callbacks.shared.state);
    mark_loop_thread(&mut state);
    state.format = format;
    // 判据要和逐帧校验（`read_dmabuf`）一致：只有线性修饰符才去要 DMA-BUF。
    // 若按「有修饰符且非 INVALID」就要 DMA-BUF，而门户 fixate 出的是非线性排布，
    // 就会每帧都被 `read_dmabuf` 拒掉；缓冲区类型又已经定死，于是既没有画面、
    // 也永远退不回共享内存（stall）。
    let dmabuf = format.and_then(|format| format.modifier) == Some(DRM_MOD_LINEAR);
    // 缓冲类型只在种类真的变了才重发。每次格式变化都重发会让门户重新 fixate
    // 一遍修饰符，fixate 不成它就退回共享内存并报成又一次格式变化——两边来回
    // 转，门户的 fixate 次数几十秒就上千，流永远定不下来，之后每帧都编成空的。
    if state.buffers_requested == Some(dmabuf) {
        drop(state);
        tracing::debug!(?format, dmabuf, "PipeWire screencast format renegotiated");
        return;
    }
    state.buffers_requested = Some(dmabuf);
    drop(state);
    callbacks.shared.changed.notify_all();
    tracing::debug!(?format, dmabuf, "PipeWire screencast format negotiated");
    let buffers = buffers(dmabuf).aligned();
    let meta = crop_meta().aligned();
    let mut params = [buffers.as_ptr().cast::<c_void>(), meta.as_ptr().cast()];
    unsafe {
        (callbacks.api.stream_update_params)(callbacks.stream, params.as_mut_ptr(), 2);
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let api = callbacks.api;
    // Only the newest buffer matters; return any older ones unread.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let buffer = unsafe { (api.stream_dequeue_buffer)(callbacks.stream) };
        if buffer.is_null() {
            break;
        }
        if !newest.is_null() {
            unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
        }
        newest = buffer;
    }
    if newest.is_null() {
        return;
    }
    let format = {
        let mut state = lock(&callbacks.shared.state);
        mark_loop_thread(&mut state);
        state.format
    };
    let picture =
        format.and_then(|format| unsafe { read_picture(&callbacks.shared, newest, format) });
    let holds = matches!(picture, Some(Delivered::Dmabuf(_)));
    if holds {
        // 同时只留一帧显存：池子本来就浅，上一帧还没被取走又要一帧会把池子抽干，
        // 之后合成器报 `Out of buffers`、整条流重来。丢这一帧只是少一帧，留着流
        // 才要紧。丢掉的那帧随 `Drop` 把缓冲还回去。
        let mut state = lock(&callbacks.shared.state);
        if state.picture.is_some() {
            drop(state);
            return;
        }
        state.picture = picture;
        state.sequence += 1;
        drop(state);
        callbacks.shared.changed.notify_all();
        return;
    }
    // 系统内存的画面此刻已经拷完，缓冲直接还回去。
    unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
    if let Some(picture) = picture {
        let mut state = lock(&callbacks.shared.state);
        state.picture = Some(picture);
        state.sequence += 1;
        drop(state);
        callbacks.shared.changed.notify_all();
    }
}

/// Copy a buffer's pixels out as tightly packed BGRA, or import a DMA-BUF.
///
/// # Safety
/// `pw` must be a buffer the stream just dequeued.
unsafe fn read_picture(
    shared: &Shared,
    pw: *mut PwBuffer,
    negotiated: NegotiatedFormat,
) -> Option<Delivered> {
    let NegotiatedFormat {
        format,
        width,
        height,
        modifier,
    } = negotiated;
    let spa = unsafe { &*(*pw).buffer };
    if spa.n_datas == 0 || spa.datas.is_null() {
        return None;
    }
    // 缓冲自己的类型说了算，不是格式里的修饰符：门户谈崩之后格式还带着它。
    // 交付过来的缓冲带的是单一类型，不是请求时的位掩码。
    if unsafe { (*spa.datas).kind } == DATA_MEM_DMABUF {
        return unsafe { read_dmabuf(shared, pw, spa, format, width, height, modifier) };
    }
    read_shm(spa, format, width, height)
}

/// 把门户交出来的 DMA-BUF 直接交给编码器，一个像素都不碰。
///
/// # Safety
/// `pw` 是刚 dequeued 的缓冲；返回的画面会把它扣在池子里直到 drop。
unsafe fn read_dmabuf(
    shared: &Shared,
    pw: *mut PwBuffer,
    spa: &SpaBuffer,
    format: u32,
    width: u32,
    height: u32,
    modifier: Option<u64>,
) -> Option<Delivered> {
    // 只认线性（`DRM_FORMAT_MOD_LINEAR`）。Scaler 只会按线性导入，别的排布读出来
    // 就是花屏；门户没给修饰符（`None`/`INVALID`）也一样没法导。
    if modifier != Some(DRM_MOD_LINEAR) {
        tracing::debug!(?modifier, "DMA-BUF 不是线性排布，这一帧不采");
        return None;
    }
    // VA-API 只认 AR24/XR24 这两种 32 位 BGRA 排布。
    let Some(fourcc) = drm_fourcc(format) else {
        tracing::debug!(format, "DMA-BUF 的 SPA 格式不是 BGRA/BGRX，这一帧不采");
        return None;
    };
    let data = unsafe { &*spa.datas };
    if data.fd < 0 || data.chunk.is_null() {
        return None;
    }
    let chunk = unsafe { &*data.chunk };
    let stride = if chunk.stride > 0 {
        chunk.stride as u32
    } else {
        width * 4
    };
    // Scaler 导入时把平面偏移写死成 0（见 `ffmpeg::drm`），偏了就会读错地方。
    // 单平面线性缓冲实测都是 0，不是就当作没协商到。
    if chunk.offset != 0 {
        tracing::debug!(offset = chunk.offset, "DMA-BUF 平面有偏移，这一帧不采");
        return None;
    }
    if width == 0 || height == 0 || stride < width * 4 {
        tracing::debug!(width, height, stride, "DMA-BUF 尺寸不合理，这一帧不采");
        return None;
    }
    // 流的句柄：租约靠它保证归还时流还活着。取不到（流正在销毁）就不采这一帧。
    let session = shared
        .session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()?;
    // 门户的 fd 随 `pw_buffer` 回收，不能长期攥着；dup 一份自己拿着。
    // SAFETY: `data.fd` 是刚交付的缓冲上的有效描述符。
    let prime = unsafe { BorrowedFd::borrow_raw(data.fd as RawFd) }
        .try_clone_to_owned()
        .ok()?;
    let size = u64::from(stride) * u64::from(height);
    let buffer = Arc::new(dmabuf::Buffer::import(
        prime,
        width,
        height,
        stride,
        fourcc,
        DRM_MOD_LINEAR,
        size,
        Some(Box::new(BufferLease {
            session,
            buffer: pw,
        })),
    ));
    tracing::debug!(fourcc, width, height, stride, "DMA-BUF 画面已导入");
    Some(Delivered::Dmabuf(buffer))
}

/// SPA 的 `spa_video_format` 对应到哪个 DRM fourcc。
///
/// 门户在交付时把合成器的 DRM fourcc 映射成 SPA 枚举（`pwFromDrmFourcc`），所以
/// 这里是那个映射反过来读。只认 BGRA/BGRX：它们才是 VA-API 能导入的 AR24/XR24。
fn drm_fourcc(format: u32) -> Option<u32> {
    Some(match format {
        VIDEO_BGRA => dmabuf::ARGB8888,
        VIDEO_BGRX => XRGB8888,
        _ => return None,
    })
}

/// 共享内存那条路：门户已经拷进 memfd，这里按行读到紧密的 BGRA。
fn read_shm(spa: &SpaBuffer, format: u32, width: u32, height: u32) -> Option<Delivered> {
    let data = unsafe { &*spa.datas };
    if data.data.is_null() || data.chunk.is_null() {
        // 既不是 DMA-BUF 又没有可读的指针：空缓冲。
        return None;
    }
    let chunk = unsafe { &*data.chunk };
    let stride = if chunk.stride > 0 {
        chunk.stride as usize
    } else {
        width as usize * 4
    };
    let (mut x, mut y, mut w, mut h) = (0usize, 0usize, width as usize, height as usize);
    for index in 0..spa.n_metas as usize {
        let meta = unsafe { &*spa.metas.add(index) };
        if meta.kind == META_VIDEO_CROP
            && meta.size as usize >= size_of::<SpaMetaRegion>()
            && !meta.data.is_null()
        {
            let region = unsafe { &*meta.data.cast::<SpaMetaRegion>() };
            if region.width > 0 && region.height > 0 {
                x = region.x.max(0) as usize;
                y = region.y.max(0) as usize;
                w = (region.width as usize).min(width as usize - x.min(width as usize));
                h = (region.height as usize).min(height as usize - y.min(height as usize));
            }
        }
    }
    let base = (chunk.offset % data.maxsize.max(1)) as usize;
    let needed = base + (y + h).saturating_sub(1) * stride + (x + w) * 4;
    if w == 0 || h == 0 || needed > data.maxsize as usize {
        return None;
    }
    let source =
        unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), data.maxsize as usize) };
    // 这些像素只有编码器会读，而两条编码路径都只取色彩通道（软件路径交给 yuv
    // 的 BGRA 转换，VA-API 路径交给 bgra_to_yuv420），alpha 字节从来没人看。
    // 为它单独扫一遍画面纯属浪费——2880x1800 上实测 2.8ms/帧——所以缓冲里
    // 的 alpha 就按合成器给的原样留着。
    let swap = matches!(format, VIDEO_RGBX | VIDEO_RGBA);
    let mut pixels = vec![0u8; w * h * 4];
    if !swap {
        // 常见情形本来就是 BGRX，整行拷过去即可。
        for row in 0..h {
            let start = base + (y + row) * stride + x * 4;
            pixels[row * w * 4..(row + 1) * w * 4].copy_from_slice(&source[start..start + w * 4]);
        }
    } else {
        // 要交换红蓝就只能逐像素来，顺手把源里的 alpha 也带过去。
        for row in 0..h {
            let start = base + (y + row) * stride + x * 4;
            let source_row = &source[start..start + w * 4];
            let target = &mut pixels[row * w * 4..(row + 1) * w * 4];
            for (texel, out) in source_row.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
                out[0] = texel[2];
                out[1] = texel[1];
                out[2] = texel[0];
                out[3] = texel[3];
            }
        }
    }
    Some(Delivered::Shm {
        width: w as u32,
        height: h as u32,
        pixels,
    })
}

/// 一到多帧画面，和它背后那条 PipeWire 流。
///
/// `Stream` 自己不负责拆，拆的是 [`Session`]（随最后一个 [`BufferLease`] 一起走）：
/// 显存帧还在编码器手上时流必须活着，否则归还缓冲就会摸到已经释放的句柄。
pub(super) struct Stream {
    /// 强引用把 [`Session`] 钉住：显存帧还挂在编码器手上时（租约持有 `Session`
    /// 的另一个 `Arc`），流必须活着才谈得上归还缓冲。这个字段不直接读，就是为
    /// 了寿命。
    #[allow(dead_code)]
    session: Arc<Session>,
    shared: Arc<Shared>,
    delivered: u64,
}

unsafe impl Send for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        // 先把「上一帧」从共享状态里取出来丢掉：它可能正扣着合成器池子里的一块
        // 缓冲，而缓冲的租约持有另一个 `Arc<Session>`。留着就成了强引用环
        // Session→Callbacks→Shared→State.picture→Arc<Buffer>→BufferLease→Session，
        // `Arc<Session>` 计数永远到不了 0，`Session::drop`（loop 线程、上下文、
        // 连接、回调）全都泄漏。取出来 drop 掉，租约归还缓冲、放开它的
        // `Arc<Session>`；随后本结构体自带的那个 `Arc<Session>` 一 drop 计数就归零，
        // `Session::drop` 在这条线程（非 loop 线程）上正常执行。
        let buffered = lock(&self.shared.state).picture.take();
        drop(buffered);
    }
}

/// The stream's PipeWire handles, shared with any buffer leases still out.
struct Session {
    api: &'static Api,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    /// 事件钩子挂在 stream 上，必须活到 `stream_destroy` 之后。用 `Option` 只是
    /// 为了让 `Session::drop` 能把它们整批交给拆流的 `Handles`。
    hook: Option<Box<SpaHook>>,
    events: Option<Box<StreamEvents>>,
    callbacks: *mut Callbacks,
}

// The raw handles are only used under the thread loop's lock or after it stopped.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

/// 拆一条流要用到的全部句柄，清一色裸指针，好整批挪到别的线程上跑（见
/// `Session::drop` 里那段「已经在 loop 线程上」的处理）。
struct Handles {
    api: &'static Api,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    hook: *mut SpaHook,
    events: *mut StreamEvents,
    callbacks: *mut Callbacks,
}

// SAFETY: 这批句柄只在 `teardown` 里按序用一次，而 `teardown` 保证跑在非 loop
// 线程上（要么本来就跑在外部线程，要么被丢给一条新线程），不会和 loop 线程并发
// 碰同一批句柄。裸指针本身不 Send，这里替它们担保。
unsafe impl Send for Handles {}

impl Handles {
    /// # Safety
    /// 只能调用一次，且必须在**非** loop 线程上跑：它会先锁 loop 线程、再把它停掉。
    unsafe fn teardown(self) {
        let api = self.api;
        // SAFETY: 按建立的相反顺序拆；loop 先停下来，之后才释放它可能还会回调到
        // 的 `callbacks`。
        unsafe {
            (api.thread_loop_lock)(self.thread_loop);
            if !self.stream.is_null() {
                (api.stream_disconnect)(self.stream);
                (api.stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (api.core_disconnect)(self.core);
            }
            (api.thread_loop_unlock)(self.thread_loop);
            (api.thread_loop_stop)(self.thread_loop);
            (api.context_destroy)(self.context);
            (api.thread_loop_destroy)(self.thread_loop);
            if !self.hook.is_null() {
                drop(Box::from_raw(self.hook));
            }
            if !self.events.is_null() {
                drop(Box::from_raw(self.events));
            }
            if !self.callbacks.is_null() {
                drop(Box::from_raw(self.callbacks));
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // 能跑到这里，说明已经没有租约持有本会话了（有的话计数不会归零）。但
        // `Stream::drop` 之外还可能有别的帧挂在共享状态里，顺手清掉，别让它连着
        // `Shared` 一起被赖着。
        if !self.callbacks.is_null() {
            let shared = unsafe { &(*self.callbacks).shared };
            let buffered = lock(&shared.state).picture.take();
            drop(buffered);
        }
        // 记下「现在是不是在自己的 loop 线程上」。是的话就地拆会自锁——
        // `thread_loop_lock`/`thread_loop_stop` 正对着自己这条线程来（`on_process`
        // 丢掉最后一帧那种窗口就会走到这里）。所以把这批句柄丢给一条新线程去拆，
        // 本线程立刻返回、让回调跑完、放掉 loop 锁，新线程随后才动得了手。
        let on_loop_thread = if self.callbacks.is_null() {
            false
        } else {
            // SAFETY: 上面的兜底清理刚用过这个指针，它还活着。显式取引用，别让
            // 编译器对裸指针的解引用做隐式 autoref（edition 2024 的
            // `dangerous_implicit_autorefs` 是拒绝级）。
            let callbacks = unsafe { &*self.callbacks };
            let loop_thread = lock(&callbacks.shared.state).loop_thread;
            loop_thread == Some(std::thread::current().id())
        };
        let handles = Handles {
            api: self.api,
            thread_loop: self.thread_loop,
            context: self.context,
            core: self.core,
            stream: self.stream,
            hook: self.hook.take().map_or(std::ptr::null_mut(), Box::into_raw),
            events: self.events.take().map_or(std::ptr::null_mut(), Box::into_raw),
            callbacks: std::mem::replace(&mut self.callbacks, std::ptr::null_mut()),
        };
        if on_loop_thread {
            std::thread::spawn(move || unsafe { handles.teardown() });
        } else {
            unsafe { handles.teardown() };
        }
    }
}

impl Stream {
    /// Connect to `node` on the PipeWire remote behind `fd` (from the portal).
    /// `dmabuf` asks for linear DMA-BUF frames; the compositor falls back to
    /// shared memory on its own when it cannot serve one.
    pub fn connect(fd: OwnedFd, node: u32, dmabuf: bool) -> Result<Self> {
        use std::os::fd::IntoRawFd;
        let api = api()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            session: Mutex::new(Weak::new()),
        });
        let name = CString::new("openuuyc-capture")?;
        // SAFETY: the PipeWire calls follow the documented setup sequence; every
        // handle is checked and owned by the returned value, which tears them
        // down in reverse order.
        let session = unsafe {
            let thread_loop = (api.thread_loop_new)(name.as_ptr(), std::ptr::null());
            ensure!(!thread_loop.is_null(), "无法创建 PipeWire 线程");
            let context = (api.context_new)(
                (api.thread_loop_get_loop)(thread_loop),
                std::ptr::null_mut(),
                0,
            );
            if context.is_null() {
                (api.thread_loop_destroy)(thread_loop);
                bail!("无法创建 PipeWire 上下文");
            }
            let mut this = Session {
                api,
                thread_loop,
                context,
                core: std::ptr::null_mut(),
                stream: std::ptr::null_mut(),
                hook: Some(Box::new(std::mem::zeroed())),
                events: Some(Box::new(StreamEvents {
                    version: STREAM_EVENTS_VERSION,
                    destroy: None,
                    state_changed: Some(on_state_changed),
                    control_info: None,
                    io_changed: None,
                    param_changed: Some(on_param_changed),
                    add_buffer: None,
                    remove_buffer: None,
                    process: Some(on_process),
                    drained: None,
                    command: None,
                    trigger_done: None,
                })),
                callbacks: std::ptr::null_mut(),
            };
            ensure!(
                (api.thread_loop_start)(thread_loop) >= 0,
                "无法启动 PipeWire 线程"
            );
            (api.thread_loop_lock)(thread_loop);
            let result = (|| -> Result<()> {
                this.core =
                    (api.context_connect_fd)(context, fd.into_raw_fd(), std::ptr::null_mut(), 0);
                ensure!(!this.core.is_null(), "无法连接屏幕共享的 PipeWire 远端");
                let key = |s: &str| CString::new(s).unwrap();
                let (media_type, video, category, capture, role, screen) = (
                    key("media.type"),
                    key("Video"),
                    key("media.category"),
                    key("Capture"),
                    key("media.role"),
                    key("Screen"),
                );
                let props = (api.properties_new)(
                    media_type.as_ptr(),
                    video.as_ptr(),
                    category.as_ptr(),
                    capture.as_ptr(),
                    role.as_ptr(),
                    screen.as_ptr(),
                    std::ptr::null::<c_char>(),
                );
                this.stream = (api.stream_new)(this.core, name.as_ptr(), props);
                ensure!(!this.stream.is_null(), "无法创建 PipeWire 画面流");
                this.callbacks = Box::into_raw(Box::new(Callbacks {
                    api,
                    stream: this.stream,
                    shared: Arc::clone(&shared),
                }));
                (api.stream_add_listener)(
                    this.stream,
                    this.hook.as_deref_mut().expect("刚建好的钩子"),
                    this.events.as_deref().expect("刚建好的事件表"),
                    this.callbacks.cast(),
                );
                // 只有格式随连接一起给出去；缓冲类型等 `on_param_changed` 报出
                // 格式之后再决定，提前发就有可能在还没知道格式时就要了显存。
                let format = enum_format(dmabuf).aligned();
                let mut params = [format.as_ptr().cast::<c_void>()];
                let connected = (api.stream_connect)(
                    this.stream,
                    DIRECTION_INPUT,
                    node,
                    FLAG_AUTOCONNECT | FLAG_MAP_BUFFERS,
                    params.as_mut_ptr(),
                    1,
                );
                ensure!(connected >= 0, "连接 PipeWire 画面流失败：{connected}");
                Ok(())
            })();
            (api.thread_loop_unlock)(thread_loop);
            result?;
            Arc::new(this)
        };
        *shared.session.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&session);
        let stream = Self {
            session,
            shared,
            delivered: 0,
        };
        // 协商是异步的，而「要不要走零拷贝」是在 `hardware_images()` 里同步问的，
        // 所以在这里等第一份格式落地（有上限；谈不到就按共享内存算，只是不走
        // 零拷贝，采集本身照常）。
        if !stream.wait_first_format(Duration::from_millis(500)) {
            tracing::debug!("PipeWire 还没有报出格式，先按共享内存继续");
        }
        Ok(stream)
    }

    /// 等第一份协商出来的格式。返回是否等到了。
    fn wait_first_format(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        while state.format.is_none() && state.error.is_none() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state.format.is_some()
    }

    /// 现在这条流谈成的是不是显存。零拷贝那条路靠它决定要不要开。
    pub fn dmabuf_active(&self) -> bool {
        lock(&self.shared.state).buffers_requested == Some(true)
    }

    /// 这块屏「看到」的尺寸，也就是协商出来的画面尺寸。
    pub fn displayed_size(&self) -> (u32, u32) {
        lock(&self.shared.state)
            .format
            .map_or((0, 0), |format| (format.width, format.height))
    }

    /// The next frame newer than the last one returned, waiting up to
    /// `timeout`. `None` when nothing new arrived.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Delivered>> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(error) = &state.error {
                bail!("{error}");
            }
            if state.sequence != self.delivered
                && let Some(picture) = state.picture.take()
            {
                self.delivered = state.sequence;
                return Ok(Some(picture));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_parses_back() {
        // A fixed format written the way enum_format writes a choice's first
        // element must be read by parse_format.
        let pod = Pod::object(OBJECT_FORMAT, PARAM_FORMAT, |p| {
            p.key(FORMAT_MEDIA_TYPE);
            p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
            p.key(FORMAT_VIDEO_FORMAT);
            p.value(TYPE_ID, &[VIDEO_BGRX]);
            p.key(FORMAT_VIDEO_SIZE);
            p.value(TYPE_RECTANGLE, &[2560, 1440]);
        })
        .aligned();
        assert_eq!(
            unsafe { parse_format(pod.as_ptr().cast()) },
            Some(NegotiatedFormat {
                format: VIDEO_BGRX,
                width: 2560,
                height: 1440,
                modifier: None,
            })
        );
        let choices = enum_format(true).aligned();
        let size = unsafe { choices.as_ptr().cast::<u32>().read() } as usize;
        assert_eq!((size + 8) % 8, 0);
    }

    /// 协商出来的格式带不带修饰符，决定缓冲类型报哪一支；两边的值都要对，
    /// 尤其是位号（`1 << 3` 而不是枚举值 3）。
    #[test]
    fn buffers_offers_dmabuf_alone_in_one_flags_choice() {
        let pod = buffers(true).aligned();
        let words: Vec<u32> = (0..25)
            .map(|index| unsafe { pod.as_ptr().cast::<u32>().add(index).read() })
            .collect();
        // [size, TYPE_OBJECT, ParamBuffers, PARAM_BUFFERS,
        //  key(count), pad, choice size, TYPE_CHOICE, CHOICE_RANGE, 0,
        //    child size, TYPE_INT, default, min, max,
        //  key(dataType), pad, choice size, TYPE_CHOICE, CHOICE_FLAGS, 0,
        //    child size, TYPE_INT, mask]
        assert_eq!(words[1], TYPE_OBJECT);
        assert_eq!(words[2], OBJECT_PARAM_BUFFERS);
        assert_eq!(words[3], PARAM_BUFFERS);
        assert_eq!(words[4], BUFFERS_COUNT);
        assert_eq!(words[8], CHOICE_RANGE);
        assert_eq!(words[12], 8, "显存帧要的池子得比默认的四块深");
        assert_eq!(words[16], BUFFERS_DATA_TYPE);
        assert_eq!(words[20], CHOICE_FLAGS);
        // DMA-BUF alone: `MemPtr` alongside it would win the collapse inside
        // libpipewire and make allocation fail against a DMA-BUF format.
        assert_eq!(words[24], 1 << DATA_MEM_DMABUF);
        // The shared-memory offer still names a CPU-readable type.
        let shm = buffers(false).aligned();
        assert_eq!(
            unsafe { shm.as_ptr().cast::<u32>().add(24).read() },
            (1 << DATA_MEM_PTR) | (1 << DATA_MEM_FD)
        );
    }

    /// SPA 的 `spa_video_format` 到 DRM fourcc 的映射：只有 BGRA/BGRX 是我们
    /// 能导入的 AR24/XR24，别的都要拒。
    #[test]
    fn only_bgra_bgrx_map_to_an_importable_fourcc() {
        assert_eq!(drm_fourcc(VIDEO_BGRA), Some(dmabuf::ARGB8888));
        assert_eq!(drm_fourcc(VIDEO_BGRX), Some(XRGB8888));
        assert_eq!(drm_fourcc(VIDEO_RGBA), None);
        assert_eq!(drm_fourcc(VIDEO_RGBX), None);
    }
}
