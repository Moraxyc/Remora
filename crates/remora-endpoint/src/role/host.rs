//! Host role: connection authorization, bidirectional stream assembly, and peer lifecycle.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use prost::Message as _;
use remora_proto::machine::peer::{PeerRole, parse_ice_servers};
use remora_proto::machine::signaling::SignalingEvent;
use remora_proto::pb;
use remora_proto::session::DataChannel;
use remora_proto::session::control::{ControlMessage, feature_flag_from_caps};
use remora_proto::session::terminal::TermFrame;
use remora_proto::session::tunnel::TunnelFrame;
use remora_proto::signal::SignalEvent;

use super::{HostToolEvent, ToolEvent};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::peer::{PeerHandle, PeerSender, PeerSessionConfig, PeerSessionEvent};
use crate::platform::{EncodedAudioFrame, EncodedVideoFrame, Platform};
use crate::{Endpoint, EndpointError, RoomEventStream, RoomSender};

const HOST_CMD_CAP: usize = 64;
const PEER_EVENT_CAP: usize = 256;
const TOOL_EVENT_CAP: usize = 128;

/// Metadata supplied to the host's authorization callback on incoming `be-controlled` requests.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    pub ice_id: String,
    pub client_id: String,
    pub app_control_id: Option<String>,
    pub controller_platform: Option<u8>,
    pub connect_options: Option<pb::ConnectOptions>,
    pub force_relay: bool,
    pub is_desktop: bool,
}

/// Decision returned by a [`HostAuthorizer`] callback.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthorizationDecision {
    Accept,
    Reject { reason: String },
}

/// Callback invoked for each incoming `be-controlled` session before spawning a WebRTC peer.
pub type HostAuthorizer = Arc<dyn Fn(&AuthorizationRequest) -> AuthorizationDecision + Send + Sync>;

/// Configuration for [`HostSession`].
#[derive(Clone)]
pub struct HostConfig {
    pub authorizer: Option<HostAuthorizer>,
    pub force_relay: bool,
    pub prefer_relay: bool,
    pub enable_hevc: bool,
    pub enable_audio: bool,
    pub video_width: i32,
    pub video_height: i32,
    pub video_fps: i32,
    pub video_poll_interval: Duration,
    pub clipboard_poll_interval: Duration,
    pub replace_existing_desktop: bool,
    pub connect_timeout: Duration,
}

impl fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostConfig")
            .field("has_authorizer", &self.authorizer.is_some())
            .field("force_relay", &self.force_relay)
            .field("prefer_relay", &self.prefer_relay)
            .field("enable_hevc", &self.enable_hevc)
            .field("enable_audio", &self.enable_audio)
            .field("replace_existing_desktop", &self.replace_existing_desktop)
            .finish()
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            authorizer: None,
            force_relay: false,
            prefer_relay: false,
            enable_hevc: false,
            enable_audio: true,
            video_width: 1920,
            video_height: 1080,
            video_fps: 30,
            video_poll_interval: Duration::from_millis(16),
            clipboard_poll_interval: Duration::from_millis(50),
            replace_existing_desktop: true,
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// Live counters for a [`HostSession`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostStats {
    pub signaling_connected: bool,
    pub active_peers: usize,
    pub authorized_sessions: u64,
    pub rejected_sessions: u64,
    pub connected: bool,
    pub peer_ready: bool,
    pub tool_channels_ready: bool,
    pub is_relayed: bool,
    pub video_frames_sent: u64,
    pub audio_frames_sent: u64,
    pub inputs_injected: u64,
    pub clipboard_received: u64,
    pub clipboard_sent: u64,
    pub last_peer_ice_id: Option<String>,
}

enum HostCommand {
    PushVideoFrame(EncodedVideoFrame),
    PushAudioFrame(EncodedAudioFrame),
    SetClipboard(String),
    SendFileResponse {
        ice_id: Option<String>,
        request_id: i64,
        which: pb::file_transfer_ftp_response::Which,
    },
    SendTunnelFrame {
        ice_id: Option<String>,
        frame: TunnelFrame,
    },
    SendTermFrame {
        ice_id: Option<String>,
        frame: TermFrame,
    },
    SendTerminalSessionChanged {
        ice_id: Option<String>,
    },
    SendPrivacyResponse {
        ice_id: Option<String>,
        request_id: i64,
        response: pb::PrivateScreenSettingResponse,
    },
    SendDrawResponse {
        ice_id: Option<String>,
        request_id: i64,
        response: pb::DrawResponse,
    },
    SendVirtualAudioPolicyResponse {
        ice_id: Option<String>,
        channel: DataChannel,
        request_id: i64,
        error_code: i32,
    },
    SendControl {
        ice_id: Option<String>,
        msg: ControlMessage,
    },
    Close,
}

/// Cloneable weak handle for sending tool-channel and system replies through a running [`HostSession`].
///
/// Holding a `HostToolSender` does not keep the host command channel open after `HostSession` is dropped.
#[derive(Clone, Debug)]
pub struct HostToolSender {
    cmd_tx: mpsc::WeakSender<HostCommand>,
}

impl HostToolSender {
    async fn send(&self, cmd: HostCommand) -> Result<(), EndpointError> {
        let tx = self
            .cmd_tx
            .upgrade()
            .ok_or(EndpointError::SignalingClosed)?;
        tx.send(cmd)
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_file_response(
        &self,
        ice_id: Option<&str>,
        request_id: i64,
        which: pb::file_transfer_ftp_response::Which,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendFileResponse {
            ice_id: ice_id.map(str::to_owned),
            request_id,
            which,
        })
        .await
    }

    pub async fn send_tunnel_frame(
        &self,
        ice_id: Option<&str>,
        frame: TunnelFrame,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendTunnelFrame {
            ice_id: ice_id.map(str::to_owned),
            frame,
        })
        .await
    }

    pub async fn send_term_frame(
        &self,
        ice_id: Option<&str>,
        frame: TermFrame,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendTermFrame {
            ice_id: ice_id.map(str::to_owned),
            frame,
        })
        .await
    }

    pub async fn send_terminal_session_changed(
        &self,
        ice_id: Option<&str>,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendTerminalSessionChanged {
            ice_id: ice_id.map(str::to_owned),
        })
        .await
    }

    pub async fn send_privacy_response(
        &self,
        ice_id: Option<&str>,
        request_id: i64,
        response: pb::PrivateScreenSettingResponse,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendPrivacyResponse {
            ice_id: ice_id.map(str::to_owned),
            request_id,
            response,
        })
        .await
    }

    pub async fn send_draw_response(
        &self,
        ice_id: Option<&str>,
        request_id: i64,
        response: pb::DrawResponse,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendDrawResponse {
            ice_id: ice_id.map(str::to_owned),
            request_id,
            response,
        })
        .await
    }

    pub async fn send_virtual_audio_policy_response(
        &self,
        ice_id: Option<&str>,
        channel: DataChannel,
        request_id: i64,
        error_code: i32,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendVirtualAudioPolicyResponse {
            ice_id: ice_id.map(str::to_owned),
            channel,
            request_id,
            error_code,
        })
        .await
    }

    pub async fn send_control(
        &self,
        ice_id: Option<&str>,
        msg: ControlMessage,
    ) -> Result<(), EndpointError> {
        self.send(HostCommand::SendControl {
            ice_id: ice_id.map(str::to_owned),
            msg,
        })
        .await
    }
}

/// Managed host session driving signaling, authorization, WebRTC peers, and [`Platform`] I/O.
pub struct HostSession {
    device_id: String,
    cmd_tx: mpsc::Sender<HostCommand>,
    stats_rx: watch::Receiver<HostStats>,
    tool_events: Option<mpsc::Receiver<HostToolEvent>>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for HostSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostSession")
            .field("device_id", &self.device_id)
            .field("stats", &*self.stats_rx.borrow())
            .finish()
    }
}

impl HostSession {
    /// Registers the host device (if needed), joins signaling, and starts serving peers.
    pub async fn start(
        endpoint: &Endpoint,
        platform: Box<dyn Platform>,
        config: HostConfig,
    ) -> Result<Self, EndpointError> {
        let room = endpoint.register_host().await?;
        let device_id = room.device_id().to_owned();
        let (room_sender, mut room_events) = room.split();

        let mut signaling_connected = false;
        let deadline = tokio::time::sleep(config.connect_timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => break,
                ev = room_events.next() => match ev {
                    Some(SignalingEvent::Connected) => {
                        signaling_connected = true;
                        break;
                    }
                    Some(SignalingEvent::ConnectError(err)) => {
                        let _ = room_sender.close().await;
                        return Err(EndpointError::ControlRefused(err.to_string()));
                    }
                    None => {
                        return Err(EndpointError::SignalingClosed);
                    }
                    _ => {}
                }
            }
        }

        let probe_target = resolve_probe_target(endpoint.http().base_url().as_str()).await;
        let host_features = feature_flag_from_caps(&endpoint.profile().caps.features);

        let initial_stats = HostStats {
            signaling_connected,
            ..Default::default()
        };
        let (cmd_tx, cmd_rx) = mpsc::channel(HOST_CMD_CAP);
        let (stats_tx, stats_rx) = watch::channel(initial_stats);
        let (tool_tx, tool_rx) = mpsc::channel(TOOL_EVENT_CAP);

        let supervisor = HostSupervisor {
            config,
            platform,
            host_features,
            probe_target,
            room_sender,
            room_events,
            cmd_rx,
            stats: HostStats {
                signaling_connected,
                ..Default::default()
            },
            stats_tx,
            tool_tx,
            peers: HashMap::new(),
            last_clipboard_sent: None,
        };

        let task = tokio::spawn(supervisor.run());
        Ok(Self {
            device_id,
            cmd_tx,
            stats_rx,
            tool_events: Some(tool_rx),
            task: Some(task),
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn stats(&self) -> HostStats {
        self.stats_rx.borrow().clone()
    }

    pub async fn push_video_frame(&self, frame: EncodedVideoFrame) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::PushVideoFrame(frame))
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn push_audio_frame(&self, frame: EncodedAudioFrame) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::PushAudioFrame(frame))
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn set_clipboard(&self, text: impl Into<String>) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SetClipboard(text.into()))
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    /// Takes ownership of the bounded inbound tool-channel event receiver.
    pub fn take_tool_events(&mut self) -> Option<mpsc::Receiver<HostToolEvent>> {
        self.tool_events.take()
    }

    /// Returns a cloneable weak sender for tool-channel and system replies.
    pub fn tool_sender(&self) -> HostToolSender {
        HostToolSender {
            cmd_tx: self.cmd_tx.downgrade(),
        }
    }

    pub async fn send_file_response(
        &self,
        ice_id: Option<&str>,
        request_id: i64,
        which: pb::file_transfer_ftp_response::Which,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendFileResponse {
                ice_id: ice_id.map(str::to_owned),
                request_id,
                which,
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_tunnel_frame(
        &self,
        ice_id: Option<&str>,
        frame: TunnelFrame,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendTunnelFrame {
                ice_id: ice_id.map(str::to_owned),
                frame,
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_term_frame(
        &self,
        ice_id: Option<&str>,
        frame: TermFrame,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendTermFrame {
                ice_id: ice_id.map(str::to_owned),
                frame,
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_terminal_session_changed(
        &self,
        ice_id: Option<&str>,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendTerminalSessionChanged {
                ice_id: ice_id.map(str::to_owned),
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_privacy_response(
        &self,
        ice_id: Option<&str>,
        request_id: i64,
        response: pb::PrivateScreenSettingResponse,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendPrivacyResponse {
                ice_id: ice_id.map(str::to_owned),
                request_id,
                response,
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_draw_response(
        &self,
        ice_id: Option<&str>,
        request_id: i64,
        response: pb::DrawResponse,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendDrawResponse {
                ice_id: ice_id.map(str::to_owned),
                request_id,
                response,
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_virtual_audio_policy_response(
        &self,
        ice_id: Option<&str>,
        channel: DataChannel,
        request_id: i64,
        error_code: i32,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendVirtualAudioPolicyResponse {
                ice_id: ice_id.map(str::to_owned),
                channel,
                request_id,
                error_code,
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    pub async fn send_control(
        &self,
        ice_id: Option<&str>,
        msg: ControlMessage,
    ) -> Result<(), EndpointError> {
        self.cmd_tx
            .send(HostCommand::SendControl {
                ice_id: ice_id.map(str::to_owned),
                msg,
            })
            .await
            .map_err(|_| EndpointError::SignalingClosed)
    }

    /// Waits until a peer completes the echo handshake (`peer_ready == true`) or `timeout` elapses.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<HostStats, EndpointError> {
        let mut rx = self.stats_rx.clone();
        tokio::time::timeout(timeout, async move {
            loop {
                {
                    let cur = rx.borrow();
                    if cur.peer_ready {
                        return cur.clone();
                    }
                }
                if rx.changed().await.is_err() {
                    return rx.borrow().clone();
                }
            }
        })
        .await
        .map_err(|_| EndpointError::SignalingClosed)
    }

    /// Gracefully shuts down all active peers and closes the signaling room.
    pub async fn close(mut self) {
        let _ = self.cmd_tx.send(HostCommand::Close).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

struct ActiveHostPeer {
    sender: PeerSender,
    controller_platform: Option<u8>,
    is_desktop: bool,
    ready: bool,
    tool_channels_ready: bool,
    closing: bool,
    forward_task: JoinHandle<()>,
}

struct HostSupervisor {
    config: HostConfig,
    platform: Box<dyn Platform>,
    host_features: pb::FeatureFlag,
    probe_target: Option<SocketAddr>,
    room_sender: RoomSender,
    room_events: RoomEventStream,
    cmd_rx: mpsc::Receiver<HostCommand>,
    stats: HostStats,
    stats_tx: watch::Sender<HostStats>,
    tool_tx: mpsc::Sender<HostToolEvent>,
    peers: HashMap<String, ActiveHostPeer>,
    last_clipboard_sent: Option<String>,
}

impl HostSupervisor {
    fn publish_stats(&self) {
        let _ = self.stats_tx.send(self.stats.clone());
    }

    async fn run(mut self) {
        let (peer_ev_tx, mut peer_ev_rx) =
            mpsc::channel::<(String, PeerSessionEvent)>(PEER_EVENT_CAP);
        let mut media_tick = tokio::time::interval(self.config.video_poll_interval);
        media_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut clip_tick = tokio::time::interval(self.config.clipboard_poll_interval);
        clip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                ev = self.room_events.next() => {
                    let Some(ev) = ev else {
                        break;
                    };
                    match ev {
                        SignalingEvent::Connected => {
                            self.stats.signaling_connected = true;
                            self.publish_stats();
                        }
                        SignalingEvent::Signal(SignalEvent::BeControlled(bc)) => {
                            self.handle_be_controlled(bc, &peer_ev_tx).await;
                        }
                        SignalingEvent::Signal(SignalEvent::Soac(soac)) => {
                            if let Some(ice_id) = soac.ice_id().map(str::to_owned)
                                && let Some(peer) = self.peers.get(&ice_id)
                            {
                                let _ = peer.sender.feed_soac(soac).await;
                            }
                        }
                        SignalingEvent::Signal(SignalEvent::Released(rel)) => {
                            if let Some(ice_id) = rel.ice_id {
                                self.begin_close_peer(&ice_id).await;
                            }
                        }
                        SignalingEvent::Signal(SignalEvent::Left(left)) => {
                            if left.is_room_kick() {
                                break;
                            }
                            if let Some(ice_id) = left.ice_id {
                                self.begin_close_peer(&ice_id).await;
                            }
                        }
                        SignalingEvent::Signal(SignalEvent::Leave(left)) => {
                            if left.is_room_kick() {
                                break;
                            }
                            if let Some(ice_id) = left.ice_id {
                                self.begin_close_peer(&ice_id).await;
                            }
                        }
                        _ => {}
                    }
                }
                Some((ice_id, pev)) = peer_ev_rx.recv() => {
                    self.handle_peer_event(&ice_id, pev).await;
                }
                _ = media_tick.tick(), if self.has_ready_desktop_peer() => {
                    self.pump_platform_media().await;
                }
                _ = clip_tick.tick(), if self.has_ready_desktop_peer() => {
                    self.pump_platform_clipboard().await;
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(HostCommand::PushVideoFrame(frame)) => {
                            self.broadcast_video_frame(frame).await;
                        }
                        Some(HostCommand::PushAudioFrame(frame)) => {
                            self.broadcast_audio_frame(frame).await;
                        }
                        Some(HostCommand::SetClipboard(text)) => {
                            if let Some(cb) = self.platform.clipboard() {
                                let _ = cb.set_text(&text);
                            }
                            self.last_clipboard_sent = Some(text.clone());
                            self.broadcast_clipboard(&text).await;
                        }
                        Some(HostCommand::SendFileResponse { ice_id, request_id, which }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender.send_file_response(request_id, which).await;
                            }
                        }
                        Some(HostCommand::SendTunnelFrame { ice_id, frame }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender.send_tunnel_frame(frame).await;
                            }
                        }
                        Some(HostCommand::SendTermFrame { ice_id, frame }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender.send_term_frame(frame).await;
                            }
                        }
                        Some(HostCommand::SendTerminalSessionChanged { ice_id }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender.send_terminal_session_changed().await;
                            }
                        }
                        Some(HostCommand::SendPrivacyResponse { ice_id, request_id, response }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender.send_privacy_response(request_id, response).await;
                            }
                        }
                        Some(HostCommand::SendDrawResponse { ice_id, request_id, response }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender.send_draw_response(request_id, response).await;
                            }
                        }
                        Some(HostCommand::SendVirtualAudioPolicyResponse {
                            ice_id,
                            channel,
                            request_id,
                            error_code,
                        }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender
                                    .send_virtual_audio_policy_response(channel, request_id, error_code)
                                    .await;
                            }
                        }
                        Some(HostCommand::SendControl { ice_id, msg }) => {
                            if let Some(sender) = self.resolve_peer_sender(ice_id.as_deref()) {
                                let _ = sender.send_control(msg).await;
                            }
                        }
                        Some(HostCommand::Close) | None => {
                            break;
                        }
                    }
                }
            }
        }

        let ids: Vec<String> = self.peers.keys().cloned().collect();
        for id in ids {
            self.remove_peer(&id).await;
        }
        let _ = self.room_sender.close().await;
    }

    fn has_ready_desktop_peer(&self) -> bool {
        self.peers
            .values()
            .any(|p| !p.closing && p.ready && p.is_desktop)
    }

    async fn handle_be_controlled(
        &mut self,
        bc: remora_proto::signal::BeControlledEvent,
        peer_ev_tx: &mpsc::Sender<(String, PeerSessionEvent)>,
    ) {
        let connect_options = (!bc.connect_options.is_empty())
            .then(|| pb::ConnectOptions::decode(bc.connect_options.as_slice()).ok())
            .flatten();
        let is_desktop = connect_options
            .as_ref()
            .is_none_or(|o| o.capture_type == 0 || o.capture_type == 1);

        let auth_req = AuthorizationRequest {
            ice_id: bc.ice_id.clone(),
            client_id: bc.client_id.clone(),
            app_control_id: bc.app_control_id.clone(),
            controller_platform: bc.controller_platform,
            connect_options,
            force_relay: bc.force_relay || self.config.force_relay,
            is_desktop,
        };

        if let Some(auth) = &self.config.authorizer
            && let AuthorizationDecision::Reject { reason } = auth(&auth_req)
        {
            info!(ice_id = %bc.ice_id, reason = %reason, "host rejected incoming control request");
            self.stats.rejected_sessions += 1;
            self.publish_stats();
            return;
        }

        self.stats.authorized_sessions += 1;

        if is_desktop && self.config.replace_existing_desktop {
            let old_desktop: Vec<String> = self
                .peers
                .iter()
                .filter(|(_, p)| p.is_desktop)
                .map(|(id, _)| id.clone())
                .collect();
            for id in old_desktop {
                self.remove_peer(&id).await;
            }
        }
        if self.peers.contains_key(&bc.ice_id) {
            self.remove_peer(&bc.ice_id).await;
        }

        let mut peer_cfg = PeerSessionConfig::new(PeerRole::Host, bc.ice_id.clone());
        peer_cfg.remote_client_id = Some(bc.client_id);
        peer_cfg.app_control_id = bc.app_control_id;
        peer_cfg.ice_servers = parse_ice_servers(&bc.ice_servers);
        peer_cfg.force_relay = bc.force_relay || self.config.force_relay;
        peer_cfg.prefer_relay = self.config.prefer_relay;
        peer_cfg.controller_platform = bc.controller_platform;
        peer_cfg.feature_flag = self.host_features;
        peer_cfg.enable_hevc = self.config.enable_hevc;
        peer_cfg.enable_audio = self.config.enable_audio;
        peer_cfg.video_width = self.config.video_width;
        peer_cfg.video_height = self.config.video_height;
        peer_cfg.video_fps = self.config.video_fps;
        peer_cfg.clipboard_enabled = self.platform.clipboard().is_some();
        peer_cfg.local_probe_target = self.probe_target;

        let signaling = Arc::new(self.room_sender.clone());
        match PeerHandle::spawn(peer_cfg, signaling).await {
            Ok(handle) => {
                let (sender, mut events) = handle.split();
                let ev_tx = peer_ev_tx.clone();
                let ice_id = bc.ice_id.clone();
                let forward_task = tokio::spawn(async move {
                    while let Some(ev) = events.recv().await {
                        if ev_tx.send((ice_id.clone(), ev)).await.is_err() {
                            break;
                        }
                    }
                });
                self.peers.insert(
                    bc.ice_id.clone(),
                    ActiveHostPeer {
                        sender,
                        controller_platform: bc.controller_platform,
                        is_desktop,
                        ready: false,
                        tool_channels_ready: false,
                        closing: false,
                        forward_task,
                    },
                );
                self.stats.active_peers = self.peers.len();
                self.stats.last_peer_ice_id = Some(bc.ice_id);
                self.publish_stats();
            }
            Err(err) => {
                warn!(ice_id = %bc.ice_id, error = %err, "failed to spawn host WebRTC peer");
            }
        }
    }

    async fn handle_peer_event(&mut self, ice_id: &str, ev: PeerSessionEvent) {
        match ev {
            PeerSessionEvent::Connected { is_relayed, .. } => {
                self.stats.connected = true;
                self.stats.is_relayed = is_relayed;
                self.publish_stats();
            }
            PeerSessionEvent::ToolChannelsReady { .. } => {
                if let Some(peer) = self.peers.get_mut(ice_id) {
                    peer.tool_channels_ready = true;
                    self.stats.tool_channels_ready = true;
                    self.publish_stats();
                }
            }
            PeerSessionEvent::ToolChannelsClosed { .. } => {
                if let Some(peer) = self.peers.get_mut(ice_id) {
                    peer.tool_channels_ready = false;
                    self.stats.tool_channels_ready =
                        self.peers.values().any(|p| p.tool_channels_ready);
                    self.publish_stats();
                }
            }
            PeerSessionEvent::PeerReady { .. } => {
                let mut should_pump = false;
                if let Some(peer) = self.peers.get_mut(ice_id) {
                    peer.ready = true;
                    should_pump = !peer.closing && peer.is_desktop;
                    self.stats.peer_ready = true;
                    self.publish_stats();
                }
                if should_pump {
                    self.pump_platform_media().await;
                }
            }
            PeerSessionEvent::Input(input) => {
                if let Some(sink) = self.platform.input_sink() {
                    if let Err(err) = sink.inject(&input) {
                        debug!(error = %err, "input injection failed");
                    } else {
                        self.stats.inputs_injected += 1;
                        self.publish_stats();
                    }
                }
            }
            PeerSessionEvent::ClipboardText(text) => {
                self.last_clipboard_sent = Some(text.clone());
                if let Some(cb) = self.platform.clipboard() {
                    let _ = cb.set_text(&text);
                }
                self.stats.clipboard_received += 1;
                self.publish_stats();
            }
            PeerSessionEvent::File(msg) => {
                self.forward_tool_event(ice_id, ToolEvent::File(msg));
            }
            PeerSessionEvent::Tunnel(frame) => {
                self.forward_tool_event(ice_id, ToolEvent::Tunnel(frame));
            }
            PeerSessionEvent::TermFrame(frame) => {
                self.forward_tool_event(ice_id, ToolEvent::TermFrame(frame));
            }
            PeerSessionEvent::TerminalControl(msg) => {
                self.forward_tool_event(ice_id, ToolEvent::TerminalControl(msg));
            }
            PeerSessionEvent::System(msg) => {
                self.forward_tool_event(ice_id, ToolEvent::System(msg));
            }
            PeerSessionEvent::Closed { .. } => {
                self.remove_peer(ice_id).await;
            }
            _ => {}
        }
    }

    async fn pump_platform_media(&mut self) {
        if let Some(vs) = self.platform.video_source() {
            match vs.next_frame() {
                Ok(Some(frame)) => {
                    self.broadcast_video_frame(frame).await;
                }
                Ok(None) => {}
                Err(err) => {
                    debug!(error = %err, "video source error");
                }
            }
        }
        if let Some(aus) = self.platform.audio_source()
            && let Ok(Some(frame)) = aus.next_frame()
        {
            self.broadcast_audio_frame(frame).await;
        }
    }

    async fn pump_platform_clipboard(&mut self) {
        let Some(cb) = self.platform.clipboard() else {
            return;
        };
        if let Ok(Some(text)) = cb.get_text()
            && self.last_clipboard_sent.as_deref() != Some(text.as_str())
        {
            self.last_clipboard_sent = Some(text.clone());
            self.broadcast_clipboard(&text).await;
        }
    }

    async fn broadcast_video_frame(&mut self, frame: EncodedVideoFrame) {
        let mut sent_any = false;
        for peer in self.peers.values() {
            if !peer.closing
                && peer.is_desktop
                && peer.ready
                && peer.sender.send_video_frame(frame.clone()).await.is_ok()
            {
                sent_any = true;
            }
        }
        if sent_any {
            self.stats.video_frames_sent += 1;
            self.publish_stats();
        }
    }

    async fn broadcast_audio_frame(&mut self, frame: EncodedAudioFrame) {
        let mut sent_any = false;
        for peer in self.peers.values() {
            if !peer.closing
                && peer.is_desktop
                && peer.ready
                && peer.sender.send_audio_frame(frame.clone()).await.is_ok()
            {
                sent_any = true;
            }
        }
        if sent_any {
            self.stats.audio_frames_sent += 1;
            self.publish_stats();
        }
    }

    async fn broadcast_clipboard(&mut self, text: &str) {
        let mut sent_any = false;
        for peer in self.peers.values() {
            if !peer.closing && peer.ready && peer.sender.send_clipboard(text).await.is_ok() {
                sent_any = true;
            }
        }
        if sent_any {
            self.stats.clipboard_sent += 1;
            self.publish_stats();
        }
    }

    async fn begin_close_peer(&mut self, ice_id: &str) {
        if let Some(peer) = self.peers.get_mut(ice_id)
            && !peer.closing
        {
            peer.closing = true;
            peer.sender
                .drain_and_close(Duration::from_millis(150))
                .await;
        }
    }

    fn resolve_peer_sender(&self, ice_id: Option<&str>) -> Option<&PeerSender> {
        if let Some(id) = ice_id {
            return self.peers.get(id).filter(|p| !p.closing).map(|p| &p.sender);
        }
        if let Some(last) = &self.stats.last_peer_ice_id
            && let Some(peer) = self.peers.get(last)
            && !peer.closing
        {
            return Some(&peer.sender);
        }
        self.peers.values().find(|p| !p.closing).map(|p| &p.sender)
    }

    fn forward_tool_event(&self, ice_id: &str, event: ToolEvent) {
        let controller_platform = self.peers.get(ice_id).and_then(|p| p.controller_platform);
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tool_tx.try_send(HostToolEvent {
            ice_id: ice_id.to_owned(),
            controller_platform,
            event,
        }) {
            warn!(ice_id = %ice_id, "host tool_events queue full; dropped event");
        }
    }

    async fn remove_peer(&mut self, ice_id: &str) {
        if let Some(peer) = self.peers.remove(ice_id) {
            peer.sender.close().await;
            let _ = tokio::time::timeout(Duration::from_millis(200), peer.forward_task).await;
            self.stats.active_peers = self.peers.len();
            self.stats.peer_ready = self.peers.values().any(|p| p.ready);
            self.stats.tool_channels_ready = self.peers.values().any(|p| p.tool_channels_ready);
            self.publish_stats();
        }
    }
}

pub(crate) async fn resolve_probe_target(base_url: &str) -> Option<SocketAddr> {
    let url = url::Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default().unwrap_or(80);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    let mut addrs = tokio::net::lookup_host((host, port)).await.ok()?;
    let mut first = None;
    for addr in &mut addrs {
        if first.is_none() {
            first = Some(addr);
        }
        if addr.is_ipv4() {
            return Some(addr);
        }
    }
    first
}
