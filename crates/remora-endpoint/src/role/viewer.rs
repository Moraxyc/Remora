//! Viewer role: room join + control request, bidirectional stream assembly, and lifecycle.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use prost::Message as _;
use rand::Rng as _;
use remora_proto::machine::peer::{PeerRole, parse_ice_servers};
use remora_proto::machine::signaling::SignalingEvent;
use remora_proto::pb;
use remora_proto::session::control::feature_flag_from_caps;
use remora_proto::session::input::InputMessage;
use remora_proto::signal::{ControlAck, SignalEvent, placeholder};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::peer::{PeerEventReceiver, PeerHandle, PeerSender, PeerSessionConfig, PeerSessionEvent};
use crate::platform::Platform;
use crate::role::host::resolve_probe_target;
use crate::{Endpoint, EndpointError, RoomEventStream, RoomSender};

const VIEWER_CMD_CAP: usize = 64;

/// Configuration for [`ViewerSession`].
#[derive(Debug, Clone)]
pub struct ViewerConfig {
    pub force_join: bool,
    pub force_relay: bool,
    pub prefer_relay: bool,
    pub enable_hevc: bool,
    pub enable_audio: bool,
    pub capture_type: i32,
    pub video_width: i32,
    pub video_height: i32,
    pub video_fps: i32,
    pub clipboard_enabled: bool,
    pub input_poll_interval: Duration,
    pub clipboard_poll_interval: Duration,
    pub connect_timeout: Duration,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            force_join: false,
            force_relay: false,
            prefer_relay: false,
            enable_hevc: false,
            enable_audio: true,
            capture_type: pb::connect_options::TypeEnum::CtDesktop as i32,
            video_width: 1920,
            video_height: 1080,
            video_fps: 30,
            clipboard_enabled: true,
            input_poll_interval: Duration::from_millis(16),
            clipboard_poll_interval: Duration::from_millis(50),
            connect_timeout: Duration::from_secs(15),
        }
    }
}

/// Live counters for a [`ViewerSession`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewerStats {
    pub ice_id: String,
    pub connected: bool,
    pub control_open: bool,
    pub peer_ready: bool,
    pub is_relayed: bool,
    pub video_frames_received: u64,
    pub audio_frames_received: u64,
    pub inputs_sent: u64,
    pub clipboard_received: u64,
    pub clipboard_sent: u64,
    pub last_clipboard: Option<String>,
    pub closed: bool,
}

enum ViewerCommand {
    SendInput(InputMessage),
    SendClipboard(String),
    RequestKeyframe,
    Close,
}

/// Managed viewer session driving room join, `control` negotiation, WebRTC peer, and [`Platform`] I/O.
pub struct ViewerSession {
    ice_id: String,
    cmd_tx: mpsc::Sender<ViewerCommand>,
    stats_rx: watch::Receiver<ViewerStats>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for ViewerSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewerSession")
            .field("ice_id", &self.ice_id)
            .field("stats", &*self.stats_rx.borrow())
            .finish()
    }
}

impl ViewerSession {
    /// Joins `target_device_id`'s room, negotiates `control`, and starts the WebRTC peer session.
    pub async fn connect(
        endpoint: &Endpoint,
        target_device_id: impl Into<String>,
        platform: Box<dyn Platform>,
        config: ViewerConfig,
    ) -> Result<Self, EndpointError> {
        let target_device_id = target_device_id.into();
        let room = if config.force_join {
            endpoint.connect_force(target_device_id).await?
        } else {
            endpoint.connect(target_device_id).await?
        };
        Self::connect_with_room(endpoint, room, platform, config).await
    }

    /// Connects to an already-authorized remote-assistance [`remora_proto::control::AssistRoomConfig`],
    /// negotiates `control`, and starts the WebRTC peer session.
    pub async fn connect_assist(
        endpoint: &Endpoint,
        assist_room: &remora_proto::control::AssistRoomConfig,
        platform: Box<dyn Platform>,
        config: ViewerConfig,
    ) -> Result<Self, EndpointError> {
        let room = endpoint.connect_assist_room(assist_room).await?;
        Self::connect_with_room(endpoint, room, platform, config).await
    }

    async fn connect_with_room(
        endpoint: &Endpoint,
        room: crate::RoomHandle,
        platform: Box<dyn Platform>,
        config: ViewerConfig,
    ) -> Result<Self, EndpointError> {
        let (room_sender, mut room_events) = room.split();

        let connect_deadline = tokio::time::sleep(config.connect_timeout);
        tokio::pin!(connect_deadline);

        // Wait for Socket.IO connection before sending control.
        loop {
            tokio::select! {
                () = &mut connect_deadline => {
                    let _ = room_sender.close().await;
                    return Err(EndpointError::SignalingClosed);
                }
                ev = room_events.next() => match ev {
                    Some(SignalingEvent::Connected) => break,
                    Some(SignalingEvent::ConnectError(err)) => {
                        let _ = room_sender.close().await;
                        return Err(EndpointError::ControlRefused(err.to_string()));
                    }
                    None => return Err(EndpointError::SignalingClosed),
                    _ => {}
                }
            }
        }

        let app_control_id = random_uuid_v4();
        let viewer_device_id = endpoint.device_id().await.unwrap_or_default();
        let profile = endpoint.profile();
        let mut decoder_caps = vec![pb::DecoderCap {
            fps: config.video_fps,
            codec_type: pb::CodecType::H264 as i32,
            resolution_width: config.video_width,
            resolution_height: config.video_height,
            chroma_format: pb::ChromaFormat::ChromaFormat420 as i32,
        }];
        if config.enable_hevc {
            decoder_caps.insert(
                0,
                pb::DecoderCap {
                    fps: config.video_fps,
                    codec_type: pb::CodecType::H265 as i32,
                    resolution_width: config.video_width,
                    resolution_height: config.video_height,
                    chroma_format: pb::ChromaFormat::ChromaFormat420 as i32,
                },
            );
        }

        let opts = pb::ConnectOptions {
            capture_type: config.capture_type,
            type_value: -1,
            capture_params: Some(pb::CaptureParams {
                fps: pb::Fps::Fps30 as i32,
                video_quality: pb::VideoQuality::Bluray as i32,
                cursor_capture: true,
                choose_resolution_type: pb::ChooseResolutionType::ChooseTypeFollowLocal as i32,
                local_resolution: Some(pb::ScreenResolution {
                    width: config.video_width,
                    height: config.video_height,
                }),
                choose_resolution: Some(pb::ScreenResolution::default()),
                chroma_format: pb::ChromaFormat::ChromaFormat420 as i32,
                max_custom_bitrate: 0,
                enable_hdr: false,
                auto_frame_quality: pb::VideoQuality::Unknown as i32,
                fps_count: config.video_fps,
            }),
            decoder_cap_list: decoder_caps,
            force_virtual_display: false,
            virtual_display_modes: vec![pb::VirtualDisplayMode {
                width: config.video_width,
                height: config.video_height,
                v_sync: config.video_fps,
            }],
            virtual_display_init_resolution: Some(pb::VirtualDisplayInitResolution {
                width: config.video_width,
                height: config.video_height,
            }),
            client_type: pb::ClientType::ClientWindows as i32,
            device_id: viewer_device_id,
            control_connect_type: 1,
            feature_flag: Some(feature_flag_from_caps(&profile.caps.features)),
            client_version: profile.version.clone(),
            entitlement_data: String::new(),
            current_server_time: 0,
        };

        let streamer_data = json!({
            "control_id": app_control_id,
            "device_capability": {
                "display_info": [{ "id": 1, "fps": config.video_fps, "type": 0, "hdr": 0 }],
                "ice_id": "",
                "video_codec_capability": [{
                    "video_codec": 1,
                    "chroma_sampling": 1,
                    "codec_impl": 34,
                    "bit_depth": 8,
                    "width": config.video_width,
                    "height": config.video_height
                }]
            }
        });
        let header = json!({
            "app_control_id": app_control_id,
            "app_data": placeholder(0),
            "streamer_data": streamer_data.to_string(),
        });

        room_sender
            .send_control(header, opts.encode_to_vec())
            .await?;

        let ack = loop {
            tokio::select! {
                () = &mut connect_deadline => {
                    let _ = room_sender.close().await;
                    return Err(EndpointError::SignalingClosed);
                }
                ev = room_events.next() => match ev {
                    Some(SignalingEvent::ControlResult(ack)) => break ack,
                    None => return Err(EndpointError::SignalingClosed),
                    _ => {}
                }
            }
        };

        let (ice_id, host_client_id, ack_app_id, ice_servers, cloud_force_relay) = match ack {
            ControlAck::Success {
                ice_id,
                client_id,
                app_control_id: ack_app,
                ice_servers,
                force_relay,
            } => (ice_id, client_id, ack_app, ice_servers, force_relay),
            ControlAck::Failure { reason } => {
                let _ = room_sender.close().await;
                return Err(EndpointError::ControlRefused(
                    reason.unwrap_or_else(|| "refused".to_owned()),
                ));
            }
            other => {
                let _ = room_sender.close().await;
                return Err(EndpointError::ControlRefused(format!("{other:?}")));
            }
        };

        let mut peer_cfg = PeerSessionConfig::new(PeerRole::Viewer, ice_id.clone());
        peer_cfg.remote_client_id = Some(host_client_id);
        peer_cfg.app_control_id = ack_app_id.or(Some(app_control_id));
        peer_cfg.ice_servers = parse_ice_servers(&ice_servers);
        peer_cfg.force_relay = cloud_force_relay || config.force_relay;
        peer_cfg.prefer_relay = config.prefer_relay;
        peer_cfg.feature_flag = feature_flag_from_caps(&profile.caps.features);
        peer_cfg.enable_hevc = config.enable_hevc;
        peer_cfg.enable_audio = config.enable_audio;
        peer_cfg.video_width = config.video_width;
        peer_cfg.video_height = config.video_height;
        peer_cfg.video_fps = config.video_fps;
        peer_cfg.clipboard_enabled = config.clipboard_enabled;
        peer_cfg.local_probe_target =
            resolve_probe_target(endpoint.http().base_url().as_str()).await;

        let peer = PeerHandle::spawn(peer_cfg, Arc::new(room_sender.clone())).await?;
        let (peer_sender, peer_events) = peer.split();
        let initial_stats = ViewerStats {
            ice_id: ice_id.clone(),
            ..Default::default()
        };
        let (cmd_tx, cmd_rx) = mpsc::channel(VIEWER_CMD_CAP);
        let (stats_tx, stats_rx) = watch::channel(initial_stats.clone());

        let supervisor = ViewerSupervisor {
            ice_id: ice_id.clone(),
            config,
            platform,
            room_sender,
            room_events,
            peer_sender,
            peer_events,
            cmd_rx,
            stats: initial_stats,
            stats_tx,
            last_clipboard_sent: None,
        };
        let task = tokio::spawn(supervisor.run());

        Ok(Self {
            ice_id,
            cmd_tx,
            stats_rx,
            task: Some(task),
        })
    }

    pub fn ice_id(&self) -> &str {
        &self.ice_id
    }

    pub fn stats(&self) -> ViewerStats {
        self.stats_rx.borrow().clone()
    }

    pub async fn send_input(&self, input: InputMessage) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(ViewerCommand::SendInput(input))
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_clipboard(&self, text: impl Into<String>) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(ViewerCommand::SendClipboard(text.into()))
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn request_keyframe(&self) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(ViewerCommand::RequestKeyframe)
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    /// Waits until the echo handshake completes (`peer_ready == true`) or `timeout` expires.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<ViewerStats, EndpointError> {
        self.wait_until(timeout, |s| s.peer_ready).await
    }

    /// Waits until at least `min_frames` encoded video frames have arrived on the video track.
    pub async fn wait_frames(
        &self,
        min_frames: u64,
        timeout: Duration,
    ) -> Result<ViewerStats, EndpointError> {
        self.wait_until(timeout, |s| s.video_frames_received >= min_frames)
            .await
    }

    /// Waits until `last_clipboard` equals `expected`.
    pub async fn wait_clipboard(
        &self,
        expected: &str,
        timeout: Duration,
    ) -> Result<ViewerStats, EndpointError> {
        self.wait_until(timeout, |s| s.last_clipboard.as_deref() == Some(expected))
            .await
    }

    async fn wait_until(
        &self,
        timeout: Duration,
        pred: impl Fn(&ViewerStats) -> bool,
    ) -> Result<ViewerStats, EndpointError> {
        let mut rx = self.stats_rx.clone();
        tokio::time::timeout(timeout, async move {
            loop {
                {
                    let cur = rx.borrow();
                    if pred(&cur) {
                        return Ok(cur.clone());
                    }
                    if cur.closed {
                        return Err(EndpointError::SignalingClosed);
                    }
                }
                if rx.changed().await.is_err() {
                    return Err(EndpointError::SignalingClosed);
                }
            }
        })
        .await
        .map_err(|_| EndpointError::SignalingClosed)?
    }

    pub async fn close(mut self) {
        let _ = self.cmd_tx.send(ViewerCommand::Close).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

struct ViewerSupervisor {
    ice_id: String,
    config: ViewerConfig,
    platform: Box<dyn Platform>,
    room_sender: RoomSender,
    room_events: RoomEventStream,
    peer_sender: PeerSender,
    peer_events: PeerEventReceiver,
    cmd_rx: mpsc::Receiver<ViewerCommand>,
    stats: ViewerStats,
    stats_tx: watch::Sender<ViewerStats>,
    last_clipboard_sent: Option<String>,
}

impl ViewerSupervisor {
    fn publish_stats(&self) {
        let _ = self.stats_tx.send(self.stats.clone());
    }

    async fn run(mut self) {
        let mut input_tick = tokio::time::interval(self.config.input_poll_interval);
        input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut clip_tick = tokio::time::interval(self.config.clipboard_poll_interval);
        clip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                ev = self.room_events.next() => {
                    let Some(ev) = ev else {
                        break;
                    };
                    match ev {
                        SignalingEvent::Signal(SignalEvent::Soac(soac)) => {
                            let _ = self.peer_sender.feed_soac(soac).await;
                        }
                        SignalingEvent::Signal(SignalEvent::Released(rel)) => {
                            if rel.ice_id.as_deref().is_none_or(|id| id == self.ice_id) {
                                break;
                            }
                        }
                        SignalingEvent::Signal(SignalEvent::Left(left)) => {
                            if left.is_room_kick()
                                || left.ice_id.as_deref() == Some(self.ice_id.as_str())
                            {
                                break;
                            }
                        }
                        SignalingEvent::Signal(SignalEvent::Leave(left))
                            if left.is_room_kick()
                                || left.ice_id.as_deref() == Some(self.ice_id.as_str()) =>
                        {
                            break;
                        }
                        _ => {}
                    }
                }
                pev = self.peer_events.recv() => {
                    let Some(pev) = pev else {
                        break;
                    };
                    match pev {
                        PeerSessionEvent::Connected { is_relayed, .. } => {
                            self.stats.connected = true;
                            self.stats.is_relayed = is_relayed;
                            self.publish_stats();
                        }
                        PeerSessionEvent::ControlChannelReady { .. } => {
                            self.stats.control_open = true;
                            self.publish_stats();
                        }
                        PeerSessionEvent::PeerReady { .. } => {
                            self.stats.peer_ready = true;
                            self.publish_stats();
                        }
                        PeerSessionEvent::VideoFrame(frame) => {
                            if let Some(sink) = self.platform.video_sink() {
                                if let Err(err) = sink.push_frame(&frame) {
                                    debug!(error = %err, "video sink rejected frame");
                                } else {
                                    self.stats.video_frames_received += 1;
                                    self.publish_stats();
                                }
                            } else {
                                self.stats.video_frames_received += 1;
                                self.publish_stats();
                            }
                        }
                        PeerSessionEvent::AudioFrame(frame) => {
                            if let Some(sink) = self.platform.audio_sink() {
                                let _ = sink.push_frame(&frame);
                            }
                            self.stats.audio_frames_received += 1;
                            self.publish_stats();
                        }
                        PeerSessionEvent::ClipboardText(text) => {
                            self.last_clipboard_sent = Some(text.clone());
                            self.stats.last_clipboard = Some(text.clone());
                            if let Some(cb) = self.platform.clipboard() {
                                let _ = cb.set_text(&text);
                            }
                            self.stats.clipboard_received += 1;
                            self.publish_stats();
                        }
                        PeerSessionEvent::Closed { .. } => {
                            break;
                        }
                        _ => {}
                    }
                }
                _ = input_tick.tick(), if self.stats.peer_ready => {
                    if let Some(src) = self.platform.input_source()
                        && let Ok(Some(msg)) = src.next_event()
                        && self.peer_sender.send_input(msg).await.is_ok()
                    {
                        self.stats.inputs_sent += 1;
                        self.publish_stats();
                    }
                }
                _ = clip_tick.tick(), if self.stats.peer_ready => {
                    if let Some(cb) = self.platform.clipboard()
                        && let Ok(Some(text)) = cb.get_text()
                        && self.last_clipboard_sent.as_deref() != Some(text.as_str())
                    {
                        self.last_clipboard_sent = Some(text.clone());
                        if self.peer_sender.send_clipboard(text).await.is_ok() {
                            self.stats.clipboard_sent += 1;
                            self.publish_stats();
                        }
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(ViewerCommand::SendInput(input)) => {
                            if self.peer_sender.send_input(input).await.is_ok() {
                                self.stats.inputs_sent += 1;
                                self.publish_stats();
                            }
                        }
                        Some(ViewerCommand::SendClipboard(text)) => {
                            self.last_clipboard_sent = Some(text.clone());
                            if let Some(cb) = self.platform.clipboard() {
                                let _ = cb.set_text(&text);
                            }
                            if self.peer_sender.send_clipboard(text).await.is_ok() {
                                self.stats.clipboard_sent += 1;
                                self.publish_stats();
                            }
                        }
                        Some(ViewerCommand::RequestKeyframe) => {
                            let _ = self.peer_sender.request_keyframe().await;
                        }
                        Some(ViewerCommand::Close) | None => {
                            break;
                        }
                    }
                }
            }
        }

        self.stats.closed = true;
        self.publish_stats();
        self.peer_sender.close().await;
        let _ = self.room_sender.close().await;
    }
}

fn random_uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}
