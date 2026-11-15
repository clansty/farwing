use crate::account::api::RoomSession;
use crate::account::client::AuthenticatedClient;
use crate::application::viewer::{
    ConnectionProgress, NativeViewerSession, ViewerDisplayHandle, ViewerLaunchConfig,
};
use crate::features::stream_control::StreamControlHandle;
use crate::media::{
    ConnectionMediaOptions, ConnectionMediaProfile, LocalDisplayInfo, VideoCodec,
    detect_local_display,
};
use crate::transport::rtc::{
    ForwardedTrack, IceServer, MediaKind, NativePeer, RtpForwardConfig, RtpForwarder,
};
use crate::transport::signal::{NegotiationEvent, SignalFailure, SignalRole, SignalSession};
use anyhow::{Context, Result, anyhow, bail};
use connection::ResolvedConnection;
pub use connection::{connect_saved_alias, connect_saved_alias_with_progress};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use viewing::run_viewer_window;
pub use viewing::{run_native_viewer_session, run_saved_viewer_window};

pub type ConnectionProgressReporter = Arc<dyn Fn(ConnectionProgress) + Send + Sync>;
/// 让房间的 socket 所有者再开一路会话。房间按设备分配、一条信令 socket 只能
/// 属于一个房间，所以第二路必须交给已经有房间的会话去开，而不是自己 join——
/// 再 join 一次会让服务端释放前一个房间，先连的那条随之断开。
async fn attach_room(
    requests: &tokio::sync::mpsc::UnboundedSender<crate::transport::signal::RoomRequest>,
    controller_device_id: &str,
    profile: ConnectionMediaProfile,
    transport: crate::media::TransportChoice,
    purpose: crate::session::negotiation::ControlPurpose,
    cancel: &CancellationToken,
) -> Result<crate::transport::signal::Attached> {
    let (reply, receiver) = oneshot::channel();
    requests
        .send(crate::transport::signal::RoomRequest::Attach(
            crate::transport::signal::SideChannelRequest {
                reply,
                controller_device_id: controller_device_id.to_owned(),
                profile,
                transport,
                purpose,
            },
        ))
        .map_err(|_| anyhow!("设备房间已结束，无法再开会话"))?;
    cancellable(cancel, async {
        receiver.await.context("房间会话请求中断")?
    })
    .await
}

/// 本机已经连着的设备上再开一路会话（终端）。
pub(crate) async fn attach_terminal(
    controller: &str,
    target: &str,
    controller_device_id: &str,
    profile: ConnectionMediaProfile,
    transport: crate::media::TransportChoice,
    cancel: &CancellationToken,
) -> Option<Result<(tokio::sync::mpsc::UnboundedSender<crate::transport::signal::RoomRequest>, crate::transport::signal::Attached)>> {
    let room = shared::room(&shared::key(controller, target))?;
    let attached = attach_room(
        &room.requests,
        controller_device_id,
        profile,
        transport,
        crate::session::negotiation::ControlPurpose::Terminal,
        cancel,
    )
    .await;
    Some(attached.map(|attached| (room.requests.clone(), attached)))
}
pub(crate) fn has_gui_connection(controller: &str, target: &str) -> bool {
    shared::get(&shared::key(controller, target)).is_some()
}
pub(crate) struct LocalConnectionActivity {
    pub viewing: bool,
    pub controlling: bool,
}
pub(crate) fn gui_connection_activity(
    controller: &str,
    target: &str,
) -> Option<LocalConnectionActivity> {
    shared::get(&shared::key(controller, target)).map(|session| session.activity())
}
mod assist;
mod shared;
pub(crate) mod takeover;
pub(crate) mod windows;

fn report_progress(
    reporter: Option<&ConnectionProgressReporter>,
    step: u8,
    title: impl Into<String>,
    detail: impl Into<String>,
) {
    if let Some(reporter) = reporter {
        reporter(ConnectionProgress::working(step, title, detail));
    }
}

pub struct ControllerConnection {
    peer: Arc<NativePeer>,
    forwarder: shared::ForwarderLease,
    profile: ConnectionMediaProfile,
    preference_writer: Option<crate::features::viewing_settings::PreferenceWriter>,
    local_preference_writer: Option<crate::features::viewing_settings::PreferenceWriter>,
    /// 本连接是哪个房间里的哪一路会话；关闭时只摘掉自己这一路。
    room: Option<Arc<shared::Room>>,
    ice_id: Option<String>,
}

impl Drop for ControllerConnection {
    fn drop(&mut self) {
        tracing::debug!(ice_id = ?self.ice_id, "controller connection dropped");
        if let (Some(room), Some(ice_id)) = (&self.room, &self.ice_id) {
            let _ = room.requests.send(
                crate::transport::signal::RoomRequest::Detach {
                    ice_id: ice_id.clone(),
                },
            );
        }
    }
}

pub struct ConnectionSummary {
    pub alias: String,
    pub stream_fps: u32,
    pub codec: &'static str,
    pub display_detection_warning: Option<String>,
}

pub struct PlaybackSummary {
    pub track_id: String,
    pub codec: &'static str,
    pub payload_type: u8,
    pub requested_keyframe: bool,
    pub player: &'static str,
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow!("connection cancelled")),
        result = future => result,
    }
}

fn retry_session_failure(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<SignalFailure>(), Some(failure) if !matches!(failure, SignalFailure::Kicked))
}

fn room_released(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<SignalFailure>(),
        Some(SignalFailure::Kicked)
    )
}

async fn await_media_startup<T>(
    ended: impl std::future::Future<Output = Result<()>>,
    startup: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        result = ended => Err(result.err().unwrap_or_else(|| anyhow!("设备连接已结束"))),
        result = startup => result,
    }
}

impl ControllerConnection {
    async fn close_after_startup_error(self, error: anyhow::Error) -> anyhow::Error {
        // Peer teardown can close RTP/decoder input before the signal task has
        // finished cleanup. Preserve the explicit leave instead of that symptom.
        match self.close().await {
            Err(ended) if room_released(&ended) => ended,
            _ => error,
        }
    }
    pub(crate) async fn connect_mapping(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::session::negotiation::ControlPurpose::PortMapping,
            false,
        )
        .await
    }
    pub(crate) async fn connect_files(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::session::negotiation::ControlPurpose::FileTransfer,
            false,
        )
        .await
    }
    /// 终端专用连接。被控端只在 `kind = 8` 的会话上绑定 TERM 帧，所以这条
    /// 连接不能复用观看会话；它也不进 shared 池，关掉时不会动别人的观看。
    pub(crate) async fn connect_terminal(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::session::negotiation::ControlPurpose::Terminal,
            true,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn connect_business(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
        purpose: crate::session::negotiation::ControlPurpose,
        dedicated: bool,
    ) -> Result<Self> {
        let key = shared::key(&client.device_id(), &device.device_id);
        let _gate = shared::acquire_connection(&key, cancel).await?;
        let display = detect_local_display().unwrap_or(LocalDisplayInfo::FALLBACK);
        let mut profile = options.resolve(display)?;
        if let Ok(store) = client.viewing_settings_store(&device.device_id)
            && let Ok(Some(saved)) = store.load().await
            && let Some(settings) = saved.settings
        {
            profile.stream_fps = settings.frame_rate.value(display);
            profile.decoder_fps_cap = display.refresh_hz.max(profile.stream_fps);
        }
        if !dedicated
            && let Some(session) = shared::get(&key)
        {
            return Self::from_shared(session, profile, false);
        }
        // 设备已经被本机自己占着（比如桌面看着、再开终端）时不算「别人在用」，
        // 不要接管：接管会把本机先前那条会话踢掉。多出来的那条是自己的第二次
        // join，服务端会另给一个房间。
        let self_occupied = device.occupied_by(&client.device_id());
        if device.participant_count() > 0 && takeover.is_none() && !self_occupied {
            return Err(takeover::Required(device.clone()).into());
        }
        let force_join = if let Some(approval) = takeover {
            cancellable(cancel, approval.verify(client, &device.device_id)).await?
        } else {
            false
        };
        // 本机已经在这台设备上有房间（终端先连）时不再 join：第二次 join 会让
        // 服务端释放那个房间，先连的那条随之断开。
        let room = if shared::room(&key).is_some() {
            None
        } else {
            Some(
                cancellable(cancel, client.join_device(&device.device_id, force_join))
                    .await
                    .map_err(|error| {
                        if force_join {
                            error.context(
                                "接管请求未确认成功，未自动重试；请检查设备状态后重新确认",
                            )
                        } else {
                            error
                        }
                    })?,
            )
        };
        let connection = Self::establish(
            room,
            &key,
            &client.device_id(),
            profile,
            policy,
            crate::session::negotiation::ControlConnectType::Normal,
            options.transport,
            None,
            crate::media::audio::AudioSettings {
                volume: 0,
                muted: true,
            },
            None,
            cancel,
            purpose,
        )
        .await?;
        // 池子里只放观看会话：文件、端口映射这些也是「本机连着」，但它们
        // 复用观看的 peer 就够了，登记进来会让界面以为开着观看窗口。
        if matches!(
            purpose,
            crate::session::negotiation::ControlPurpose::Viewing
                | crate::session::negotiation::ControlPurpose::Audio
        ) {
            shared::register(key, &connection.forwarder.session, client.ended());
        }
        Ok(connection)
    }
    fn from_shared(
        session: Arc<shared::Session>,
        profile: ConnectionMediaProfile,
        viewing: bool,
    ) -> Result<Self> {
        Ok(Self {
            peer: Arc::clone(&session.peer),
            forwarder: session.lease(viewing)?,
            profile,
            preference_writer: None,
            local_preference_writer: None,
            room: None,
            ice_id: None,
        })
    }
    pub(crate) async fn activate_viewing(&self) -> Result<()> {
        if self.profile.audio_only {
            return self.start_listening().await;
        }
        self.wait_port_mapping_ready().await?;
        let handle = self.stream_control_handle();
        handle.set_viewing_enabled(true);
        let snapshot = handle.snapshot();
        let screen = snapshot
            .screens
            .first()
            .context("被控端尚未提供显示器列表")?;
        handle.set_screen_capture(screen.id, true).await?;
        handle.set_viewing_enabled(true);
        Ok(())
    }
    async fn start_listening(&self) -> Result<()> {
        let handle = self.stream_control_handle();
        handle.wait_media_control().await?;
        self.peer.pause_video();
        handle.set_video_enabled(false).await?;
        self.forwarder.start_audio();
        handle.audio().start()?;
        self.performance_monitor().set_decoder("仅音频");
        Ok(())
    }
    /// 这条连接的 peer 是否还连着（用于自测：另一路会话关掉后本路是否还在）。
    pub(crate) fn alive(&self) -> bool {
        self.peer.connection_state()
            == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected
    }
    pub(crate) fn port_mapping_transport(&self) -> Arc<crate::features::port_mapping::Transport> {
        self.peer.port_mapping()
    }
    pub(crate) fn terminal_transport(
        &self,
    ) -> Arc<crate::features::terminal::client::Transport> {
        self.peer.terminal_transport()
    }
    pub(crate) async fn wait_port_mapping_ready(&self) -> Result<()> {
        let control = self.stream_control_handle();
        let changed = control.protocol_notifications();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let wake = changed.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if control.handshake_status().connected {
                    break;
                }
                wake.await;
            }
        })
        .await
        .context("端口转发协议握手未完成")?;
        self.port_mapping_transport().wait_ready().await
    }
    pub fn stream_control_handle(&self) -> StreamControlHandle {
        self.peer.stream_control_handle()
    }

    pub fn performance_monitor(&self) -> crate::diagnostics::performance::PerformanceMonitor {
        self.peer.performance_monitor()
    }

    async fn select_video_track(&mut self) -> Result<(ForwardedTrack, VideoCodec)> {
        let video = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let snapshot = self.stream_control_handle().snapshot();
                let current = snapshot.remote_display.as_ref().map(|d| d.screen_id);
                let preferred = snapshot.screens.iter()
                    .filter(|s| s.video_track_index >= 0)
                    .min_by_key(|s| (Some(s.id) != current, !s.primary))
                    .map(|s| s.video_track_index);
                if let Some(index) = preferred
                    && let Some(track) = self.forwarder.select_registered_video(index)
                {
                    return Ok::<_, anyhow::Error>(track);
                }
                let track = if preferred.is_none() && let Some(track) = self.forwarder.selected_metadata() {
                    track
                } else {
                    tokio::select! {
                        track = self.forwarder.next_track() => track.ok_or_else(|| anyhow!("remote RTP track channel closed"))?,
                        _ = tokio::time::sleep(Duration::from_millis(20)) => continue,
                    }
                };
                match track.kind {
                    MediaKind::Audio => {}
                    MediaKind::Video if preferred.is_none() || track.id == format!("video_{}", preferred.unwrap()) => return Ok::<_, anyhow::Error>(track),
                    MediaKind::Video => {}
                }
            }
        })
        .await
        .context("remote device sent no video RTP within 12 seconds")??;

        let codec = match video.codec.to_ascii_lowercase() {
            value if value.contains("h265") || value.contains("hevc") => VideoCodec::H265,
            value if value.contains("h264") => VideoCodec::H264,
            value if value.contains("av1") => VideoCodec::Av1,
            _ => bail!(
                "remote device selected unsupported video codec {}",
                video.codec
            ),
        };
        let video_track_index = video
            .id
            .strip_prefix("video_")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(0);
        self.peer.set_stream_video_stream(codec, video_track_index);
        Ok((video, codec))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn establish(
        room: Option<RoomSession>,
        room_key: &str,
        controller_device_id: &str,
        profile: ConnectionMediaProfile,
        features: crate::account::feature_ability::FeaturePolicy,
        connect_type: crate::session::negotiation::ControlConnectType,
        transport: crate::media::TransportChoice,
        preferences: Option<crate::features::stream_control::StreamControlPreferences>,
        audio_settings: crate::media::audio::AudioSettings,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        purpose: crate::session::negotiation::ControlPurpose,
    ) -> Result<Self> {
        tracing::info!(?profile, ?transport, "establishing controller connection");
        // 房间按设备分配，一个房间只有一条信令 socket：本机已经在这台设备上
        // 有房间（观看或终端）就挂上去，否则自己开一个。
        let (requests, owner, room_handle) = match shared::room(room_key) {
            Some(existing) => (existing.requests.clone(), None, existing),
            None => {
                report_progress(
                    reporter,
                    4,
                    "连接信令服务",
                    "正在完成 TLS、WebSocket、Engine.IO 与 Socket.IO 房间握手",
                );
                let room = room.context("缺少房间凭据")?;
                let signal = SignalSession::connect_cancellable(
                    room,
                    SignalRole::Controller,
                    cancel,
                )
                .await
                .context("connect controller signaling")?;
                report_progress(
                    reporter,
                    4,
                    "信令通道已连接",
                    "Socket.IO 房间验证完成，保活与断线重连密钥已就绪",
                );
                let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                let (shutdown_sender, shutdown_receiver) = oneshot::channel();
                let task = tokio::spawn(signal.keep_alive_room(shutdown_receiver, receiver));
                let room = shared::publish_room(room_key, sender.clone());
                (sender, Some((shutdown_sender, task)), room)
            }
        };
        report_progress(
            reporter,
            5,
            "控制握手",
            format!(
                "正在探测并上报解码能力；帧率上限 {} FPS，{}",
                profile.stream_fps,
                profile.codec.label()
            ),
        );
        let attached = attach_room(&requests, controller_device_id, profile, transport, purpose, cancel)
            .await
            .context("complete controller handshake")?;
        let peer = Arc::clone(&attached.peer);
        let transport = if attached.force_relay {
            crate::media::TransportChoice::Relay
        } else {
            transport
        };
        peer.stream_control_handle().set_feature_policy(features);
        peer.stream_control_handle()
            .set_viewing_enabled(purpose == crate::session::negotiation::ControlPurpose::Viewing);
        peer.stream_control_handle()
            .set_display_connection_type(connect_type);
        if let Some(preferences) = preferences
            && let Err(error) = peer
                .stream_control_handle()
                .restore_preferences(preferences)
        {
            let _ = peer.close().await;
            return Err(error);
        }
        // Restore before media can arrive or the playback window is exposed.
        peer.stream_control_handle()
            .audio()
            .set_settings(audio_settings);
        peer.configure_network_control(transport, attached.force_relay);
        report_progress(
            reporter,
            6,
            "安全传输栈已就绪",
            "本地媒体能力与控制数据通道已经创建，等待远端协商",
        );
        let forward_config = RtpForwardConfig {
            video_track_id: None,
        };
        let forwarder = match peer.install_rtp_forwarder(forward_config).await {
            Ok(forwarder) => forwarder,
            Err(error) => {
                let _ = peer.close().await;
                return Err(error).context("install decrypted RTP forwarder");
            }
        };
        let route = peer
            .selected_route_details()
            .await
            .unwrap_or_else(|| "安全媒体通道已连接，候选对详情尚未发布".to_owned());
        report_progress(reporter, 8, "连接线路已选定", route);
        peer.stream_control_handle()
            .network_control()
            .connected(true);
        tracing::info!("controller WebRTC negotiation complete");
        // 房间的主人才持有 socket 的关闭权；挂上去的会话只摘掉自己那一路。
        let (signal_shutdown, signal_task) = match owner {
            Some((shutdown, task)) => (Some(shutdown), Some(task)),
            None => (None, None),
        };
        let session = shared::Session::new(
            peer,
            forwarder,
            signal_shutdown,
            signal_task,
            requests.clone(),
        );
        let mut connection = Self::from_shared(
            session,
            profile,
            matches!(
                purpose,
                crate::session::negotiation::ControlPurpose::Viewing
                    | crate::session::negotiation::ControlPurpose::Audio
            ),
        )?;
        connection.room = Some(room_handle);
        connection.ice_id = Some(attached.ice_id);
        Ok(connection)
    }

    pub async fn start_native_viewer(
        &mut self,
        alias: &str,
    ) -> Result<(NativeViewerSession, PlaybackSummary)> {
        let (mut viewer, summary) = self
            .start_native_viewer_with_progress(alias, None, ViewerDisplayHandle::default())
            .await?;
        // The progress-window entry awaits startup separately. This direct
        // API must consume it before run() selects the native presenter.
        tokio::time::timeout(Duration::from_secs(30), viewer.startup())
            .await
            .context("first-frame decoder startup timeout")??;
        Ok((viewer, summary))
    }

    pub(crate) async fn start_native_viewer_with_progress(
        &mut self,
        alias: &str,
        reporter: Option<&ConnectionProgressReporter>,
        display: ViewerDisplayHandle,
    ) -> Result<(NativeViewerSession, PlaybackSummary)> {
        let hardware_decode = self.profile.decoder.mode.hardware();
        report_progress(
            reporter,
            9,
            "等待视频轨道",
            "安全媒体通道已建立，正在等待远端发布桌面视频轨道",
        );
        let (video, codec) = self.select_video_track().await?;
        report_progress(
            reporter,
            10,
            "视频轨道已协商",
            format!(
                "{} · RTP PT {} · SSRC {} · 轨道 {}",
                match codec {
                    VideoCodec::H264 => "H.264/AVC",
                    VideoCodec::H265 => "H.265/HEVC",
                    VideoCodec::Av1 => "AV1",
                },
                video.payload_type,
                video.ssrc,
                video.id
            ),
        );
        let profile = self.profile;
        let track_index = video
            .id
            .strip_prefix("video_")
            .and_then(|id| id.parse().ok())
            .unwrap_or(0);
        let performance = self
            .peer
            .performance_monitor()
            .for_video_track(track_index as u64);
        performance.set_video_codec(match codec {
            VideoCodec::H264 => format!("H.264/AVC · RTP PT {}", video.payload_type),
            VideoCodec::H265 => format!("H.265/HEVC · RTP PT {}", video.payload_type),
            VideoCodec::Av1 => format!("AV1 · RTP PT {}", video.payload_type),
        });
        let stream_control = self.peer.stream_control_handle();
        let receiver_feedback = self.forwarder.video_receiver_feedback();
        let title = format!("{}{alias}", crate::VIEWER_TITLE_PREFIX);
        report_progress(
            reporter,
            11,
            "初始化视频解码器",
            format!(
                "正在打开 {} {} 解码路径，实际画面尺寸以收到的码流为准",
                if hardware_decode {
                    "平台硬件优先"
                } else {
                    "软件"
                },
                match codec {
                    VideoCodec::H264 => "H.264",
                    VideoCodec::H265 => "H.265",
                    VideoCodec::Av1 => "AV1",
                },
            ),
        );
        let mut viewer = NativeViewerSession::launch(ViewerLaunchConfig {
            codec,
            decoder: profile.decoder,
            title,
            initial_width: profile.local_display.width,
            initial_height: profile.local_display.height,
            frame_rate: profile.stream_fps,
            receiver_feedback,
            performance: performance.clone(),
            stream_control,
            display,
        })
        .await?;
        viewer.attach_screen_playback(&self.peer, profile, alias, track_index);

        report_progress(
            reporter,
            11,
            "视频解码器已就绪",
            format!(
                "{} · {}",
                performance.snapshot().decoder,
                if hardware_decode {
                    "允许平台硬解，失败时由解码层报告实际后端"
                } else {
                    "已按用户设置禁用硬解优先"
                }
            ),
        );
        self.forwarder.add_video_sink(viewer.video_sink()).await;
        viewer.ensure_running()?;
        let keyframe_generation = self.forwarder.video_keyframe_generation();
        self.forwarder.start();
        if let Err(error) = self.peer.stream_control_handle().audio().start() {
            tracing::warn!(%error, "native audio did not start; video remains available");
        }
        report_progress(
            reporter,
            12,
            "同步首个完整画面",
            "已启动 RTP 接收，正在发送 PLI 并等待参数集、完整关键帧与首帧解码",
        );
        self.peer.request_keyframe(video.ssrc).await?;
        let keyframe_ready = self
            .forwarder
            .wait_for_video_keyframe_after(keyframe_generation, Duration::from_secs(2))
            .await;
        viewer.ensure_running()?;
        if !keyframe_ready {
            tracing::warn!("PLI 后 2 秒内未识别到完整参数集和关键帧，已继续等待后续关键帧");
            report_progress(
                reporter,
                12,
                "继续等待关键帧",
                "远端尚未在 2 秒内返回完整关键帧；接收链保持运行并继续请求恢复",
            );
        } else {
            report_progress(
                reporter,
                12,
                "首个完整画面已到达",
                "参数集与关键帧已通过接收门禁，播放器可以开始显示",
            );
        }
        let summary = PlaybackSummary {
            track_id: video.id,
            codec: match codec {
                VideoCodec::H264 => "H.264",
                VideoCodec::H265 => "H.265",
                VideoCodec::Av1 => "AV1",
            },
            payload_type: video.payload_type,
            requested_keyframe: true,
            player: "原生窗口",
        };

        Ok((viewer, summary))
    }

    pub async fn keep_alive(mut self, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
        let result = tokio::select! {
            result = self.forwarder.session.ended() => result,
            _ = &mut shutdown => Ok(()),
        };
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        if let Some(writer) = &mut self.local_preference_writer {
            writer.finish().await;
        }
        let closed = self.close().await;
        result.and(closed)
    }

    pub async fn close(mut self) -> Result<()> {
        if self.forwarder.viewing {
            let handle = self.stream_control_handle();
            handle.mouse().disable();
            handle.audio().suspend();
            // Local capture and clipboard permissions end before any network
            // wait. A shared file/port session must not keep a closing viewer's
            // microphone alive while screen-stop messages are backpressured.
            handle.set_viewing_enabled(false);
            if Arc::strong_count(&self.forwarder.session) > 1 {
                for screen in handle.snapshot().screens {
                    let _ = handle.set_screen_capture(screen.id, false).await;
                }
            }
        }
        let result = if Arc::strong_count(&self.forwarder.session) == 1 {
            self.forwarder.session.request_close();
            self.forwarder.session.ended().await
        } else {
            Ok(())
        };
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        if let Some(writer) = &mut self.local_preference_writer {
            writer.finish().await;
        }
        result
    }
}

fn flatten_signal_task(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    result.context("controller signaling task failed")?
}

mod connection;
mod viewing;
