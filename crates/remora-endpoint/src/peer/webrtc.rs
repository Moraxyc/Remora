//! WebRTC peer session driver combining `str0m`, [`PeerMachine`], and TURN/STUN ICE transport.

use std::fmt;
use std::future::Future;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::GzDecoder;
use rand::SeedableRng;
use rand::rngs::StdRng;
use remora_proto::machine::peer::{
    IceServerSpec, PeerError, PeerEvent, PeerMachine, PeerRole, PeerTransmit,
};
use remora_proto::machine::{Machine, Timestamp};
use remora_proto::pb;
use remora_proto::session::DataChannel;
use remora_proto::session::control::{ControlMessage, encode_message};
use remora_proto::session::input::InputMessage;
use remora_proto::session::media::{MediaMessage, encode_forward_setting};
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
    ChannelError, ChannelRegistry, DEFAULT_DATA_CHANNELS, build_host_init_burst,
    build_send_video_track_request, media_data_to_audio_frame, media_data_to_video_frame,
    write_audio_frame, write_channel, write_video_frame,
};
use crate::peer::turn::{TurnAllocation, TurnError, stun_binding_address};
use crate::platform::{EncodedAudioFrame, EncodedVideoFrame};

const CMD_CAP: usize = 256;
const EVENT_CAP: usize = 256;
const UDP_RECV_CAP: usize = 512;
const MAX_COMPRESSED_SDP_B64: usize = 2 * 1024 * 1024;
const MAX_SDP_BYTES: usize = 1024 * 1024;

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

#[cfg(feature = "client")]
impl SignalingRelay for crate::RoomSender {
    fn send_soac(&self, payload: Value) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move { self.send_soac(payload).await.map_err(|e| e.to_string()) })
    }

    fn send_forward_setting(&self, payload: Value) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.send_forward_setting(payload)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn send_ice_finished<'a>(
        &'a self,
        ice_id: &'a str,
        app_control_id: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.send_ice_finished(ice_id, app_control_id)
                .await
                .map_err(|e| e.to_string())
        })
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
    pub feature_flag: pb::FeatureFlag,
    pub enable_hevc: bool,
    pub enable_audio: bool,
    pub video_width: i32,
    pub video_height: i32,
    pub video_fps: i32,
    pub clipboard_enabled: bool,
    pub local_probe_target: Option<SocketAddr>,
}

impl fmt::Debug for PeerSessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerSessionConfig")
            .field("role", &self.role)
            .field("ice_id", &self.ice_id)
            .field("force_relay", &self.force_relay)
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
    pub peer_ready: bool,
    pub is_relayed: bool,
    pub turn_packets_tx: u64,
    pub turn_packets_rx: u64,
    pub host_packets_tx: u64,
    pub host_packets_rx: u64,
    pub video_frames_tx: u64,
    pub video_frames_rx: u64,
    pub audio_frames_tx: u64,
    pub audio_frames_rx: u64,
}

/// High-level events emitted by a [`PeerHandle`].
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum PeerSessionEvent {
    Connected { ice_id: String, is_relayed: bool },
    ControlChannelReady { ice_id: String },
    PeerReady { ice_id: String },
    Input(InputMessage),
    ClipboardText(String),
    VideoFrame(EncodedVideoFrame),
    AudioFrame(EncodedAudioFrame),
    Control(Box<ControlMessage>),
    KeyframeRequested,
    EchoTimedOut { ice_id: String, request_seq: i64 },
    RawChannelData { channel: DataChannel, data: Vec<u8> },
    Closed { ice_id: String },
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
            Self::Control(msg) => f.debug_tuple("Control").field(msg).finish(),
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
    SendChannelRaw { channel: DataChannel, data: Vec<u8> },
    RequestKeyframe,
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
        let start_instant = Instant::now();
        let mut rtc_cfg = RtcConfig::new().set_sctp_max_message_size(262_144);
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

        if !config.force_relay {
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

        let mut turn_alloc = TurnAllocation::allocate_first(&config.ice_servers).await?;
        if let Some(alloc) = &turn_alloc {
            if let Ok(cand) = Candidate::relayed(alloc.relayed_addr(), alloc.local_addr(), "udp")
                && let Some(added) = rtc.add_local_candidate(cand)
            {
                local_candidates.push(added.clone());
            }
        } else if config.force_relay {
            return Err(PeerSessionError::NoRelayAvailable);
        }

        let mut machine =
            PeerMachine::new(StdRng::from_os_rng()).with_feature_flag(config.feature_flag);
        machine.start_session(
            Timestamp::ZERO,
            config.role,
            config.ice_id.clone(),
            config.remote_client_id.clone(),
            config.app_control_id.clone(),
            config.force_relay,
        );

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
            start_instant,
            rtc,
            machine,
            registry: ChannelRegistry::new(),
            host_sockets,
            udp_tasks,
            udp_rx,
            turn_alloc: turn_alloc.take(),
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

struct PeerLoopState<R: SignalingRelay> {
    config: PeerSessionConfig,
    start_instant: Instant,
    rtc: Rtc,
    machine: PeerMachine<StdRng>,
    registry: ChannelRegistry,
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
}

impl<R: SignalingRelay> PeerLoopState<R> {
    fn machine_now(&self) -> Timestamp {
        Timestamp::from_duration(self.start_instant.elapsed())
    }

    fn publish_stats(&self) {
        let _ = self.stats_tx.send(self.stats.clone());
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
            let sleep_until = tokio::time::Instant::from_std(earliest);

            tokio::select! {
                _ = tokio::time::sleep_until(sleep_until) => {
                    let now = Instant::now();
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
                }
                pkt = self.udp_rx.recv(), if !self.host_sockets.is_empty() => {
                    let Some((local_addr, src_addr, data)) = pkt else {
                        break;
                    };
                    if let Ok(contents) = data.as_slice().try_into() {
                        self.stats.host_packets_rx += 1;
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
                            let data = encode_message(&msg);
                            if let Err(err) = write_channel(
                                &mut self.rtc,
                                &self.registry,
                                &DataChannel::Control,
                                &data,
                            ) {
                                debug!(ice_id = %self.config.ice_id, error = %err, "control write skipped");
                            }
                        }
                        Some(PeerCommand::SendChannelRaw { channel, data }) => {
                            if let Err(err) = write_channel(&mut self.rtc, &self.registry, &channel, &data) {
                                debug!(ice_id = %self.config.ice_id, error = %err, "channel raw write skipped");
                            }
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
            let mutated_rtc = self.drain_machine().await?;
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
                        self.last_tx_was_relay = false;
                        if let Err(err) = sock.send_to(&tx.contents, tx.destination).await {
                            debug!(ice_id = %self.config.ice_id, error = %err, "UDP send failed");
                        }
                    }
                }
                Str0mOutput::Event(ev) => match ev {
                    Str0mEvent::Connected => {
                        let is_relayed = self.config.force_relay
                            || (self.last_tx_was_relay && self.last_rx_was_relay)
                            || (self.stats.turn_packets_tx > 0 && self.stats.host_packets_tx == 0);
                        self.stats.connected = true;
                        self.stats.is_relayed = is_relayed;
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
                        if ch == DataChannel::Control {
                            self.stats.control_open = true;
                            self.publish_stats();
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
                        if let Some(ch) = self.registry.unregister_channel(id)
                            && ch == DataChannel::Control
                        {
                            self.stats.control_open = false;
                            self.publish_stats();
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
                    let sdp = if self.config.force_relay {
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
                    let sdp = if self.config.force_relay {
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
                            if self.config.force_relay && cand.kind() != CandidateKind::Relayed {
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
                        if write_channel(
                            &mut self.rtc,
                            &self.registry,
                            &DataChannel::Control,
                            &frame,
                        )
                        .is_ok()
                        {
                            mutated_rtc = true;
                        }
                    }
                }
                PeerTransmit::SendChannel { channel, data } => {
                    if write_channel(&mut self.rtc, &self.registry, &channel, &data).is_ok() {
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
                    self.emit(PeerSessionEvent::Control(msg)).await;
                }
                PeerEvent::Input(input) => {
                    self.emit(PeerSessionEvent::Input(input)).await;
                }
                PeerEvent::ClipboardText(text) => {
                    self.emit(PeerSessionEvent::ClipboardText(text)).await;
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
    use std::time::Duration;

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
            data,
        }
    }

    #[tokio::test]
    async fn str0m_host_and_viewer_exchange_video_input_and_clipboard() {
        let (host_sig, mut host_out) = ChannelSignalingRelay::new();
        let (viewer_sig, mut viewer_out) = ChannelSignalingRelay::new();

        let mut host_cfg = PeerSessionConfig::new(PeerRole::Host, "ice-loop-1");
        host_cfg.app_control_id = Some("ctrl-1".into());
        host_cfg.remote_client_id = Some("viewer-cid".into());

        let mut viewer_cfg = PeerSessionConfig::new(PeerRole::Viewer, "ice-loop-1");
        viewer_cfg.app_control_id = Some("ctrl-1".into());
        viewer_cfg.remote_client_id = Some("host-cid".into());

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

        // Wait until viewer PeerReady (echo complete)
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
}
