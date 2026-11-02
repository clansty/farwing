//! 把 GPU 纹理交付和 NVENC 放到独立线程，采集线程只负责等帧和共享纹理复制。
//! 只有“NVENC 在独立 D3D11 设备上”这条路径才启用；其它后端保持原来的
//! 串行路径，避免把未经验证的设备共享语义扩散到所有编码器。
use crate::features::host::format::{Color, Format, Rate};
use crate::features::host::{capture, encoder};
use anyhow::Result;
use std::sync::{Arc, atomic::AtomicBool};
#[cfg(windows)]
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Stats {
    pub encoded: u64,
    pub encode_count: u64,
    pub encode_total: Duration,
    pub encode_max: Duration,
    pub send_drops: u64,
    pub busy: u64,
}

#[cfg(windows)]
mod real {
    use super::*;
    use crate::features::host::format::Backend;
    use std::collections::BTreeMap;
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::thread::JoinHandle;

    struct Shared {
        encoded: AtomicU64,
        encode_count: AtomicU64,
        encode_ns: AtomicU64,
        encode_max_ns: AtomicU64,
        send_drops: AtomicU64,
        busy: AtomicU64,
    }
    impl Shared {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                encoded: AtomicU64::new(0),
                encode_count: AtomicU64::new(0),
                encode_ns: AtomicU64::new(0),
                encode_max_ns: AtomicU64::new(0),
                send_drops: AtomicU64::new(0),
                busy: AtomicU64::new(0),
            })
        }
    }

    struct Ready {
        maximum_size: (u32, u32),
        implementation: i32,
        path: &'static str,
    }

    struct Job {
        capture: capture::RawCapture,
        rate: Rate,
    }

    pub(crate) struct Pipeline {
        ring: crate::platform::transfer::pipeline::Sender,
        tx: Option<SyncSender<Job>>,
        thread: Option<JoinHandle<()>>,
        stats: Arc<Shared>,
        error: Arc<Mutex<Option<String>>>,
        maximum_size: (u32, u32),
        implementation: i32,
        path: &'static str,
    }

    impl Pipeline {
        pub(crate) fn new(
            source: &capture::Device,
            destination: capture::Device,
            desc: (u32, u32, u32),
            size: (u32, u32),
            format: Format,
            rate: Rate,
            color: Color,
            sender: tokio::sync::mpsc::Sender<encoder::Encoded>,
            keyframe: Arc<AtomicBool>,
            cancel: Arc<AtomicBool>,
            started: Instant,
        ) -> Result<Self> {
            let desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC {
                Width: desc.0,
                Height: desc.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(desc.2 as i32),
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let ring =
                crate::platform::transfer::pipeline::Sender::new(source, &destination, desc)?;
            let (tx, rx) = sync_channel(2);
            let (ready_tx, ready_rx) = sync_channel(1);
            let stats = Shared::new();
            let error = Arc::new(Mutex::new(None));
            let thread_stats = stats.clone();
            let thread_error = error.clone();
            let thread = std::thread::Builder::new()
                .name("host-encode".into())
                .spawn(move || {
                    let result = run(
                        destination,
                        rx,
                        ready_tx,
                        sender,
                        keyframe,
                        cancel,
                        started,
                        thread_stats,
                        size,
                        format,
                        rate,
                        color,
                    );
                    if let Err(error) = result {
                        tracing::warn!(%error, "host encode pipeline thread stopped");
                        let mut slot = thread_error
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        if slot.is_none() {
                            *slot = Some(format!("{error:#}"));
                        }
                    }
                })?;
            match ready_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(ready)) => Ok(Self {
                    ring,
                    tx: Some(tx),
                    thread: Some(thread),
                    stats,
                    error,
                    maximum_size: ready.maximum_size,
                    implementation: ready.implementation,
                    path: ready.path,
                }),
                Ok(Err(error)) => {
                    let _ = thread.join();
                    anyhow::bail!(error);
                }
                Err(_) => {
                    let _ = thread.join();
                    anyhow::bail!("编码线程未能启动");
                }
            }
        }

        pub(crate) fn next_raw(
            &mut self,
            desktop: &mut capture::Desktop,
            timeout: u32,
            quality: i32,
            hdr: bool,
            maximum: (u32, u32),
        ) -> Result<Option<capture::RawCapture>> {
            desktop.next_raw(timeout, quality, hdr, maximum, &mut self.ring)
        }

        pub(crate) fn submit_raw(
            &mut self,
            capture: capture::RawCapture,
            rate: Rate,
        ) -> Result<bool> {
            let job = Job { capture, rate };
            match self.tx.as_ref().expect("tx until drop").try_send(job) {
                Ok(()) => Ok(true),
                Err(std::sync::mpsc::TrySendError::Full(job)) => {
                    drop(job);
                    self.stats.busy.fetch_add(1, Ordering::Relaxed);
                    Ok(false)
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    anyhow::bail!("编码线程已退出")
                }
            }
        }

        pub(crate) fn stats(&mut self) -> Stats {
            let ns = self.stats.encode_ns.swap(0, Ordering::AcqRel);
            let count = self.stats.encode_count.swap(0, Ordering::AcqRel);
            Stats {
                encoded: self.stats.encoded.swap(0, Ordering::AcqRel),
                encode_count: count,
                encode_total: Duration::from_nanos(ns),
                encode_max: Duration::from_nanos(
                    self.stats.encode_max_ns.swap(0, Ordering::AcqRel),
                ),
                send_drops: self.stats.send_drops.swap(0, Ordering::AcqRel),
                busy: self.stats.busy.swap(0, Ordering::AcqRel),
            }
        }

        pub(crate) fn error(&self) -> Option<String> {
            self.error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        }

        pub(crate) fn maximum_size(&self) -> (u32, u32) {
            self.maximum_size
        }

        pub(crate) fn implementation(&self) -> i32 {
            self.implementation
        }

        pub(crate) fn path(&self) -> &'static str {
            self.path
        }
    }

    impl Drop for Pipeline {
        fn drop(&mut self) {
            self.tx.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        device: capture::Device,
        rx: Receiver<Job>,
        ready: SyncSender<Result<Ready, String>>,
        sender: tokio::sync::mpsc::Sender<encoder::Encoded>,
        keyframe: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
        started: Instant,
        stats: Arc<Shared>,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        color: Color,
    ) -> Result<()> {
        let mut encoder = match encoder::Encoder::hardware_format(
            Backend::Nvidia,
            &device,
            size,
            format,
            rate,
            color,
        ) {
            Ok(encoder) => encoder,
            Err(error) => {
                let _ = ready.send(Err(format!("{error:#}")));
                return Err(error);
            }
        };
        let _ = ready.send(Ok(Ready {
            maximum_size: encoder.maximum_size(),
            implementation: encoder.implementation(),
            path: encoder.path(),
        }));
        tracing::info!(
            size = ?size,
            format = ?format,
            path = encoder.path(),
            "host encode pipeline ready"
        );
        let context = unsafe { device.GetImmediateContext()? };
        let mut converter = crate::platform::windows::preprocess::Converter::new(&device)?;
        let mut metadata = BTreeMap::new();
        while let Ok(mut job) = rx.recv() {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            job.capture.lease.wait()?;
            let reconfigured = encoder.configure_rate(job.rate)?;
            if reconfigured {
                keyframe.store(true, Ordering::Release);
            }
            let force = keyframe.swap(false, Ordering::AcqRel);
            let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
            unsafe {
                job.capture.lease.texture().GetDesc(&mut desc);
            }
            let converted = converter.convert(
                &device,
                &context,
                job.capture.lease.texture(),
                desc,
                job.capture.rotation,
                job.capture.white,
                job.capture.source_hdr,
                job.capture.request,
                None,
            )?;
            let timestamp_100ns =
                (job.capture.captured.duration_since(started).as_nanos() / 100) as i64;
            let encode_started = Instant::now();
            metadata.insert(
                timestamp_100ns,
                (
                    job.capture.captured,
                    encode_started,
                    job.capture.is_new,
                    job.capture.hdr_metadata,
                ),
            );
            while metadata.len() > 2048 {
                metadata.pop_first();
            }
            let encoded = encoder.encode_cancellable(&converted, timestamp_100ns, force, &cancel);
            let elapsed = encode_started.elapsed();
            stats.encode_count.fetch_add(1, Ordering::Relaxed);
            stats
                .encode_ns
                .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
            stats
                .encode_max_ns
                .fetch_max(elapsed.as_nanos() as u64, Ordering::Relaxed);
            let encoded = match encoded {
                Ok(encoded) => encoded,
                Err(error) => {
                    keyframe.store(true, Ordering::Release);
                    return Err(error);
                }
            };
            stats
                .encoded
                .fetch_add(encoded.len() as u64, Ordering::Relaxed);
            let encode_finished = Instant::now();
            for mut frame in encoded {
                if let Some((captured, input_started, is_new, hdr)) =
                    metadata.remove(&frame.timestamp_100ns)
                {
                    frame.is_new = is_new;
                    frame.color.hdr_metadata = if frame.color.transfer == 16 {
                        hdr
                    } else {
                        None
                    };
                    frame.timing = Some(encoder::FrameTiming {
                        captured,
                        encode_started: input_started,
                        encode_finished,
                    });
                }
                while metadata
                    .first_key_value()
                    .is_some_and(|(&timestamp, _)| timestamp < frame.timestamp_100ns)
                {
                    metadata.pop_first();
                }
                match sender.try_send(frame) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(frame)) => {
                        stats.send_drops.fetch_add(1, Ordering::Relaxed);
                        keyframe.store(true, Ordering::Release);
                        drop(frame);
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        anyhow::bail!("画面发送队列已关闭");
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) use real::Pipeline;

#[cfg(not(windows))]
pub(crate) struct Pipeline;

#[cfg(not(windows))]
impl Pipeline {
    pub(crate) fn new(
        _source: &capture::Device,
        _destination: capture::Device,
        _desc: (u32, u32, u32),
        _size: (u32, u32),
        _format: Format,
        _rate: Rate,
        _color: Color,
        _sender: tokio::sync::mpsc::Sender<encoder::Encoded>,
        _keyframe: Arc<AtomicBool>,
        _cancel: Arc<AtomicBool>,
        _started: Instant,
    ) -> Result<Self> {
        anyhow::bail!("Windows 编码流水线不可用")
    }

    pub(crate) fn next_raw(
        &mut self,
        _desktop: &mut capture::Desktop,
        _timeout: u32,
        _quality: i32,
        _hdr: bool,
        _maximum: (u32, u32),
    ) -> Result<Option<capture::RawCapture>> {
        Ok(None)
    }

    pub(crate) fn submit_raw(
        &mut self,
        _capture: capture::RawCapture,
        _rate: Rate,
    ) -> Result<bool> {
        Ok(false)
    }

    pub(crate) fn stats(&mut self) -> Stats {
        Stats::default()
    }

    pub(crate) fn error(&self) -> Option<String> {
        None
    }

    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        (0, 0)
    }

    pub(crate) fn implementation(&self) -> i32 {
        0
    }

    pub(crate) fn path(&self) -> &'static str {
        "软件编码"
    }
}
