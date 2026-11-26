//! WebRTC peer session driver combining `str0m`, [`PeerMachine`], and TURN/STUN ICE transport.

use std::fmt;
use std::future::Future;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::GzDecoder;
use rand::rngs::StdRng;
use remora_proto::machine::peer::{
    IceServerSpec, PeerError, PeerEvent, PeerMachine, PeerRole, PeerTransmit,
};
use remora_proto::machine::{Machine, Timestamp};
use remora_proto::pb;
use remora_proto::session::DataChannel;
use remora_proto::session::clipboard::extract_clipboard_message;
use remora_proto::session::control::{ControlMessage, encode_message};
use remora_proto::session::file::FileMessage;
use remora_proto::session::input::InputMessage;
use remora_proto::session::media::{MediaMessage, encode_forward_setting};
use remora_proto::session::system::SystemMessage;
use remora_proto::session::terminal::{TermFrame, TerminalControlMessage};
use remora_proto::session::tunnel::TunnelFrame;
use remora_proto::signal::SoacEvent;
use serde_json::Value;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate, CandidateKind, Event as Str0mEvent, Input as Str0mInput, Output as Str0mOutput, Rtc,
    RtcConfig,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::peer::channels::{
    ChannelError, ChannelOutbox, ChannelRegistry, DEFAULT_DATA_CHANNELS, EnqueueStatus,
    build_host_init_burst, build_send_video_track_request, format_protobuf_shape,
    inspect_protobuf_shape, media_data_to_audio_frame, media_data_to_video_frame,
    register_video_extensions, write_audio_frame, write_channel, write_video_frame,
};
use crate::peer::governor::{GovernorConfig, GovernorSample, VideoGovernor};
use crate::peer::turn::{TurnAllocation, TurnError, stun_binding_address};
use crate::platform::{EncodedAudioFrame, EncodedVideoFrame};

const CMD_CAP: usize = 256;
const EVENT_CAP: usize = 256;
const UDP_RECV_CAP: usize = 512;
const VIDEO_STATS_INTERVAL: Duration = Duration::from_secs(1);
const PATH_DIAG_INTERVAL: Duration = Duration::from_secs(5);
const MAX_COMPRESSED_SDP_B64: usize = 2 * 1024 * 1024;
const MAX_SDP_BYTES: usize = 1024 * 1024;
const MAX_CLIPBOARD_SHAPE_LOGS: usize = 24;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Error returned by [`PeerHandle`] or [`PeerSender`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerSessionError {
    Ice(TurnError),
    NoRelayAvailable,
    Sdp(String),
    Machine(PeerError),
    Channel(ChannelError),
    Closed,
}

impl fmt::Display for PeerSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ice(err) => err.fmt(f),
            Self::NoRelayAvailable => {
                f.write_str("force_relay requested but no TURN relay could be allocated")
            }
            Self::Sdp(msg) => write!(f, "SDP negotiation error: {msg}"),
            Self::Machine(err) => err.fmt(f),
            Self::Channel(err) => err.fmt(f),
            Self::Closed => f.write_str("peer session is closed"),
        }
    }
}

impl std::error::Error for PeerSessionError {}

impl From<TurnError> for PeerSessionError {
    fn from(err: TurnError) -> Self {
        Self::Ice(err)
    }
}

impl From<PeerError> for PeerSessionError {
    fn from(err: PeerError) -> Self {
        Self::Machine(err)
    }
}

impl From<ChannelError> for PeerSessionError {
    fn from(err: ChannelError) -> Self {
        Self::Channel(err)
    }
}

/// Outbound signaling messages emitted by a [`PeerHandle`] when using [`ChannelSignalingRelay`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PeerSignalingOut {
    Soac(Value),
    ForwardSetting(Value),
    IceFinished {
        ice_id: String,
        app_control_id: String,
    },
}

/// Abstraction for sending outbound signaling messages produced by [`PeerMachine`].
pub trait SignalingRelay: Send + Sync + 'static {
    fn send_soac(&self, payload: Value) -> BoxFuture<'_, Result<(), String>>;
    fn send_forward_setting(&self, payload: Value) -> BoxFuture<'_, Result<(), String>>;
    fn send_ice_finished<'a>(
        &'a self,
        ice_id: &'a str,
        app_control_id: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// [`SignalingRelay`] implementation backed by an `mpsc::UnboundedSender<PeerSignalingOut>`.
#[derive(Clone, Debug)]
pub struct ChannelSignalingRelay {
    tx: mpsc::UnboundedSender<PeerSignalingOut>,
}

impl ChannelSignalingRelay {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PeerSignalingOut>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

impl SignalingRelay for ChannelSignalingRelay {
    fn send_soac(&self, payload: Value) -> BoxFuture<'_, Result<(), String>> {
        let res = self
            .tx
            .send(PeerSignalingOut::Soac(payload))
            .map_err(|_| "signaling relay closed".to_owned());
        Box::pin(async move { res })
    }

    fn send_forward_setting(&self, payload: Value) -> BoxFuture<'_, Result<(), String>> {
        let res = self
            .tx
            .send(PeerSignalingOut::ForwardSetting(payload))
            .map_err(|_| "signaling relay closed".to_owned());
        Box::pin(async move { res })
    }

    fn send_ice_finished<'a>(
        &'a self,
        ice_id: &'a str,
        app_control_id: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        let res = self
            .tx
            .send(PeerSignalingOut::IceFinished {
                ice_id: ice_id.to_owned(),
                app_control_id: app_control_id.to_owned(),
            })
            .map_err(|_| "signaling relay closed".to_owned());
        Box::pin(async move { res })
    }
}

/// Configuration for spawning a WebRTC peer session.
#[derive(Clone)]
pub struct PeerSessionConfig {
    pub role: PeerRole,
    pub ice_id: String,
    pub remote_client_id: Option<String>,
    pub app_control_id: Option<String>,
    pub ice_servers: Vec<IceServerSpec>,
    pub force_relay: bool,
    pub prefer_relay: bool,
    pub controller_platform: Option<u8>,
    pub feature_flag: pb::FeatureFlag,
    pub enable_hevc: bool,
    pub enable_audio: bool,
    pub video_width: i32,
    pub video_height: i32,
    pub video_fps: i32,
    pub video_governor: Option<GovernorConfig>,
    pub clipboard_enabled: bool,
    pub local_probe_target: Option<SocketAddr>,
}

impl fmt::Debug for PeerSessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerSessionConfig")
            .field("role", &self.role)
            .field("ice_id", &self.ice_id)
            .field("force_relay", &self.force_relay)
            .field("prefer_relay", &self.prefer_relay)
            .field("ice_servers", &self.ice_servers)
            .field("enable_hevc", &self.enable_hevc)
            .field("enable_audio", &self.enable_audio)
            .finish()
    }
}

impl PeerSessionConfig {
    pub fn new(role: PeerRole, ice_id: impl Into<String>) -> Self {
        Self {
            role,
            ice_id: ice_id.into(),
            remote_client_id: None,
            app_control_id: None,
            ice_servers: Vec::new(),
            force_relay: false,
            prefer_relay: false,
            controller_platform: None,
            feature_flag: pb::FeatureFlag {
                ff_capture_setting: 4,
                ff_simple_action: 2,
                ff_system_metrics: 2,
                ff_clipboard: 3,
                ..Default::default()
            },
            enable_hevc: false,
            enable_audio: true,
            video_width: 1920,
            video_height: 1080,
            video_fps: 30,
            video_governor: None,
            clipboard_enabled: true,
            local_probe_target: None,
        }
    }
}

/// Live transport and media counters for a [`PeerHandle`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerStats {
    pub connected: bool,
    pub control_open: bool,
    pub tool_channels_ready: bool,
    pub peer_ready: bool,
    pub is_relayed: bool,
    pub turn_packets_tx: u64,
    pub turn_packets_rx: u64,
    pub host_packets_tx: u64,
    pub host_packets_rx: u64,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub video_frames_tx: u64,
    pub video_frames_rx: u64,
    pub audio_frames_tx: u64,
    pub audio_frames_rx: u64,
}

/// High-level events emitted by a [`PeerHandle`].
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum PeerSessionEvent {
    Connected {
        ice_id: String,
        is_relayed: bool,
    },
    ControlChannelReady {
        ice_id: String,
    },
    ToolChannelsReady {
        ice_id: String,
    },
    ToolChannelsClosed {
        ice_id: String,
    },
    PeerReady {
        ice_id: String,
    },
    Input(InputMessage),
    ClipboardText(String),
    VideoFrame(EncodedVideoFrame),
    AudioFrame(EncodedAudioFrame),
    VideoCapChanged {
        bitrate_kbps: u32,
        fps_cap: Option<u32>,
    },
    Control(Box<ControlMessage>),
    File(Box<FileMessage>),
    Tunnel(TunnelFrame),
    TermFrame(TermFrame),
    TerminalControl(TerminalControlMessage),
    System(Box<SystemMessage>),
    KeyframeRequested,
    EchoTimedOut {
        ice_id: String,
        request_seq: i64,
    },
    RawChannelData {
        channel: DataChannel,
        data: Vec<u8>,
    },
    Closed {
        ice_id: String,
    },
}

impl fmt::Debug for PeerSessionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected { ice_id, is_relayed } => f
                .debug_struct("Connected")
                .field("ice_id", ice_id)
                .field("is_relayed", is_relayed)
                .finish(),
            Self::ControlChannelReady { ice_id } => f
                .debug_struct("ControlChannelReady")
                .field("ice_id", ice_id)
                .finish(),
            Self::ToolChannelsReady { ice_id } => f
                .debug_struct("ToolChannelsReady")
                .field("ice_id", ice_id)
                .finish(),
            Self::ToolChannelsClosed { ice_id } => f
                .debug_struct("ToolChannelsClosed")
                .field("ice_id", ice_id)
                .finish(),
            Self::PeerReady { ice_id } => {
                f.debug_struct("PeerReady").field("ice_id", ice_id).finish()
            }
            Self::Input(input) => f.debug_tuple("Input").field(input).finish(),
            Self::ClipboardText(text) => f
                .debug_struct("ClipboardText")
                .field("len", &text.len())
                .finish(),
            Self::VideoFrame(frame) => f
                .debug_struct("VideoFrame")
                .field("codec", &frame.codec)
                .field("is_keyframe", &frame.is_keyframe)
                .field("pts_ms", &frame.pts_ms)
                .field("len", &frame.data.len())
                .finish(),
            Self::AudioFrame(frame) => f
                .debug_struct("AudioFrame")
                .field("pts_ms", &frame.pts_ms)
                .field("len", &frame.data.len())
                .finish(),
            Self::VideoCapChanged {
                bitrate_kbps,
                fps_cap,
            } => f
                .debug_struct("VideoCapChanged")
                .field("bitrate_kbps", bitrate_kbps)
                .field("fps_cap", fps_cap)
                .finish(),
            Self::Control(msg) => f.debug_tuple("Control").field(msg).finish(),
            Self::File(msg) => f.debug_tuple("File").field(msg).finish(),
            Self::Tunnel(frame) => f.debug_tuple("Tunnel").field(frame).finish(),
            Self::TermFrame(frame) => f.debug_tuple("TermFrame").field(frame).finish(),
            Self::TerminalControl(msg) => f.debug_tuple("TerminalControl").field(msg).finish(),
            Self::System(msg) => f.debug_tuple("System").field(msg).finish(),
            Self::KeyframeRequested => f.write_str("KeyframeRequested"),
            Self::EchoTimedOut {
                ice_id,
                request_seq,
            } => f
                .debug_struct("EchoTimedOut")
                .field("ice_id", ice_id)
                .field("request_seq", request_seq)
                .finish(),
            Self::RawChannelData { channel, data } => f
                .debug_struct("RawChannelData")
                .field("channel", channel)
                .field("len", &data.len())
                .finish(),
            Self::Closed { ice_id } => f.debug_struct("Closed").field("ice_id", ice_id).finish(),
        }
    }
}

enum PeerCommand {
    Soac(SoacEvent),
    SendVideoFrame(EncodedVideoFrame),
    SendAudioFrame(EncodedAudioFrame),
    SendInput(InputMessage),
    SendClipboard(String),
    SendControl(ControlMessage),
    SendChannelRaw {
        channel: DataChannel,
        data: Vec<u8>,
    },
    SendFileRequest {
        channel: DataChannel,
        request_id: i64,
        which: pb::file_transfer_ftp_request::Which,
    },
    SendFileResponse {
        request_id: i64,
        which: pb::file_transfer_ftp_response::Which,
    },
    SendTunnelFrame(TunnelFrame),
    SendTermFrame(TermFrame),
    SendTerminalSessionChanged,
    SendPrivacyResponse {
        request_id: i64,
        response: pb::PrivateScreenSettingResponse,
    },
    SendDrawResponse {
        request_id: i64,
        response: pb::DrawResponse,
    },
    SendVirtualAudioPolicyResponse {
        channel: DataChannel,
        request_id: i64,
        error_code: i32,
    },
    RequestKeyframe,
    DrainAndClose(Instant),
    Close,
}

/// Clonable command handle for driving a running WebRTC peer session.
#[derive(Clone)]
pub struct PeerSender {
    ice_id: String,
    cmd_tx: mpsc::Sender<PeerCommand>,
    stats_rx: watch::Receiver<PeerStats>,
}

impl fmt::Debug for PeerSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerSender")
            .field("ice_id", &self.ice_id)
            .finish()
    }
}

impl PeerSender {
    pub fn ice_id(&self) -> &str {
        &self.ice_id
    }

    pub fn stats(&self) -> PeerStats {
        self.stats_rx.borrow().clone()
    }

    pub async fn feed_soac(&self, soac: SoacEvent) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::Soac(soac))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn send_video_frame(&self, frame: EncodedVideoFrame) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendVideoFrame(frame))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn send_audio_frame(&self, frame: EncodedAudioFrame) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendAudioFrame(frame))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn send_input(&self, input: InputMessage) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendInput(input))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn send_clipboard(&self, text: impl Into<String>) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendClipboard(text.into()))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn send_control(&self, msg: ControlMessage) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendControl(msg))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues a file RPC request on the selected channel.
    pub async fn send_file_request(
        &self,
        channel: DataChannel,
        request_id: i64,
        which: pb::file_transfer_ftp_request::Which,
    ) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendFileRequest {
                channel,
                request_id,
                which,
            })
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues a file RPC reply on the file channel.
    pub async fn send_file_response(
        &self,
        request_id: i64,
        which: pb::file_transfer_ftp_response::Which,
    ) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendFileResponse { request_id, which })
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues a port-mapping frame on the file channel.
    pub async fn send_tunnel_frame(&self, frame: TunnelFrame) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendTunnelFrame(frame))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues a TERM frame on the binary channel.
    pub async fn send_term_frame(&self, frame: TermFrame) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendTermFrame(frame))
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues a terminal session-change notification on the binary channel.
    pub async fn send_terminal_session_changed(&self) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendTerminalSessionChanged)
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues a privacy-screen reply on the text channel.
    pub async fn send_privacy_response(
        &self,
        request_id: i64,
        response: pb::PrivateScreenSettingResponse,
    ) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendPrivacyResponse {
                request_id,
                response,
            })
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues an annotation reply on the text channel.
    pub async fn send_draw_response(
        &self,
        request_id: i64,
        response: pb::DrawResponse,
    ) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendDrawResponse {
                request_id,
                response,
            })
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    /// Queues a virtual audio policy reply on the selected channel.
    pub async fn send_virtual_audio_policy_response(
        &self,
        channel: DataChannel,
        request_id: i64,
        error_code: i32,
    ) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendVirtualAudioPolicyResponse {
                channel,
                request_id,
                error_code,
            })
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn send_channel_raw(
        &self,
        channel: DataChannel,
        data: Vec<u8>,
    ) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::SendChannelRaw { channel, data })
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn request_keyframe(&self) -> Result<(), PeerSessionError> {
        self.cmd_tx
            .send(PeerCommand::RequestKeyframe)
            .await
            .map_err(|_| PeerSessionError::Closed)
    }

    pub async fn close(&self) {
        let _ = self.cmd_tx.send(PeerCommand::Close).await;
    }

    pub(crate) async fn drain_and_close(&self, grace: Duration) {
        let _ = self
            .cmd_tx
            .send(PeerCommand::DrainAndClose(Instant::now() + grace))
            .await;
    }
}

/// Stream of inbound [`PeerSessionEvent`]s from a [`PeerHandle`].
pub struct PeerEventReceiver {
    rx: mpsc::Receiver<PeerSessionEvent>,
}

impl fmt::Debug for PeerEventReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerEventReceiver").finish()
    }
}

impl PeerEventReceiver {
    pub async fn recv(&mut self) -> Option<PeerSessionEvent> {
        self.rx.recv().await
    }
}

/// Running WebRTC peer session built on `str0m` + [`PeerMachine`].
#[derive(Debug)]
pub struct PeerHandle {
    sender: PeerSender,
    events: PeerEventReceiver,
}

impl PeerHandle {
    /// Spawns a new WebRTC peer session using `config` and `signaling`.
    pub async fn spawn<R: SignalingRelay>(
        config: PeerSessionConfig,
        signaling: Arc<R>,
    ) -> Result<Self, PeerSessionError> {
        let turn_alloc = match TurnAllocation::allocate_first(&config.ice_servers).await {
            Ok(alloc) => alloc,
            Err(err) if !config.force_relay => {
                warn!(
                    ice_id = %config.ice_id,
                    prefer_relay = config.prefer_relay,
                    error = %err,
                    "TURN allocation failed; falling back to direct ICE"
                );
                None
            }
            Err(err) => return Err(err.into()),
        };
        let effective_force_relay = if turn_alloc.is_none() {
            if config.force_relay {
                return Err(PeerSessionError::NoRelayAvailable);
            }
            if config.prefer_relay {
                warn!(ice_id = %config.ice_id, "prefer_relay requested but no TURN relay available; falling back to direct ICE");
            }
            false
        } else {
            config.force_relay || config.prefer_relay
        };

        let start_instant = Instant::now();
        let mut rtc_cfg =
            register_video_extensions(RtcConfig::new().set_sctp_max_message_size(262_144));
        let video_governor = if config.role == PeerRole::Host {
            config.video_governor.clone().map(|governor_config| {
                let mut governor = VideoGovernor::new(governor_config);
                if let Ok(nominal_fps) = u32::try_from(config.video_fps)
                    && nominal_fps > 0
                {
                    governor.set_requested_limits(None, nominal_fps);
                }
                governor
            })
        } else {
            None
        };
        if video_governor.is_some() {
            rtc_cfg = rtc_cfg.set_stats_interval(Some(VIDEO_STATS_INTERVAL));
        }
        {
            let codecs = rtc_cfg.codec_config();
            codecs.clear();
            codecs.enable_opus(config.enable_audio, false);
            codecs.enable_h264(true);
            codecs.enable_h265(config.enable_hevc);
        }
        let mut rtc = rtc_cfg.build(start_instant);

        let mut host_sockets: Vec<(SocketAddr, Arc<UdpSocket>)> = Vec::new();
        let mut local_candidates: Vec<Candidate> = Vec::new();

        if !effective_force_relay {
            let mut bound_ips = Vec::new();
            if let Some(lan_ip) = detect_local_ip(config.local_probe_target).await
                && !lan_ip.is_loopback()
            {
                let sock = UdpSocket::bind(SocketAddr::new(lan_ip, 0))
                    .await
                    .map_err(|e| TurnError::Io(e.to_string()))?;
                let local_addr = sock
                    .local_addr()
                    .map_err(|e| TurnError::Io(e.to_string()))?;
                let sock = Arc::new(sock);
                if let Ok(cand) = Candidate::host(local_addr, "udp")
                    && let Some(added) = rtc.add_local_candidate(cand)
                {
                    local_candidates.push(added.clone());
                }
                // Try STUN discovery on LAN socket if stun: servers are present.
                for server in &config.ice_servers {
                    for url in &server.urls {
                        if url.starts_with("stun:")
                            && let Ok(Some(srflx)) = stun_binding_address(&sock, url).await
                            && let Ok(cand) = Candidate::server_reflexive(srflx, local_addr, "udp")
                            && let Some(added) = rtc.add_local_candidate(cand)
                        {
                            local_candidates.push(added.clone());
                        }
                    }
                }
                bound_ips.push(lan_ip);
                host_sockets.push((local_addr, sock));
            }

            let lo_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
            if !bound_ips.contains(&lo_ip) {
                let sock = UdpSocket::bind(SocketAddr::new(lo_ip, 0))
                    .await
                    .map_err(|e| TurnError::Io(e.to_string()))?;
                let local_addr = sock
                    .local_addr()
                    .map_err(|e| TurnError::Io(e.to_string()))?;
                let sock = Arc::new(sock);
                if let Ok(cand) = Candidate::host(local_addr, "udp")
                    && let Some(added) = rtc.add_local_candidate(cand)
                {
                    local_candidates.push(added.clone());
                }
                host_sockets.push((local_addr, sock));
            }
        }

        if let Some(alloc) = &turn_alloc
            && let Ok(cand) = Candidate::relayed(alloc.relayed_addr(), alloc.local_addr(), "udp")
            && let Some(added) = rtc.add_local_candidate(cand)
        {
            local_candidates.push(added.clone());
        }

        let mut machine =
            PeerMachine::new(rand::make_rng::<StdRng>()).with_feature_flag(config.feature_flag);
        machine.start_session(
            Timestamp::ZERO,
            config.role,
            config.ice_id.clone(),
            config.remote_client_id.clone(),
            config.app_control_id.clone(),
            effective_force_relay,
        );
        machine.set_controller_platform(config.controller_platform);

        let mut pending_offer: Option<SdpPendingOffer> = None;
        let mut initial_mid: Option<String> = None;

        if config.role == PeerRole::Viewer {
            let mut change = rtc.sdp_api();
            let _video_mid =
                change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
            if config.enable_audio {
                let _audio_mid =
                    change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
            }
            for label in DEFAULT_DATA_CHANNELS {
                let _ = change.add_channel(label.to_owned());
            }
            let (offer, pending) = change
                .apply()
                .ok_or_else(|| PeerSessionError::Sdp("viewer SDP apply yielded no offer".into()))?;
            let sdp = offer.to_sdp_string();
            initial_mid = first_sdp_mid(&sdp);
            pending_offer = Some(pending);
            machine.on_local_offer(sdp)?;
            for cand in &local_candidates {
                machine.on_local_candidate(
                    cand.to_sdp_string(),
                    initial_mid.clone(),
                    initial_mid.as_ref().map(|_| 0),
                )?;
            }
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAP);
        let (ev_tx, ev_rx) = mpsc::channel(EVENT_CAP);
        let (stats_tx, stats_rx) = watch::channel(PeerStats::default());

        let (udp_tx, udp_rx) = mpsc::channel(UDP_RECV_CAP);
        let mut udp_tasks = Vec::with_capacity(host_sockets.len());
        for (local_addr, sock) in &host_sockets {
            let local_addr = *local_addr;
            let sock = Arc::clone(sock);
            let tx = udp_tx.clone();
            udp_tasks.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                while let Ok((n, src)) = sock.recv_from(&mut buf).await {
                    if tx.send((local_addr, src, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }));
        }
        drop(udp_tx);

        let ice_id = config.ice_id.clone();
        let state = PeerLoopState {
            config,
            effective_force_relay,
            video_governor,
            start_instant,
            rtc,
            machine,
            registry: ChannelRegistry::new(),
            outbox: ChannelOutbox::new(),
            host_sockets,
            udp_tasks,
            udp_rx,
            turn_alloc,
            signaling,
            pending_offer,
            initial_mid,
            stats: PeerStats::default(),
            stats_tx,
            cmd_rx,
            ev_tx,
            local_candidates,
            last_tx_was_relay: false,
            last_rx_was_relay: false,
            path_monitor: PathRateMonitor::default(),
            next_path_diag: None,
            clipboard_shape_logs: 0,
            closing_deadline: None,
        };

        tokio::spawn(state.run());

        Ok(Self {
            sender: PeerSender {
                ice_id,
                cmd_tx,
                stats_rx,
            },
            events: PeerEventReceiver { rx: ev_rx },
        })
    }

    pub fn ice_id(&self) -> &str {
        self.sender.ice_id()
    }

    pub fn sender(&self) -> PeerSender {
        self.sender.clone()
    }

    pub fn split(self) -> (PeerSender, PeerEventReceiver) {
        (self.sender, self.events)
    }

    pub fn stats(&self) -> PeerStats {
        self.sender.stats()
    }

    pub async fn feed_soac(&self, soac: SoacEvent) -> Result<(), PeerSessionError> {
        self.sender.feed_soac(soac).await
    }

    pub async fn send_video_frame(&self, frame: EncodedVideoFrame) -> Result<(), PeerSessionError> {
        self.sender.send_video_frame(frame).await
    }

    pub async fn send_audio_frame(&self, frame: EncodedAudioFrame) -> Result<(), PeerSessionError> {
        self.sender.send_audio_frame(frame).await
    }

    pub async fn send_input(&self, input: InputMessage) -> Result<(), PeerSessionError> {
        self.sender.send_input(input).await
    }

    pub async fn send_clipboard(&self, text: impl Into<String>) -> Result<(), PeerSessionError> {
        self.sender.send_clipboard(text).await
    }

    pub async fn send_control(&self, msg: ControlMessage) -> Result<(), PeerSessionError> {
        self.sender.send_control(msg).await
    }

    /// Queues a file RPC request on the selected channel.
    pub async fn send_file_request(
        &self,
        channel: DataChannel,
        request_id: i64,
        which: pb::file_transfer_ftp_request::Which,
    ) -> Result<(), PeerSessionError> {
        self.sender
            .send_file_request(channel, request_id, which)
            .await
    }

    /// Queues a file RPC reply on the file channel.
    pub async fn send_file_response(
        &self,
        request_id: i64,
        which: pb::file_transfer_ftp_response::Which,
    ) -> Result<(), PeerSessionError> {
        self.sender.send_file_response(request_id, which).await
    }

    /// Queues a port-mapping frame on the file channel.
    pub async fn send_tunnel_frame(&self, frame: TunnelFrame) -> Result<(), PeerSessionError> {
        self.sender.send_tunnel_frame(frame).await
    }

    /// Queues a TERM frame on the binary channel.
    pub async fn send_term_frame(&self, frame: TermFrame) -> Result<(), PeerSessionError> {
        self.sender.send_term_frame(frame).await
    }

    /// Queues a terminal session-change notification on the binary channel.
    pub async fn send_terminal_session_changed(&self) -> Result<(), PeerSessionError> {
        self.sender.send_terminal_session_changed().await
    }

    /// Queues a privacy-screen reply on the text channel.
    pub async fn send_privacy_response(
        &self,
        request_id: i64,
        response: pb::PrivateScreenSettingResponse,
    ) -> Result<(), PeerSessionError> {
        self.sender
            .send_privacy_response(request_id, response)
            .await
    }

    /// Queues an annotation reply on the text channel.
    pub async fn send_draw_response(
        &self,
        request_id: i64,
        response: pb::DrawResponse,
    ) -> Result<(), PeerSessionError> {
        self.sender.send_draw_response(request_id, response).await
    }

    /// Queues a virtual audio policy reply on the selected channel.
    pub async fn send_virtual_audio_policy_response(
        &self,
        channel: DataChannel,
        request_id: i64,
        error_code: i32,
    ) -> Result<(), PeerSessionError> {
        self.sender
            .send_virtual_audio_policy_response(channel, request_id, error_code)
            .await
    }

    pub async fn request_keyframe(&self) -> Result<(), PeerSessionError> {
        self.sender.request_keyframe().await
    }

    pub async fn next_event(&mut self) -> Option<PeerSessionEvent> {
        self.events.recv().await
    }

    pub async fn close(&self) {
        self.sender.close().await;
    }
}

#[derive(Debug, Default)]
struct PathRateMonitor {
    previous: Option<(Instant, u64, u64)>,
    last_route: Option<&'static str>,
    ticks: u64,
}

impl PathRateMonitor {
    fn sample(
        &mut self,
        now: Instant,
        route: &'static str,
        tx_bytes: u64,
        rx_bytes: u64,
    ) -> Option<(u64, u64, bool)> {
        let rates = match self.previous.replace((now, tx_bytes, rx_bytes)) {
            Some((prev_at, prev_tx, prev_rx)) => {
                let secs = now.saturating_duration_since(prev_at).as_secs_f64();
                if secs > 0.0 && tx_bytes >= prev_tx && rx_bytes >= prev_rx {
                    Some((
                        ((tx_bytes - prev_tx) as f64 * 8.0 / secs) as u64,
                        ((rx_bytes - prev_rx) as f64 * 8.0 / secs) as u64,
                    ))
                } else {
                    None
                }
            }
            None => None,
        };
        self.ticks = self.ticks.saturating_add(1);
        let route_changed = self.last_route != Some(route);
        if route_changed || self.ticks.is_multiple_of(6) {
            self.last_route = Some(route);
            let (tx_bps, rx_bps) = rates.unwrap_or((0, 0));
            return Some((tx_bps, rx_bps, route_changed));
        }
        None
    }

    fn reset(&mut self) {
        self.previous = None;
        self.last_route = None;
        self.ticks = 0;
    }
}

struct PeerLoopState<R: SignalingRelay> {
    config: PeerSessionConfig,
    effective_force_relay: bool,
    video_governor: Option<VideoGovernor>,
    start_instant: Instant,
    rtc: Rtc,
    machine: PeerMachine<StdRng>,
    registry: ChannelRegistry,
    outbox: ChannelOutbox,
    host_sockets: Vec<(SocketAddr, Arc<UdpSocket>)>,
    udp_tasks: Vec<tokio::task::JoinHandle<()>>,
    udp_rx: mpsc::Receiver<(SocketAddr, SocketAddr, Vec<u8>)>,
    turn_alloc: Option<TurnAllocation>,
    signaling: Arc<R>,
    pending_offer: Option<SdpPendingOffer>,
    initial_mid: Option<String>,
    stats: PeerStats,
    stats_tx: watch::Sender<PeerStats>,
    cmd_rx: mpsc::Receiver<PeerCommand>,
    ev_tx: mpsc::Sender<PeerSessionEvent>,
    local_candidates: Vec<Candidate>,
    last_tx_was_relay: bool,
    last_rx_was_relay: bool,
    path_monitor: PathRateMonitor,
    next_path_diag: Option<Instant>,
    clipboard_shape_logs: usize,
    closing_deadline: Option<Instant>,
}

impl<R: SignalingRelay> PeerLoopState<R> {
    fn machine_now(&self) -> Timestamp {
        Timestamp::from_duration(self.start_instant.elapsed())
    }

    fn queue_channel_command(
        &mut self,
        queue: impl FnOnce(&mut PeerMachine<StdRng>, Timestamp) -> Result<(), PeerError>,
    ) {
        let now = self.machine_now();
        if let Err(err) = queue(&mut self.machine, now) {
            warn!(ice_id = %self.config.ice_id, error = %err, "queue channel command failed");
        }
    }

    fn publish_stats(&self) {
        let _ = self.stats_tx.send(self.stats.clone());
    }

    fn send_or_queue(&mut self, channel: DataChannel, data: Vec<u8>, is_metrics: bool) -> bool {
        if !self.outbox.has_pending(&channel)
            && write_channel(&mut self.rtc, &self.registry, &channel, &data).is_ok()
        {
            return true;
        }
        if is_metrics {
            self.outbox.set_metrics(channel, data);
        } else if self.outbox.enqueue_reply(channel.clone(), data)
            == EnqueueStatus::DroppedFirstOverflow
        {
            warn!(
                ice_id = %self.config.ice_id,
                channel = channel.as_str(),
                "channel outbox full; dropping excess replies"
            );
        }
        false
    }

    async fn emit(&self, event: PeerSessionEvent) {
        let _ = self.ev_tx.send(event).await;
    }

    async fn run(mut self) {
        let mut turn_buf = vec![0u8; 4096];

        loop {
            // Drain str0m and PeerMachine until both are quiescent.
            let rtc_timeout = match self.drain_until_quiescent().await {
                Ok(Some(t)) => t,
                Ok(None) => break,
                Err(err) => {
                    warn!(ice_id = %self.config.ice_id, error = %err, "peer loop fatal error");
                    break;
                }
            };

            let machine_deadline = self
                .machine
                .poll_timeout()
                .map(|t| self.start_instant + t.as_duration());
            let turn_deadline = self.turn_alloc.as_ref().map(TurnAllocation::next_refresh);

            let mut earliest = rtc_timeout;
            if let Some(m) = machine_deadline
                && m < earliest
            {
                earliest = m;
            }
            if let Some(t) = turn_deadline
                && t < earliest
            {
                earliest = t;
            }
            if let Some(c) = self.closing_deadline
                && c < earliest
            {
                earliest = c;
            }
            if let Some(p) = self.next_path_diag
                && p < earliest
            {
                earliest = p;
            }
            let sleep_until = tokio::time::Instant::from_std(earliest);

            tokio::select! {
                _ = tokio::time::sleep_until(sleep_until) => {
                    let now = Instant::now();
                    if self.closing_deadline.is_some_and(|d| now >= d) {
                        self.machine.release();
                        let _ = self.drain_machine().await;
                        break;
                    }
                    if now >= rtc_timeout {
                        let _ = self.rtc.handle_input(Str0mInput::Timeout(now));
                    }
                    let mnow = self.machine_now();
                    if self.machine.poll_timeout().is_some_and(|d| mnow >= d) {
                        self.machine.handle_timeout(mnow);
                    }
                    if let Some(alloc) = &mut self.turn_alloc {
                        let _ = alloc.maybe_refresh(now).await;
                    }
                    if self.stats.connected && self.next_path_diag.is_some_and(|d| now >= d) {
                        let route = if self.effective_force_relay
                            || self.last_tx_was_relay
                            || self.last_rx_was_relay
                        {
                            "relay"
                        } else {
                            "direct"
                        };
                        if let Some((tx_bps, rx_bps, route_changed)) = self.path_monitor.sample(
                            now,
                            route,
                            self.stats.tx_bytes,
                            self.stats.rx_bytes,
                        ) {
                            info!(
                                ice_id = %self.config.ice_id,
                                route,
                                route_changed,
                                tx_bps,
                                rx_bps,
                                "ICE path throughput"
                            );
                        }
                        self.next_path_diag = Some(now + PATH_DIAG_INTERVAL);
                    }
                }
                pkt = self.udp_rx.recv(), if !self.host_sockets.is_empty() => {
                    let Some((local_addr, src_addr, data)) = pkt else {
                        break;
                    };
                    if let Ok(contents) = data.as_slice().try_into() {
                        self.stats.host_packets_rx += 1;
                        self.stats.rx_bytes = self.stats.rx_bytes.saturating_add(data.len() as u64);
                        self.last_rx_was_relay = false;
                        let now = Instant::now();
                        let _ = self.rtc.handle_input(Str0mInput::Receive(
                            now,
                            Receive {
                                proto: Protocol::Udp,
                                source: src_addr,
                                destination: local_addr,
                                contents,
                            },
                        ));
                    }
                }
                turn_res = async {
                    match &self.turn_alloc {
                        Some(alloc) => alloc.socket().recv(&mut turn_buf).await,
                        None => std::future::pending().await,
                    }
                } => {
                    match turn_res {
                        Ok(n) => {
                            if let Some(alloc) = &mut self.turn_alloc {
                                let relayed_addr = alloc.relayed_addr();
                                match alloc.handle_socket_packet(&turn_buf[..n]).await {
                                    Ok(Some((peer_addr, payload))) => {
                                        if let Ok(contents) = payload.as_slice().try_into() {
                                            self.stats.turn_packets_rx += 1;
                                            self.stats.rx_bytes = self
                                                .stats
                                                .rx_bytes
                                                .saturating_add(payload.len() as u64);
                                            self.last_rx_was_relay = true;
                                            let now = Instant::now();
                                            let _ = self.rtc.handle_input(Str0mInput::Receive(
                                                now,
                                                Receive {
                                                    proto: Protocol::Udp,
                                                    source: peer_addr,
                                                    destination: relayed_addr,
                                                    contents,
                                                },
                                            ));
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        warn!(ice_id = %self.config.ice_id, error = %err, "TURN packet handling failed");
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            warn!(ice_id = %self.config.ice_id, error = %err, "TURN socket receive error");
                        }
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(PeerCommand::Soac(soac)) => {
                            let normalized = normalize_soac_sdp(soac);
                            let mnow = self.machine_now();
                            if let Err(err) = self.machine.handle_soac(mnow, &normalized) {
                                warn!(ice_id = %self.config.ice_id, error = %err, "soac handling error");
                            }
                        }
                        Some(PeerCommand::SendVideoFrame(frame)) => {
                            if let Err(err) = write_video_frame(
                                &mut self.rtc,
                                &self.registry,
                                self.start_instant,
                                &frame,
                            ) {
                                debug!(ice_id = %self.config.ice_id, error = %err, "video frame write skipped");
                            } else {
                                self.stats.video_frames_tx += 1;
                                self.publish_stats();
                            }
                        }
                        Some(PeerCommand::SendAudioFrame(frame)) => {
                            if let Err(err) = write_audio_frame(
                                &mut self.rtc,
                                &self.registry,
                                self.start_instant,
                                &frame,
                            ) {
                                debug!(ice_id = %self.config.ice_id, error = %err, "audio frame write skipped");
                            } else {
                                self.stats.audio_frames_tx += 1;
                                self.publish_stats();
                            }
                        }
                        Some(PeerCommand::SendInput(input)) => {
                            if let Err(err) = self.machine.queue_input(&input) {
                                warn!(ice_id = %self.config.ice_id, error = %err, "queue input failed");
                            }
                        }
                        Some(PeerCommand::SendClipboard(text)) => {
                            let mnow = self.machine_now();
                            let ch = if self.registry.channel_id(&DataChannel::Text).is_some() {
                                DataChannel::Text
                            } else {
                                DataChannel::Control
                            };
                            if let Err(err) = self.machine.queue_clipboard_text(mnow, ch, &text) {
                                warn!(ice_id = %self.config.ice_id, error = %err, "queue clipboard failed");
                            }
                        }
                        Some(PeerCommand::SendControl(msg)) => {
                            let is_metrics = matches!(msg, ControlMessage::SystemMetrics { .. });
                            let data = encode_message(&msg);
                            self.send_or_queue(DataChannel::Control, data, is_metrics);
                        }
                        Some(PeerCommand::SendChannelRaw { channel, data }) => {
                            self.send_or_queue(channel, data, false);
                        }
                        Some(PeerCommand::SendFileRequest { channel, request_id, which }) => {
                            self.queue_channel_command(|machine, now| {
                                machine.queue_file_request(now, channel, request_id, which)
                            });
                        }
                        Some(PeerCommand::SendFileResponse { request_id, which }) => {
                            self.queue_channel_command(|machine, now| {
                                machine.queue_file_response(now, request_id, which)
                            });
                        }
                        Some(PeerCommand::SendTunnelFrame(frame)) => {
                            self.queue_channel_command(|machine, _| {
                                machine.queue_tunnel_frame(&frame)
                            });
                        }
                        Some(PeerCommand::SendTermFrame(frame)) => {
                            self.queue_channel_command(|machine, _| {
                                machine.queue_term_frame(&frame)
                            });
                        }
                        Some(PeerCommand::SendTerminalSessionChanged) => {
                            self.queue_channel_command(|machine, now| {
                                machine.queue_terminal_session_changed(now)
                            });
                        }
                        Some(PeerCommand::SendPrivacyResponse { request_id, response }) => {
                            self.queue_channel_command(|machine, now| {
                                machine.queue_privacy_response(now, request_id, response)
                            });
                        }
                        Some(PeerCommand::SendDrawResponse { request_id, response }) => {
                            self.queue_channel_command(|machine, now| {
                                machine.queue_draw_response(now, request_id, response)
                            });
                        }
                        Some(PeerCommand::SendVirtualAudioPolicyResponse { channel, request_id, error_code }) => {
                            self.queue_channel_command(|machine, now| {
                                machine.queue_virtual_audio_policy_response(now, channel, request_id, error_code)
                            });
                        }
                        Some(PeerCommand::RequestKeyframe) => {
                            if let Some(mid) = self.registry.video_rx_mid()
                                && let Some(mut writer) = self.rtc.writer(mid)
                            {
                                let _ = writer.request_keyframe(
                                    None,
                                    str0m::media::KeyframeRequestKind::Pli,
                                );
                            }
                        }
                        Some(PeerCommand::DrainAndClose(deadline)) => {
                            if self.closing_deadline.is_none() {
                                self.closing_deadline = Some(deadline);
                            }
                        }
                        Some(PeerCommand::Close) | None => {
                            self.machine.release();
                            let _ = self.drain_machine().await;
                            break;
                        }
                    }
                }
            }
        }

        for task in self.udp_tasks.drain(..) {
            task.abort();
        }
        self.emit(PeerSessionEvent::Closed {
            ice_id: self.config.ice_id.clone(),
        })
        .await;
    }

    async fn drain_until_quiescent(&mut self) -> Result<Option<Instant>, PeerSessionError> {
        loop {
            let timeout = self.drain_rtc().await?;
            let mut mutated_rtc = self.drain_machine().await?;
            if self.outbox.flush_open(&mut self.rtc, &self.registry) > 0 {
                mutated_rtc = true;
            }
            let Some(timeout) = timeout else {
                return Ok(None);
            };
            if !mutated_rtc {
                return Ok(Some(timeout));
            }
        }
    }

    async fn drain_rtc(&mut self) -> Result<Option<Instant>, PeerSessionError> {
        loop {
            match self
                .rtc
                .poll_output()
                .map_err(|e| PeerSessionError::Sdp(e.to_string()))?
            {
                Str0mOutput::Timeout(t) => return Ok(Some(t)),
                Str0mOutput::Transmit(tx) => {
                    if let Some(alloc) = &mut self.turn_alloc
                        && alloc.relayed_addr() == tx.source
                    {
                        self.stats.turn_packets_tx += 1;
                        self.stats.tx_bytes =
                            self.stats.tx_bytes.saturating_add(tx.contents.len() as u64);
                        self.last_tx_was_relay = true;
                        if let Err(err) = alloc.send_to_peer(tx.destination, &tx.contents).await {
                            debug!(ice_id = %self.config.ice_id, error = %err, "TURN send failed");
                        }
                    } else if let Some((_, sock)) = self
                        .host_sockets
                        .iter()
                        .find(|(addr, _)| *addr == tx.source)
                        .or_else(|| self.host_sockets.first())
                    {
                        self.stats.host_packets_tx += 1;
                        self.stats.tx_bytes =
                            self.stats.tx_bytes.saturating_add(tx.contents.len() as u64);
                        self.last_tx_was_relay = false;
                        if let Err(err) = sock.send_to(&tx.contents, tx.destination).await {
                            debug!(ice_id = %self.config.ice_id, error = %err, "UDP send failed");
                        }
                    }
                }
                Str0mOutput::Event(ev) => match ev {
                    Str0mEvent::Connected => {
                        let is_relayed = self.effective_force_relay
                            || (self.last_tx_was_relay && self.last_rx_was_relay)
                            || (self.stats.turn_packets_tx > 0 && self.stats.host_packets_tx == 0);
                        self.stats.connected = true;
                        self.stats.is_relayed = is_relayed;
                        self.next_path_diag = Some(Instant::now() + PATH_DIAG_INTERVAL);
                        self.publish_stats();
                        let mnow = self.machine_now();
                        self.machine.on_ice_connected(mnow)?;
                        info!(ice_id = %self.config.ice_id, is_relayed, "peer ICE/DTLS connected");
                        self.emit(PeerSessionEvent::Connected {
                            ice_id: self.config.ice_id.clone(),
                            is_relayed,
                        })
                        .await;
                    }
                    Str0mEvent::IceConnectionStateChange(state) => {
                        debug!(ice_id = %self.config.ice_id, ?state, "ICE state changed");
                        if state == str0m::IceConnectionState::Disconnected {
                            self.path_monitor.reset();
                        }
                    }
                    Str0mEvent::MediaAdded(added) => {
                        if added.kind == MediaKind::Video {
                            if added.direction.is_sending() {
                                self.registry.set_video_tx_mid(added.mid);
                            } else {
                                self.registry.set_video_rx_mid(added.mid);
                            }
                        } else if added.kind == MediaKind::Audio && added.direction.is_sending() {
                            self.registry.set_audio_tx_mid(added.mid);
                        }
                    }
                    Str0mEvent::ChannelOpen(id, label) => {
                        let ch = self.registry.register_channel(id, &label);
                        let mut stats_dirty = false;
                        if ch == DataChannel::Control {
                            self.stats.control_open = true;
                            stats_dirty = true;
                        }
                        let mut emit_tool_ready = false;
                        if !self.stats.tool_channels_ready && self.registry.has_tool_channels() {
                            self.stats.tool_channels_ready = true;
                            stats_dirty = true;
                            emit_tool_ready = true;
                        }
                        if stats_dirty {
                            self.publish_stats();
                        }
                        if emit_tool_ready {
                            self.emit(PeerSessionEvent::ToolChannelsReady {
                                ice_id: self.config.ice_id.clone(),
                            })
                            .await;
                        }
                        let mnow = self.machine_now();
                        self.machine.on_channel_open(mnow, ch.clone())?;
                        if self.config.role == PeerRole::Viewer && ch == DataChannel::Control {
                            let seq = self.machine.alloc_seq();
                            let ts = i64::try_from(mnow.as_millis()).unwrap_or(i64::MAX);
                            let req = build_send_video_track_request(seq, ts);
                            let _ = write_channel(
                                &mut self.rtc,
                                &self.registry,
                                &DataChannel::Control,
                                &req,
                            );
                        }
                    }
                    Str0mEvent::ChannelData(cd) => {
                        if let Some(ch) = self.registry.lookup(cd.id).cloned() {
                            let mnow = self.machine_now();
                            if let Err(err) = self.machine.handle_channel_input(mnow, ch, &cd.data)
                            {
                                debug!(ice_id = %self.config.ice_id, error = %err, "channel input decode error");
                            }
                        }
                    }
                    Str0mEvent::ChannelClose(id) => {
                        if let Some(ch) = self.registry.unregister_channel(id) {
                            let mut stats_dirty = false;
                            if ch == DataChannel::Control {
                                self.stats.control_open = false;
                                stats_dirty = true;
                            }
                            let mut emit_tool_closed = false;
                            if self.stats.tool_channels_ready && !self.registry.has_tool_channels()
                            {
                                self.stats.tool_channels_ready = false;
                                stats_dirty = true;
                                emit_tool_closed = true;
                            }
                            if stats_dirty {
                                self.publish_stats();
                            }
                            if emit_tool_closed {
                                self.emit(PeerSessionEvent::ToolChannelsClosed {
                                    ice_id: self.config.ice_id.clone(),
                                })
                                .await;
                            }
                        }
                    }
                    Str0mEvent::MediaData(data) => {
                        if let Some(frame) = media_data_to_video_frame(&data) {
                            self.stats.video_frames_rx += 1;
                            self.publish_stats();
                            self.emit(PeerSessionEvent::VideoFrame(frame)).await;
                        } else if let Some(frame) = media_data_to_audio_frame(&data) {
                            self.stats.audio_frames_rx += 1;
                            self.publish_stats();
                            self.emit(PeerSessionEvent::AudioFrame(frame)).await;
                        }
                    }
                    Str0mEvent::MediaEgressStats(stats) => {
                        if self.registry.video_tx_mid() == Some(stats.mid)
                            && let Some(governor) = &mut self.video_governor
                            && let Some(loss) = stats.loss
                            && let Some(decision) = governor.on_sample(GovernorSample {
                                now: stats
                                    .timestamp
                                    .saturating_duration_since(self.start_instant),
                                fraction_lost_pct: loss * 100.0,
                                rtt_ms: stats
                                    .rtt
                                    .map(|rtt| u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX)),
                            })
                        {
                            self.emit(PeerSessionEvent::VideoCapChanged {
                                bitrate_kbps: decision.bitrate_kbps,
                                fps_cap: decision.fps_cap,
                            })
                            .await;
                        }
                    }
                    Str0mEvent::KeyframeRequest(_) => {
                        self.emit(PeerSessionEvent::KeyframeRequested).await;
                    }
                    Str0mEvent::Closed => {
                        return Ok(None);
                    }
                    _ => {}
                },
            }
        }
    }

    async fn drain_machine(&mut self) -> Result<bool, PeerSessionError> {
        let mut mutated_rtc = false;

        while let Some(tx) = self.machine.poll_transmit() {
            match tx {
                PeerTransmit::SendSoac { payload, .. } => {
                    if let Err(err) = self.signaling.send_soac(payload).await {
                        warn!(ice_id = %self.config.ice_id, error = %err, "send_soac failed");
                    }
                }
                PeerTransmit::ApplyRemoteOffer { sdp, .. } => {
                    let sdp = if self.effective_force_relay {
                        filter_sdp_relay_only(&sdp)
                    } else {
                        sdp
                    };
                    if let Some(alloc) = &mut self.turn_alloc {
                        for ip in extract_sdp_candidate_ips(&sdp) {
                            let _ = alloc.ensure_permission(ip).await;
                        }
                    }
                    let offer = SdpOffer::from_sdp_string(&sdp)
                        .map_err(|e| PeerSessionError::Sdp(e.to_string()))?;
                    let answer = self
                        .rtc
                        .sdp_api()
                        .accept_offer(offer)
                        .map_err(|e| PeerSessionError::Sdp(e.to_string()))?;
                    mutated_rtc = true;
                    let answer_sdp = answer.to_sdp_string();
                    self.initial_mid = first_sdp_mid(&answer_sdp);
                    self.machine.on_local_answer(answer_sdp)?;
                    for cand in &self.local_candidates {
                        self.machine.on_local_candidate(
                            cand.to_sdp_string(),
                            self.initial_mid.clone(),
                            self.initial_mid.as_ref().map(|_| 0),
                        )?;
                    }
                }
                PeerTransmit::ApplyRemoteAnswer { sdp, .. } => {
                    let sdp = if self.effective_force_relay {
                        filter_sdp_relay_only(&sdp)
                    } else {
                        sdp
                    };
                    if let Some(alloc) = &mut self.turn_alloc {
                        for ip in extract_sdp_candidate_ips(&sdp) {
                            let _ = alloc.ensure_permission(ip).await;
                        }
                    }
                    let answer = SdpAnswer::from_sdp_string(&sdp)
                        .map_err(|e| PeerSessionError::Sdp(e.to_string()))?;
                    let pending = self.pending_offer.take().ok_or_else(|| {
                        PeerSessionError::Sdp("received SDP answer with no pending offer".into())
                    })?;
                    self.rtc
                        .sdp_api()
                        .accept_answer(pending, answer)
                        .map_err(|e| PeerSessionError::Sdp(e.to_string()))?;
                    mutated_rtc = true;
                }
                PeerTransmit::AddRemoteCandidate { candidate, .. } => {
                    let trimmed = candidate
                        .trim()
                        .strip_prefix("a=")
                        .unwrap_or(candidate.trim());
                    match Candidate::from_sdp_string(trimmed) {
                        Ok(cand) => {
                            if self.effective_force_relay && cand.kind() != CandidateKind::Relayed {
                                continue;
                            }
                            if let Some(alloc) = &mut self.turn_alloc {
                                let _ = alloc.ensure_permission(cand.addr().ip()).await;
                            }
                            self.rtc.add_remote_candidate(cand);
                            mutated_rtc = true;
                        }
                        Err(err) => {
                            debug!(ice_id = %self.config.ice_id, error = %err, "ignored invalid remote candidate");
                        }
                    }
                }
                PeerTransmit::SendForwardSetting { ice_id } => {
                    let mut payloads =
                        vec![encode_forward_setting(&MediaMessage::DeviceCapability(
                            remora_proto::session::media::DeviceCapability {
                                ice_id: ice_id.clone(),
                                display_info: vec![
                                    remora_proto::session::media::DisplayCapabilityInfo {
                                        id: 0,
                                        fps: self.config.video_fps,
                                        kind: 0,
                                        hdr: 0,
                                        extra: Default::default(),
                                    },
                                ],
                                video_codec_capability: vec![
                                    remora_proto::session::media::VideoCodecCapability {
                                        video_codec: 1,
                                        chroma_sampling: 1,
                                        codec_impl: 5,
                                        bit_depth: 8,
                                        width: self.config.video_width,
                                        height: self.config.video_height,
                                        extra: Default::default(),
                                    },
                                ],
                                extra: Default::default(),
                            },
                        ))];
                    payloads.push(encode_forward_setting(&MediaMessage::SenderParaInfo(
                        remora_proto::session::media::SenderParaInfo {
                            ice_id: ice_id.clone(),
                            capture_impl: "Headless".to_owned(),
                            encoder_impl: "Software".to_owned(),
                            video_track_index: 0,
                            sender_media_infos: Vec::new(),
                            extra: Default::default(),
                        },
                    )));
                    for data in payloads {
                        let mut body = serde_json::Map::new();
                        if let Some(cid) = self.machine.remote_client_id() {
                            body.insert("client_id".into(), Value::String(cid.to_owned()));
                        }
                        body.insert("data".into(), data);
                        let _ = self
                            .signaling
                            .send_forward_setting(Value::Object(body))
                            .await;
                    }
                }
                PeerTransmit::SendIceFinishedLog { ice_id } => {
                    if let Some(app_id) = self.machine.app_control_id() {
                        let _ = self.signaling.send_ice_finished(&ice_id, app_id).await;
                    }
                }
                PeerTransmit::SendControlInitBurst { .. } => {
                    let mnow = self.machine_now();
                    let ts = i64::try_from(mnow.as_millis()).unwrap_or(i64::MAX);
                    let frames = build_host_init_burst(
                        || self.machine.alloc_seq(),
                        ts,
                        self.config.video_width,
                        self.config.video_height,
                        self.config.video_fps,
                        self.config.enable_hevc,
                        self.config.clipboard_enabled,
                    );
                    for frame in frames {
                        if self.send_or_queue(DataChannel::Control, frame, false) {
                            mutated_rtc = true;
                        }
                    }
                }
                PeerTransmit::SendChannel { channel, data } => {
                    if self.send_or_queue(channel, data, false) {
                        mutated_rtc = true;
                    }
                }
                PeerTransmit::ClosePeer { .. } => {
                    self.rtc.disconnect();
                    mutated_rtc = true;
                }
                _ => {}
            }
        }

        while let Some(ev) = self.machine.poll_event() {
            match ev {
                PeerEvent::ControlChannelReady { ice_id } => {
                    self.emit(PeerSessionEvent::ControlChannelReady { ice_id })
                        .await;
                }
                PeerEvent::PeerReady { ice_id } => {
                    self.stats.peer_ready = true;
                    self.publish_stats();
                    self.emit(PeerSessionEvent::PeerReady { ice_id }).await;
                }
                PeerEvent::Control(msg) => {
                    if let ControlMessage::Unknown { raw, .. } = msg.as_ref() {
                        debug!(
                            ice_id = %self.config.ice_id,
                            len = raw.len(),
                            fields = ?inspect_protobuf_shape(raw),
                            shape = %format_protobuf_shape(raw),
                            "received unknown control protobuf"
                        );
                    } else if self.clipboard_shape_logs < MAX_CLIPBOARD_SHAPE_LOGS
                        && tracing::enabled!(tracing::Level::DEBUG)
                        && extract_clipboard_message(&msg).is_some()
                    {
                        self.clipboard_shape_logs += 1;
                        let encoded = encode_message(&msg);
                        debug!(
                            ice_id = %self.config.ice_id,
                            shape = %format_protobuf_shape(&encoded),
                            "received clipboard RPC shape"
                        );
                    }
                    self.emit(PeerSessionEvent::Control(msg)).await;
                }
                PeerEvent::Input(input) => {
                    self.emit(PeerSessionEvent::Input(input)).await;
                }
                PeerEvent::ClipboardText(text) => {
                    self.emit(PeerSessionEvent::ClipboardText(text)).await;
                }
                PeerEvent::File(msg) => {
                    self.emit(PeerSessionEvent::File(msg)).await;
                }
                PeerEvent::Tunnel(frame) => {
                    self.emit(PeerSessionEvent::Tunnel(frame)).await;
                }
                PeerEvent::TermFrame(frame) => {
                    self.emit(PeerSessionEvent::TermFrame(frame)).await;
                }
                PeerEvent::TerminalControl(msg) => {
                    self.emit(PeerSessionEvent::TerminalControl(msg)).await;
                }
                PeerEvent::System(msg) => {
                    self.emit(PeerSessionEvent::System(msg)).await;
                }
                PeerEvent::RawChannelData { channel, data } => {
                    self.emit(PeerSessionEvent::RawChannelData { channel, data })
                        .await;
                }
                PeerEvent::EchoTimedOut {
                    ice_id,
                    request_seq,
                } => {
                    self.emit(PeerSessionEvent::EchoTimedOut {
                        ice_id,
                        request_seq,
                    })
                    .await;
                }
                PeerEvent::Closed { .. } => {}
                _ => {}
            }
        }

        Ok(mutated_rtc)
    }
}

async fn detect_local_ip(probe_target: Option<SocketAddr>) -> Option<IpAddr> {
    let target = probe_target.unwrap_or_else(|| "192.0.2.1:80".parse().expect("valid test net"));
    let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sock.connect(target).await.ok()?;
    Some(sock.local_addr().ok()?.ip())
}

fn extract_sdp_candidate_ips(sdp: &str) -> Vec<IpAddr> {
    let mut ips = Vec::new();
    for line in sdp.lines() {
        let trimmed = line.trim_end_matches('\r');
        if let Some(cand_str) = trimmed.strip_prefix("a=")
            && cand_str.starts_with("candidate:")
            && let Ok(cand) = Candidate::from_sdp_string(cand_str)
        {
            ips.push(cand.addr().ip());
        }
    }
    ips
}

fn first_sdp_mid(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|line| line.strip_prefix("a=mid:").map(|s| s.trim().to_owned()))
}

fn filter_sdp_relay_only(sdp: &str) -> String {
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.starts_with("a=candidate:") && !trimmed.contains(" typ relay") {
            continue;
        }
        out.push_str(trimmed);
        out.push_str(eol);
    }
    out
}

fn normalize_soac_sdp(mut soac: SoacEvent) -> SoacEvent {
    let decompress = |b64: &str| -> Option<String> {
        if b64.len() > MAX_COMPRESSED_SDP_B64 {
            return None;
        }
        let compressed = BASE64.decode(b64).ok()?;
        let mut out = String::new();
        GzDecoder::new(compressed.as_slice())
            .take((MAX_SDP_BYTES + 1) as u64)
            .read_to_string(&mut out)
            .ok()?;
        (out.len() <= MAX_SDP_BYTES).then_some(out)
    };

    if let Some(Value::Object(map)) = soac.data.as_mut()
        && map
            .get("sdp")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        && let Some(gz) = map.get("gzip_sdp").and_then(Value::as_str)
        && let Some(sdp) = decompress(gz)
    {
        map.insert("sdp".to_owned(), Value::String(sdp));
    } else if soac
        .extra
        .get("sdp")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(gz) = soac.extra.get("gzip_sdp").and_then(Value::as_str)
        && let Some(sdp) = decompress(gz)
    {
        soac.extra.insert("sdp".to_owned(), Value::String(sdp));
    }
    soac
}

#[cfg(test)]
mod tests {
    use super::*;
    use remora_proto::session::media::VideoCodec;
    use remora_proto::session::system::encode_virtual_audio_policy_request;
    use remora_proto::session::terminal::TERM_VERSION;
    use remora_proto::session::tunnel::{TunnelFrameType, TunnelSyn, TunnelSynAck};

    fn sample_h264_frame(pts_ms: u64, keyframe: bool) -> EncodedVideoFrame {
        let mut data = Vec::new();
        if keyframe {
            // SPS + PPS + IDR
            data.extend_from_slice(&[
                0, 0, 0, 1, 0x67, 0x42, 0xE0, 0x1F, 0x8C, 0x8D, 0x40, 0x50, 0x1E, 0xD0, 0x0F, 0x08,
                0x84, 0x6A,
            ]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0xA0]);
        } else {
            data.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x22, 0x10]);
        }
        EncodedVideoFrame {
            codec: VideoCodec::H264,
            is_keyframe: keyframe,
            pts_ms,
            capture_index: Some(2),
            is_new_frame: Some(keyframe),
            data: data.into(),
        }
    }

    #[tokio::test]
    async fn force_relay_without_turn_fails() {
        let (signaling, _outgoing) = ChannelSignalingRelay::new();
        let mut config = PeerSessionConfig::new(PeerRole::Host, "ice-force-relay");
        config.force_relay = true;
        assert!(matches!(
            PeerHandle::spawn(config, Arc::new(signaling)).await,
            Err(PeerSessionError::NoRelayAvailable)
        ));
    }

    #[tokio::test]
    async fn prefer_relay_with_invalid_turn_falls_back_while_force_relay_fails() {
        let bad_server = IceServerSpec {
            urls: vec!["turn:not-a-valid-port:abc".into()],
            username: "u".into(),
            credential: "p".into(),
        };

        let (signaling, _outgoing) = ChannelSignalingRelay::new();
        let mut force_cfg = PeerSessionConfig::new(PeerRole::Viewer, "ice-force-bad-turn");
        force_cfg.force_relay = true;
        force_cfg.ice_servers = vec![bad_server.clone()];
        assert!(matches!(
            PeerHandle::spawn(force_cfg, Arc::new(signaling)).await,
            Err(PeerSessionError::Ice(_))
        ));

        let (signaling, mut outgoing) = ChannelSignalingRelay::new();
        let mut prefer_cfg = PeerSessionConfig::new(PeerRole::Viewer, "ice-prefer-bad-turn");
        prefer_cfg.prefer_relay = true;
        prefer_cfg.ice_servers = vec![bad_server];
        let peer = PeerHandle::spawn(prefer_cfg, Arc::new(signaling))
            .await
            .unwrap();
        let Some(PeerSignalingOut::Soac(_)) =
            tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
                .await
                .unwrap()
        else {
            panic!("expected direct ICE offer fallback");
        };
        peer.sender().close().await;
    }

    #[tokio::test]
    async fn prefer_relay_without_turn_offers_direct_ice() {
        let (signaling, mut outgoing) = ChannelSignalingRelay::new();
        let mut config = PeerSessionConfig::new(PeerRole::Viewer, "ice-prefer-relay");
        config.prefer_relay = true;
        let peer = PeerHandle::spawn(config, Arc::new(signaling))
            .await
            .unwrap();

        let Some(PeerSignalingOut::Soac(offer)) =
            tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
                .await
                .unwrap()
        else {
            panic!("expected viewer offer");
        };
        let offer: SoacEvent = serde_json::from_value(offer).unwrap();
        let Some(sdp) = offer
            .data
            .as_ref()
            .and_then(|data| data.get("sdp"))
            .and_then(Value::as_str)
        else {
            panic!("expected SDP in viewer offer");
        };
        assert!(
            sdp.lines()
                .any(|line| line.starts_with("a=candidate:") && line.contains(" typ host"))
        );
        peer.close().await;
    }

    async fn loopback_peers() -> (PeerHandle, PeerHandle, tokio::task::JoinHandle<()>) {
        let (host_sig, mut host_out) = ChannelSignalingRelay::new();
        let (viewer_sig, mut viewer_out) = ChannelSignalingRelay::new();

        let mut host_cfg = PeerSessionConfig::new(PeerRole::Host, "ice-loop-1");
        host_cfg.app_control_id = Some("ctrl-1".into());
        host_cfg.remote_client_id = Some("viewer-cid".into());

        let mut viewer_cfg = PeerSessionConfig::new(PeerRole::Viewer, "ice-loop-1");
        viewer_cfg.app_control_id = Some("ctrl-1".into());
        viewer_cfg.remote_client_id = Some("host-cid".into());
        host_cfg.local_probe_target = Some((Ipv4Addr::LOCALHOST, 9).into());
        viewer_cfg.local_probe_target = host_cfg.local_probe_target;

        let mut host = PeerHandle::spawn(host_cfg, Arc::new(host_sig))
            .await
            .unwrap();
        let mut viewer = PeerHandle::spawn(viewer_cfg, Arc::new(viewer_sig))
            .await
            .unwrap();

        let host_sender = host.sender();
        let viewer_sender = viewer.sender();

        let relay_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = viewer_out.recv() => {
                        if let PeerSignalingOut::Soac(val) = msg {
                            let soac: SoacEvent = serde_json::from_value(val).unwrap();
                            let _ = host_sender.feed_soac(soac).await;
                        }
                    }
                    Some(msg) = host_out.recv() => {
                        if let PeerSignalingOut::Soac(val) = msg {
                            let soac: SoacEvent = serde_json::from_value(val).unwrap();
                            let _ = viewer_sender.feed_soac(soac).await;
                        }
                    }
                    else => break,
                }
            }
        });

        // Wait for both sides to complete the control-channel echo.
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut host_ready = false;
            let mut viewer_ready = false;
            while !(host_ready && viewer_ready) {
                tokio::select! {
                    Some(ev) = host.next_event() => {
                        if matches!(ev, PeerSessionEvent::PeerReady { .. }) {
                            host_ready = true;
                        }
                    }
                    Some(ev) = viewer.next_event() => {
                        if matches!(ev, PeerSessionEvent::PeerReady { .. }) {
                            viewer_ready = true;
                        }
                    }
                }
            }
        })
        .await
        .expect("peers did not complete echo handshake");
        (host, viewer, relay_task)
    }

    async fn expect_peer_event(
        peer: &mut PeerHandle,
        description: &str,
        matches: impl Fn(&PeerSessionEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = peer.next_event().await.expect("peer closed");
                if matches(&event) {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
    }

    async fn wait_for_data_channel(
        sender: &PeerSender,
        receiver: &mut PeerHandle,
        channel: DataChannel,
    ) {
        // A protobuf with only seq set surfaces as Unknown on Text, and raw on File/Binary.
        let probe = [0x08, 0x01];
        let mut retry = tokio::time::interval(Duration::from_millis(50));
        let received = expect_peer_event(receiver, channel.as_str(), |event| match event {
            PeerSessionEvent::RawChannelData {
                channel: received_on,
                data,
            } => *received_on == channel && *data == probe,
            PeerSessionEvent::Control(msg) if channel == DataChannel::Text => {
                matches!(msg.as_ref(), ControlMessage::Unknown { raw, .. } if *raw == probe)
            }
            _ => false,
        });
        tokio::pin!(received);
        loop {
            tokio::select! {
                _ = &mut received => break,
                _ = retry.tick() => sender.send_channel_raw(channel.clone(), probe.to_vec()).await.unwrap(),
            }
        }
    }

    #[tokio::test]
    async fn str0m_host_and_viewer_exchange_video_input_and_clipboard() {
        let (mut host, mut viewer, relay_task) = loopback_peers().await;

        // Send H.264 frames from host -> viewer
        host.send_video_frame(sample_h264_frame(0, true))
            .await
            .unwrap();
        host.send_video_frame(sample_h264_frame(33, false))
            .await
            .unwrap();
        host.send_video_frame(sample_h264_frame(66, false))
            .await
            .unwrap();

        // Send input & clipboard from viewer -> host
        viewer
            .send_input(InputMessage::Keyboard {
                key: 65,
                pressed: true,
            })
            .await
            .unwrap();
        viewer.send_clipboard("hello-clipboard").await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            let mut frames_got = 0u64;
            let mut input_got = false;
            let mut clip_got = false;
            while frames_got < 3 || !input_got || !clip_got {
                tokio::select! {
                    Some(ev) = viewer.next_event() => {
                        if let PeerSessionEvent::VideoFrame(f) = ev {
                            assert_eq!(f.codec, VideoCodec::H264);
                            assert!(!f.data.is_empty());
                            assert_eq!(f.capture_index, Some(2));
                            assert_eq!(f.is_new_frame, Some(f.pts_ms == 0));
                            frames_got += 1;
                        }
                    }
                    Some(ev) = host.next_event() => {
                        match ev {
                            PeerSessionEvent::Input(InputMessage::Keyboard { key: 65, pressed: true }) => {
                                input_got = true;
                            }
                            PeerSessionEvent::ClipboardText(t) if t == "hello-clipboard" => {
                                clip_got = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
        })
        .await
        .expect("video/input/clipboard roundtrip timed out");

        host.close().await;
        viewer.close().await;
        relay_task.abort();
    }

    #[tokio::test]
    async fn str0m_host_and_viewer_exchange_session_codecs() {
        use pb::file_transfer_ftp_request::Which as FileRequest;
        use pb::file_transfer_ftp_response::Which as FileResponse;

        let (mut host, mut viewer, relay_task) = loopback_peers().await;
        let host_sender = host.sender();
        let viewer_sender = viewer.sender();
        for channel in [DataChannel::File, DataChannel::Binary, DataChannel::Text] {
            wait_for_data_channel(&host_sender, &mut viewer, channel).await;
        }

        let task = pb::TaskId {
            task_id: 17,
            file_index: 0,
        };
        let remove = FileRequest::RemoveDir(pb::FileRemoveDir {
            id: Some(task),
            path: "/example/remove".into(),
            recursive: false,
        });
        viewer
            .send_file_request(DataChannel::Control, 42, remove.clone())
            .await
            .unwrap();
        expect_peer_event(&mut host, "file RemoveDir request", |event| {
            matches!(event, PeerSessionEvent::File(msg) if matches!(msg.as_ref(),
                FileMessage::Request { request_id: 42, request, .. }
                    if request.which.as_ref() == Some(&remove)))
        })
        .await;
        let removed = FileResponse::Result(pb::FileTransferResult {
            id: Some(task),
            file_error: pb::FileTransferError::FtSucceeded as i32,
            err_msg: String::new(),
        });
        host_sender
            .send_file_response(42, removed.clone())
            .await
            .unwrap();
        expect_peer_event(&mut viewer, "file RemoveDir result", |event| {
            matches!(event, PeerSessionEvent::File(msg) if matches!(msg.as_ref(),
                FileMessage::Response { request_id: 42, response, .. }
                    if response.which.as_ref() == Some(&removed)))
        })
        .await;

        let syn = TunnelFrame {
            seq: 1,
            timestamp: 2,
            session_id: 3,
            rule_id: 4,
            stream_id: 5,
            frame_type: TunnelFrameType::Syn,
            payload: serde_json::to_vec(&TunnelSyn {
                version: 1,
                target_host: "target.invalid".into(),
                target_port: 12345,
            })
            .unwrap(),
        };
        viewer_sender.send_tunnel_frame(syn.clone()).await.unwrap();
        expect_peer_event(
            &mut host,
            "tunnel SYN",
            |event| matches!(event, PeerSessionEvent::Tunnel(frame) if frame == &syn),
        )
        .await;
        let ack = TunnelFrame {
            seq: 2,
            frame_type: TunnelFrameType::SynAck,
            payload: serde_json::to_vec(&TunnelSynAck {
                version: 1,
                ok: true,
                error: String::new(),
            })
            .unwrap(),
            ..syn
        };
        host.send_tunnel_frame(ack.clone()).await.unwrap();
        expect_peer_event(
            &mut viewer,
            "tunnel SYN_ACK",
            |event| matches!(event, PeerSessionEvent::Tunnel(frame) if frame == &ack),
        )
        .await;

        let term = TermFrame {
            version: TERM_VERSION,
            frame_type: 1,
            flags: [0; 2],
            session_id: 7,
            payload: b"terminal data".to_vec(),
        };
        viewer.send_term_frame(term.clone()).await.unwrap();
        expect_peer_event(
            &mut host,
            "binary TERM frame",
            |event| matches!(event, PeerSessionEvent::TermFrame(frame) if frame == &term),
        )
        .await;
        host.send_terminal_session_changed().await.unwrap();
        expect_peer_event(&mut viewer, "binary terminal session change", |event| {
            matches!(
                event,
                PeerSessionEvent::TerminalControl(TerminalControlMessage::SessionChanged { .. })
            )
        })
        .await;

        let privacy = pb::PrivateScreenSettingResponse::default();
        host.send_privacy_response(43, privacy).await.unwrap();
        expect_peer_event(&mut viewer, "privacy response", |event| {
            matches!(event, PeerSessionEvent::System(msg) if matches!(msg.as_ref(),
                SystemMessage::PrivacyResponse { request_id: 43, response, .. } if *response == privacy))
        }).await;
        let draw = pb::DrawResponse {
            which: Some(pb::draw_response::Which::ToggleResponse(
                pb::DrawToggleResponse { error_code: 0 },
            )),
        };
        host.send_draw_response(44, draw).await.unwrap();
        expect_peer_event(&mut viewer, "draw response", |event| {
            matches!(event, PeerSessionEvent::System(msg) if matches!(msg.as_ref(),
                SystemMessage::DrawResponse { request_id: 44, response, .. } if *response == draw))
        })
        .await;

        viewer_sender
            .send_channel_raw(
                DataChannel::Control,
                encode_virtual_audio_policy_request(1, 2, 45, 1),
            )
            .await
            .unwrap();
        expect_peer_event(&mut host, "virtual audio policy probe", |event| {
            matches!(event, PeerSessionEvent::System(msg) if matches!(msg.as_ref(),
                SystemMessage::VirtualAudioPolicyRequest { request_id: 45, policy: 1, .. }))
        })
        .await;
        host.send_virtual_audio_policy_response(DataChannel::Control, 45, 0)
            .await
            .unwrap();
        expect_peer_event(
            &mut viewer,
            "virtual audio policy no-op response",
            |event| {
                matches!(event, PeerSessionEvent::System(msg) if matches!(msg.as_ref(),
                SystemMessage::VirtualAudioPolicyResponse { request_id: 45, error_code: 0, .. }))
            },
        )
        .await;

        assert!(host.stats().tx_bytes > 0);
        assert!(host.stats().rx_bytes > 0);
        assert!(viewer.stats().tx_bytes > 0);
        assert!(viewer.stats().rx_bytes > 0);

        host.close().await;
        viewer.close().await;
        relay_task.abort();
    }

    #[test]
    fn path_rate_monitor_computes_bps_and_resets_safely() {
        let t0 = Instant::now();
        let mut monitor = PathRateMonitor::default();
        assert_eq!(
            monitor.sample(t0, "direct", 1_000, 2_000),
            Some((0, 0, true))
        );
        assert_eq!(
            monitor.sample(t0 + Duration::from_secs(5), "direct", 6_000, 12_000),
            None
        );
        // Route change surfaces immediately with measured bitrates.
        assert_eq!(
            monitor.sample(t0 + Duration::from_secs(10), "relay", 11_000, 22_000),
            Some((8_000, 16_000, true))
        );
        monitor.reset();
        assert_eq!(
            monitor.sample(t0 + Duration::from_secs(15), "direct", 500, 500),
            Some((0, 0, true))
        );
    }

    #[tokio::test]
    async fn early_text_replies_buffer_until_channel_opens() {
        let (host_sig, mut host_out) = ChannelSignalingRelay::new();
        let (viewer_sig, mut viewer_out) = ChannelSignalingRelay::new();

        let mut host_cfg = PeerSessionConfig::new(PeerRole::Host, "ice-early-text");
        host_cfg.local_probe_target = Some((Ipv4Addr::LOCALHOST, 9).into());
        let mut viewer_cfg = PeerSessionConfig::new(PeerRole::Viewer, "ice-early-text");
        viewer_cfg.local_probe_target = host_cfg.local_probe_target;

        let host = PeerHandle::spawn(host_cfg, Arc::new(host_sig))
            .await
            .unwrap();
        let mut viewer = PeerHandle::spawn(viewer_cfg, Arc::new(viewer_sig))
            .await
            .unwrap();

        // Queue replies before any SOAC exchange occurs so TEXT_DATA_CHANNEL is guaranteed closed.
        let privacy_a = pb::PrivateScreenSettingResponse {
            req_privacy_type: 1,
            cur_privacy_type: 1,
            ..Default::default()
        };
        let privacy_b = pb::PrivateScreenSettingResponse {
            req_privacy_type: 2,
            cur_privacy_type: 2,
            ..Default::default()
        };
        host.send_privacy_response(101, privacy_a).await.unwrap();
        host.send_privacy_response(102, privacy_b).await.unwrap();

        let host_sender = host.sender();
        let viewer_sender = viewer.sender();
        let relay_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = viewer_out.recv() => {
                        if let PeerSignalingOut::Soac(val) = msg {
                            let soac: SoacEvent = serde_json::from_value(val).unwrap();
                            let _ = host_sender.feed_soac(soac).await;
                        }
                    }
                    Some(msg) = host_out.recv() => {
                        if let PeerSignalingOut::Soac(val) = msg {
                            let soac: SoacEvent = serde_json::from_value(val).unwrap();
                            let _ = viewer_sender.feed_soac(soac).await;
                        }
                    }
                    else => break,
                }
            }
        });

        expect_peer_event(&mut viewer, "first buffered privacy response", |event| {
            matches!(event, PeerSessionEvent::System(msg) if matches!(msg.as_ref(),
                SystemMessage::PrivacyResponse { request_id: 101, response, .. } if *response == privacy_a))
        })
        .await;
        expect_peer_event(&mut viewer, "second buffered privacy response", |event| {
            matches!(event, PeerSessionEvent::System(msg) if matches!(msg.as_ref(),
                SystemMessage::PrivacyResponse { request_id: 102, response, .. } if *response == privacy_b))
        })
        .await;

        host.close().await;
        viewer.close().await;
        relay_task.abort();
    }
}
