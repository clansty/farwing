//! 本地剪贴板的「写次数」。
//!
//! 判断「本地剪贴板变了没有」不能只看内容：同一个文件复制第二遍，路径、文本、像素
//! 全都一样，可它仍然是一次新的复制——对端可能已经把上一次那份用掉了（官方 Windows
//! 客户端粘完就不再拿那个 offer），不重新广播它就一直粘不上。
//!
//! Windows 那边用的是 `GetClipboardSequenceNumber()`，每次写剪贴板都会加一。这里取
//! 它的等价物：合成器的 `zwlr_data_control_device_v1.selection` 事件——每次有人认领
//! 剪贴板都会收到一次，**内容相同也一样**（本机实测：连写两次同一段文本，两次都到）。
//!
//! 反过来也成立：这条通道上没有前进，Wayland 那一侧的内容就没变过。调用方据此跳过
//! 「内容没变也重读一遍」——剪贴板上挂着一张截图时，那一下要解整份 PNG。X11 是另一座
//! 孤岛（本机实测：`xclip` 写进去 `wl-paste` 看不见，反过来也一样），XWayland 里的
//! 程序复制不会在这里留下事件，所以那一侧只能照旧定时去读，不能跟着一起跳过。
//!
//! 接不上（合成器没这个协议、没有会话）就安静收场：计数保持 0，调用方退回按内容判断。
use anyhow::{Context as _, Result, bail};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
};

pub(super) struct Watch {
    generation: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
}
impl Watch {
    pub(super) fn start() -> Self {
        let generation = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(false));
        let counter = generation.clone();
        let running = alive.clone();
        let spawned = std::thread::Builder::new()
            .name("clipboard write watch".into())
            .spawn(move || {
                if let Err(error) = run(&counter, &running) {
                    // 这条要看得见：接不上时调用方会安静地退回按内容判断，
                    // 而「重复复制同一个文件没反应」正是那一路的老毛病。
                    tracing::info!(
                        error = format!("{error:#}"),
                        "剪贴板写事件通道不可用，退回按内容判断"
                    );
                }
                // 通道断了以后写次数就停在原地：调用方不能再拿「没前进」当「没变化」，
                // 否则 Wayland 那一侧真变了也会一直被跳过。
                running.store(false, Ordering::Release);
            });
        if let Err(error) = spawned {
            tracing::info!(%error, "剪贴板写事件线程起不来");
        }
        Self { generation, alive }
    }
    /// 0 表示这条通道没起来，调用方要退回按内容判断。
    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    /// 通道还活着。只有活着，「写次数没前进」才等于「Wayland 那一侧没变」。
    pub(super) fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

struct State {
    counter: Arc<AtomicU64>,
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    /// 最近一个 offer。要一直拿着，而且要等下一个来了才拆上一个：合成器可能在下一次
    /// `selection` 里再引用它，提前拆掉就是收一个已经没有的对象的 id。
    offer: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
    finished: bool,
}

fn run(counter: &Arc<AtomicU64>, alive: &Arc<AtomicBool>) -> Result<()> {
    let connection = Connection::connect_to_env().context("没有可用的 Wayland 会话")?;
    let mut queue = connection.new_event_queue::<State>();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());
    let mut state = State {
        counter: counter.clone(),
        manager: None,
        seat: None,
        offer: None,
        finished: false,
    };
    queue
        .roundtrip(&mut state)
        .context("读取 Wayland 全局对象失败")?;
    let (Some(manager), Some(seat)) = (state.manager.clone(), state.seat.clone()) else {
        bail!("合成器没有 zwlr_data_control_manager_v1 或 wl_seat");
    };
    let device = manager.get_data_device(&seat, &handle, ());
    // 连接和设备都建好了，写次数从这里起看得见：调用方可以开始按「没前进」跳过重读。
    alive.store(true, Ordering::Release);
    while !state.finished {
        if let Err(error) = queue.blocking_dispatch(&mut state) {
            // 连接断了（合成器重启之类）：到此为止，调用方退回按内容判断。
            return Err(error).context("剪贴板写事件通道已断开");
        }
    }
    device.destroy();
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "zwlr_data_control_manager_v1" => {
                state.manager = Some(registry.bind(name, version.min(2), handle, ()))
            }
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), handle, ())),
            _ => {}
        }
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                if let Some(previous) = state.offer.replace(id) {
                    previous.destroy();
                }
            }
            // 有人认领了剪贴板：内容一样也算一次新的复制，这正是要数的东西。
            zwlr_data_control_device_v1::Event::Selection { .. } => {
                state.counter.fetch_add(1, Ordering::AcqRel);
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {}
            zwlr_data_control_device_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    // `data_offer` 是「创建子对象」的事件，少了这条特化，合成器真送来事件时会
    // panic，而那个 panic 要跨过 C 回调、没法 unwinding，整个进程直接 abort。
    wayland_client::event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
