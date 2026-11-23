//! Sans-IO per-connection peer session state machine.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use rand_core::Rng;
use serde_json::{Value, json};

use super::{Machine, Timestamp};
use crate::pb;
use crate::session::DataChannel;
use crate::session::clipboard::{
    CLIPBOARD_CHUNK_BYTES, ClipboardAssembler, ClipboardError, TEXT_FORMAT_ID, chunk_text,
    encode_clipboard_change, encode_format_list_request, encode_text_rpc_request,
    encode_text_rpc_response, is_mobile_platform, windows_unicode_text_format,
};
use crate::session::control::{
    ControlDecodeError, ControlMessage, decode_message, echo_request, echo_response, encode_message,
};
use crate::session::file::{
    FILE_REPLY_CHANNEL, FileMessage, encode_file_request, encode_file_response,
    extract_file_message,
};
use crate::session::input::{InputDecodeError, InputMessage, decode_input, encode_input};
use crate::session::system::{
    DRAW_REPLY_CHANNEL, PRIVACY_REPLY_CHANNEL, SystemMessage, encode_draw_response,
    encode_privacy_response, encode_virtual_audio_policy_response, extract_system_message,
};
use crate::session::terminal::{
    TERMINAL_CHANNEL, TermDecodeError, TermFrame, TerminalControlMessage, decode_term_frame,
    encode_session_changed, encode_term_frame, extract_terminal_control, is_term_frame,
};
use crate::session::tunnel::{
    TUNNEL_CHANNEL, TunnelFrame, encode_tunnel_frame, extract_tunnel_frame,
};
use crate::signal::SoacEvent;

const ECHO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EARLY_CANDIDATES: usize = 256;

/// Role of the local endpoint within a peer session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PeerRole {
    #[default]
    Host,
    Viewer,
}

/// Parsed ICE (STUN / TURN) server entry from `be-controlled` or `control` ack.
#[derive(Clone, PartialEq, Eq)]
pub struct IceServerSpec {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl fmt::Debug for IceServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let schemes: Vec<&str> = self
            .urls
            .iter()
            .filter_map(|u| u.split(':').next())
            .collect();
        f.debug_struct("IceServerSpec")
            .field("schemes", &schemes)
            .field("url_count", &self.urls.len())
            .field("has_credentials", &(!self.credential.is_empty()))
            .finish()
    }
}

/// Parses `iceServers` JSON array from `be-controlled` or `control` ack.
pub fn parse_ice_servers(value: &Value) -> Vec<IceServerSpec> {
    let Some(list) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in list.iter().take(16) {
        let raw_urls = entry.get("urls").or_else(|| entry.get("url"));
        let urls: Vec<String> = match raw_urls {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(Value::as_str)
                .take(16)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
        .into_iter()
        .filter(|u| {
            u.len() <= 2048
                && ["stun:", "stuns:", "turn:", "turns:"]
                    .iter()
                    .any(|prefix| u.starts_with(prefix))
        })
        .collect();
        if urls.is_empty() {
            continue;
        }
        let username = entry
            .get("username")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 4096)
            .unwrap_or("")
            .to_owned();
        let credential = entry
            .get("credential")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 4096)
            .unwrap_or("")
            .to_owned();
        out.push(IceServerSpec {
            urls,
            username,
            credential,
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PeerState {
    #[default]
    Idle,
    Negotiating,
    Connected,
    Closed,
}

#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerTransmit {
    SendSoac {
        ice_id: String,
        client_id: Option<String>,
        payload: Value,
    },
    ApplyRemoteOffer {
        ice_id: String,
        sdp: String,
    },
    ApplyRemoteAnswer {
        ice_id: String,
        sdp: String,
    },
    AddRemoteCandidate {
        ice_id: String,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    SendForwardSetting {
        ice_id: String,
    },
    SendIceFinishedLog {
        ice_id: String,
    },
    SendControlInitBurst {
        ice_id: String,
    },
    SendChannel {
        channel: DataChannel,
        data: Vec<u8>,
    },
    ClosePeer {
        ice_id: String,
    },
}

impl fmt::Debug for PeerTransmit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SendSoac {
                ice_id,
                client_id,
                payload,
            } => f
                .debug_struct("SendSoac")
                .field("ice_id", ice_id)
                .field("has_client_id", &client_id.is_some())
                .field(
                    "kind",
                    &payload
                        .get("data")
                        .unwrap_or(payload)
                        .get("type")
                        .and_then(Value::as_str),
                )
                .finish(),
            Self::ApplyRemoteOffer { ice_id, sdp } => f
                .debug_struct("ApplyRemoteOffer")
                .field("ice_id", ice_id)
                .field("sdp_len", &sdp.len())
                .finish(),
            Self::ApplyRemoteAnswer { ice_id, sdp } => f
                .debug_struct("ApplyRemoteAnswer")
                .field("ice_id", ice_id)
                .field("sdp_len", &sdp.len())
                .finish(),
            Self::AddRemoteCandidate {
                ice_id,
                sdp_mid,
                sdp_mline_index,
                ..
            } => f
                .debug_struct("AddRemoteCandidate")
                .field("ice_id", ice_id)
                .field("sdp_mid", sdp_mid)
                .field("sdp_mline_index", sdp_mline_index)
                .finish(),
            Self::SendForwardSetting { ice_id } => f
                .debug_struct("SendForwardSetting")
                .field("ice_id", ice_id)
                .finish(),
            Self::SendIceFinishedLog { ice_id } => f
                .debug_struct("SendIceFinishedLog")
                .field("ice_id", ice_id)
                .finish(),
            Self::SendControlInitBurst { ice_id } => f
                .debug_struct("SendControlInitBurst")
                .field("ice_id", ice_id)
                .finish(),
            Self::SendChannel { channel, data } => f
                .debug_struct("SendChannel")
                .field("channel", channel)
                .field("len", &data.len())
                .finish(),
            Self::ClosePeer { ice_id } => {
                f.debug_struct("ClosePeer").field("ice_id", ice_id).finish()
            }
        }
    }
}

#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum PeerEvent {
    ControlChannelReady { ice_id: String },
    PeerReady { ice_id: String },
    Control(Box<ControlMessage>),
    Input(InputMessage),
    ClipboardText(String),
    File(Box<FileMessage>),
    Tunnel(TunnelFrame),
    TermFrame(TermFrame),
    TerminalControl(TerminalControlMessage),
    System(Box<SystemMessage>),
    RawChannelData { channel: DataChannel, data: Vec<u8> },
    EchoTimedOut { ice_id: String, request_seq: i64 },
    Closed { ice_id: String },
}

impl fmt::Debug for PeerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlChannelReady { ice_id } => f
                .debug_struct("ControlChannelReady")
                .field("ice_id", ice_id)
                .finish(),
            Self::PeerReady { ice_id } => {
                f.debug_struct("PeerReady").field("ice_id", ice_id).finish()
            }
            Self::Control(msg) => f.debug_tuple("Control").field(msg).finish(),
            Self::Input(input) => f.debug_tuple("Input").field(input).finish(),
            Self::ClipboardText(text) => f
                .debug_struct("ClipboardText")
                .field("len", &text.len())
                .finish(),
            Self::File(msg) => f.debug_tuple("File").field(msg).finish(),
            Self::Tunnel(frame) => f.debug_tuple("Tunnel").field(frame).finish(),
            Self::TermFrame(frame) => f.debug_tuple("TermFrame").field(frame).finish(),
            Self::TerminalControl(msg) => f.debug_tuple("TerminalControl").field(msg).finish(),
            Self::System(msg) => f.debug_tuple("System").field(msg).finish(),
            Self::RawChannelData { channel, data } => f
                .debug_struct("RawChannelData")
                .field("channel", channel)
                .field("len", &data.len())
                .finish(),
            Self::EchoTimedOut {
                ice_id,
                request_seq,
            } => f
                .debug_struct("EchoTimedOut")
                .field("ice_id", ice_id)
                .field("request_seq", request_seq)
                .finish(),
            Self::Closed { ice_id } => f.debug_struct("Closed").field("ice_id", ice_id).finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerError {
    MissingIceId,
    InvalidSoac(&'static str),
    Control(ControlDecodeError),
    Input(InputDecodeError),
    Clipboard(ClipboardError),
    Terminal(TermDecodeError),
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingIceId => f.write_str("peer session has no active ice_id"),
            Self::InvalidSoac(reason) => write!(f, "invalid signaling payload: {reason}"),
            Self::Control(err) => err.fmt(f),
            Self::Input(err) => err.fmt(f),
            Self::Clipboard(err) => err.fmt(f),
            Self::Terminal(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for PeerError {}

/// Sans-IO per-`ice_id` peer session state machine.
pub struct PeerMachine<R: Rng> {
    state: PeerState,
    role: PeerRole,
    ice_id: Option<String>,
    remote_client_id: Option<String>,
    app_control_id: Option<String>,
    force_relay: bool,
    remote_sdp_applied: bool,
    early_candidates: Vec<(String, Option<String>, Option<u16>)>,
    host_features: Option<pb::FeatureFlag>,
    rng: R,
    next_seq: i64,
    next_clipboard_id: u32,
    clipboard_assembler: ClipboardAssembler,
    controller_platform: Option<u8>,
    clipboard_ready: bool,
    early_clipboard: Option<String>,
    pending_echo: Option<(i64, Timestamp)>,
    transmits: VecDeque<PeerTransmit>,
    events: VecDeque<PeerEvent>,
}

impl<R: Rng> fmt::Debug for PeerMachine<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerMachine")
            .field("state", &self.state)
            .field("role", &self.role)
            .field("ice_id", &self.ice_id)
            .field("force_relay", &self.force_relay)
            .field("remote_sdp_applied", &self.remote_sdp_applied)
            .field("controller_platform", &self.controller_platform)
            .field("clipboard_ready", &self.clipboard_ready)
            .field("has_early_clipboard", &self.early_clipboard.is_some())
            .finish()
    }
}

impl<R: Rng> PeerMachine<R> {
    pub fn new(mut rng: R) -> Self {
        let initial_seq = i64::from((rng.next_u32() % 1000) + 1);
        Self {
            state: PeerState::Idle,
            role: PeerRole::Host,
            ice_id: None,
            remote_client_id: None,
            app_control_id: None,
            force_relay: false,
            remote_sdp_applied: false,
            early_candidates: Vec::new(),
            host_features: None,
            rng,
            next_seq: initial_seq,
            next_clipboard_id: 1,
            clipboard_assembler: ClipboardAssembler::new(),
            controller_platform: None,
            clipboard_ready: true,
            early_clipboard: None,
            pending_echo: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn with_feature_flag(mut self, features: pb::FeatureFlag) -> Self {
        self.host_features = Some(features);
        self
    }

    pub fn state(&self) -> PeerState {
        self.state
    }

    pub fn role(&self) -> PeerRole {
        self.role
    }

    pub fn ice_id(&self) -> Option<&str> {
        self.ice_id.as_deref()
    }

    pub fn remote_client_id(&self) -> Option<&str> {
        self.remote_client_id.as_deref()
    }

    pub fn app_control_id(&self) -> Option<&str> {
        self.app_control_id.as_deref()
    }

    pub fn force_relay(&self) -> bool {
        self.force_relay
    }

    pub fn set_controller_platform(&mut self, platform: Option<u8>) {
        self.controller_platform = platform;
    }

    pub fn controller_platform(&self) -> Option<u8> {
        self.controller_platform
    }

    /// When the clipboard bridge becomes ready, delivers the newest buffered text.
    pub fn set_clipboard_ready(&mut self, ready: bool) {
        let was_ready = std::mem::replace(&mut self.clipboard_ready, ready);
        if ready
            && !was_ready
            && let Some(text) = self.early_clipboard.take()
        {
            self.events.push_back(PeerEvent::ClipboardText(text));
        }
    }

    pub fn alloc_seq(&mut self) -> i64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Generates random bytes using the injected [`Rng`].
    pub fn next_random_bytes(&mut self, buf: &mut [u8]) {
        self.rng.fill_bytes(buf);
    }

    /// Starts negotiating a peer connection for `ice_id` (default [`PeerRole::Host`]).
    pub fn start(&mut self, _now: Timestamp, ice_id: impl Into<String>) {
        self.start_session(Timestamp::ZERO, PeerRole::Host, ice_id, None, None, false);
    }

    /// Starts negotiating a peer connection with explicit role and signaling metadata.
    pub fn start_session(
        &mut self,
        _now: Timestamp,
        role: PeerRole,
        ice_id: impl Into<String>,
        remote_client_id: Option<String>,
        app_control_id: Option<String>,
        force_relay: bool,
    ) {
        self.role = role;
        self.ice_id = Some(ice_id.into());
        self.remote_client_id = remote_client_id;
        self.app_control_id = app_control_id;
        self.force_relay = force_relay;
        self.state = PeerState::Negotiating;
        self.remote_sdp_applied = false;
        self.early_candidates.clear();
        self.pending_echo = None;
        self.clipboard_assembler.reset();
        self.early_clipboard = None;
    }

    /// Emits an outbound `soac` offer generated by the local WebRTC engine.
    pub fn on_local_offer(&mut self, sdp: impl Into<String>) -> Result<(), PeerError> {
        let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        let mut data = json!({
            "type": "offer",
            "sdp": sdp.into(),
            "ice_id": ice_id,
            "ice_network_type": 3,
        });
        if let Some(app) = &self.app_control_id {
            data["app_control_id"] = Value::String(app.clone());
        }
        let payload = self.wrap_soac_payload(data);
        self.transmits.push_back(PeerTransmit::SendSoac {
            ice_id,
            client_id: self.remote_client_id.clone(),
            payload,
        });
        Ok(())
    }

    /// Emits an outbound `soac` answer generated by the local WebRTC engine.
    pub fn on_local_answer(&mut self, sdp: impl Into<String>) -> Result<(), PeerError> {
        let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        let mut data = json!({
            "type": "answer",
            "sdp": sdp.into(),
            "ice_id": ice_id,
        });
        if let Some(app) = &self.app_control_id {
            data["app_control_id"] = Value::String(app.clone());
        }
        let payload = self.wrap_soac_payload(data);
        self.transmits.push_back(PeerTransmit::SendSoac {
            ice_id,
            client_id: self.remote_client_id.clone(),
            payload,
        });
        Ok(())
    }

    /// Emits an outbound trickle ICE candidate over `soac`.
    pub fn on_local_candidate(
        &mut self,
        candidate: impl Into<String>,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<(), PeerError> {
        let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        let mut cand_obj = json!({
            "candidate": candidate.into(),
        });
        if let Some(mid) = sdp_mid {
            cand_obj["sdpMid"] = Value::String(mid);
        }
        if let Some(idx) = sdp_mline_index {
            cand_obj["sdpMLineIndex"] = Value::Number(idx.into());
        }
        let mut data = json!({
            "type": "candidate",
            "candidate": cand_obj,
            "ice_id": ice_id,
        });
        if let Some(app) = &self.app_control_id {
            data["app_control_id"] = Value::String(app.clone());
        }
        let payload = self.wrap_soac_payload(data);
        self.transmits.push_back(PeerTransmit::SendSoac {
            ice_id,
            client_id: self.remote_client_id.clone(),
            payload,
        });
        Ok(())
    }

    fn wrap_soac_payload(&self, data: Value) -> Value {
        let mut outer = serde_json::Map::new();
        if let Some(cid) = &self.remote_client_id {
            outer.insert("client_id".to_owned(), Value::String(cid.clone()));
        }
        outer.insert("data".to_owned(), data);
        Value::Object(outer)
    }

    /// Processes an inbound `soac` event (`offer`, `answer`, or `candidate`).
    ///
    /// Events belonging to a different `ice_id` are ignored without error.
    /// Remote candidates arriving before the remote SDP is applied are buffered
    /// and emitted immediately after `ApplyRemoteOffer` / `ApplyRemoteAnswer`.
    pub fn handle_soac(&mut self, _now: Timestamp, soac: &SoacEvent) -> Result<(), PeerError> {
        let active_ice = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        if let Some(ev_ice) = soac.ice_id()
            && ev_ice != active_ice
        {
            return Ok(());
        }
        if self.remote_client_id.is_none()
            && let Some(cid) = &soac.client_id
            && !cid.is_empty()
        {
            self.remote_client_id = Some(cid.clone());
        }

        let body = soac
            .data
            .as_ref()
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| Value::Object(soac.extra.clone()));
        let kind = body.get("type").and_then(Value::as_str);

        match kind {
            Some("offer") => {
                let sdp = body
                    .get("sdp")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or(PeerError::InvalidSoac("offer missing plain sdp"))?
                    .to_owned();
                self.transmits.push_back(PeerTransmit::ApplyRemoteOffer {
                    ice_id: active_ice.clone(),
                    sdp,
                });
                self.remote_sdp_applied = true;
                self.flush_early_candidates(&active_ice);
            }
            Some("answer") => {
                let sdp = body
                    .get("sdp")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or(PeerError::InvalidSoac("answer missing plain sdp"))?
                    .to_owned();
                self.transmits.push_back(PeerTransmit::ApplyRemoteAnswer {
                    ice_id: active_ice.clone(),
                    sdp,
                });
                self.remote_sdp_applied = true;
                self.flush_early_candidates(&active_ice);
            }
            Some("candidate") => {
                let Some((cand, mid, mline)) = extract_candidate(&body) else {
                    return Ok(());
                };
                if cand.is_empty() {
                    return Ok(());
                }
                if self.remote_sdp_applied {
                    self.transmits.push_back(PeerTransmit::AddRemoteCandidate {
                        ice_id: active_ice,
                        candidate: cand,
                        sdp_mid: mid,
                        sdp_mline_index: mline,
                    });
                } else if self.early_candidates.len() < MAX_EARLY_CANDIDATES {
                    self.early_candidates.push((cand, mid, mline));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn flush_early_candidates(&mut self, ice_id: &str) {
        for (candidate, sdp_mid, sdp_mline_index) in self.early_candidates.drain(..) {
            self.transmits.push_back(PeerTransmit::AddRemoteCandidate {
                ice_id: ice_id.to_owned(),
                candidate,
                sdp_mid,
                sdp_mline_index,
            });
        }
    }

    /// Marks the WebRTC ICE/DTLS transport as connected and queues initial signaling telemetry.
    pub fn on_ice_connected(&mut self, _now: Timestamp) -> Result<(), PeerError> {
        let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        self.state = PeerState::Connected;
        if self.role == PeerRole::Host {
            self.transmits.push_back(PeerTransmit::SendForwardSetting {
                ice_id: ice_id.clone(),
            });
            self.transmits
                .push_back(PeerTransmit::SendIceFinishedLog { ice_id });
        }
        Ok(())
    }

    /// Announces that a WebRTC data channel opened; for `CONTROL_DATA_CHANNEL`, queues the
    /// initialization burst (for host) and sends an echo handshake probe.
    pub fn on_channel_open(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
    ) -> Result<(), PeerError> {
        let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        if channel == DataChannel::Control {
            if self.role == PeerRole::Host {
                self.transmits
                    .push_back(PeerTransmit::SendControlInitBurst {
                        ice_id: ice_id.clone(),
                    });
            }
            let seq = self.alloc_seq();
            let probe = encode_message(&echo_request(
                seq,
                i64::try_from(now.as_millis()).unwrap_or(i64::MAX),
                self.host_features,
            ));
            self.pending_echo = Some((seq, now + ECHO_TIMEOUT));
            self.transmits.push_back(PeerTransmit::SendChannel {
                channel: DataChannel::Control,
                data: probe,
            });
            self.events
                .push_back(PeerEvent::ControlChannelReady { ice_id });
        }
        Ok(())
    }

    /// Encodes and queues an input message on `CONTROL_DATA_CHANNEL`.
    pub fn queue_input(&mut self, input: &InputMessage) -> Result<(), PeerError> {
        if self.ice_id.is_none() {
            return Err(PeerError::MissingIceId);
        }
        self.transmits.push_back(PeerTransmit::SendChannel {
            channel: DataChannel::Control,
            data: encode_input(input),
        });
        Ok(())
    }

    /// Encodes and queues UTF-8 clipboard text as chunked `ClipboardChange` frames.
    pub fn queue_clipboard_text(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        text: &str,
    ) -> Result<(), PeerError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.ice_id.is_none() {
            return Err(PeerError::MissingIceId);
        }
        let content_id = self.next_clipboard_id;
        self.next_clipboard_id = self.next_clipboard_id.wrapping_add(1).max(1);
        let ts = i64::try_from(now.as_millis()).unwrap_or(i64::MAX);
        let chunks = chunk_text(text, CLIPBOARD_CHUNK_BYTES);
        let total = chunks.len();
        for (idx, chunk) in chunks.into_iter().enumerate() {
            let seq = self.alloc_seq();
            let more = idx + 1 < total;
            let data = encode_clipboard_change(seq, ts, content_id, chunk, more);
            self.transmits.push_back(PeerTransmit::SendChannel {
                channel: channel.clone(),
                data,
            });
        }
        Ok(())
    }

    /// Encodes and queues nonempty clipboard text as a text-change RPC.
    pub fn queue_clipboard_text_rpc(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        request_id: i64,
        text: &str,
    ) -> Result<(), PeerError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.ice_id.is_none() {
            return Err(PeerError::MissingIceId);
        }
        let seq = self.alloc_seq();
        let ts = i64::try_from(now.as_millis()).unwrap_or(i64::MAX);
        let data = encode_text_rpc_request(seq, ts, request_id, text);
        self.transmits
            .push_back(PeerTransmit::SendChannel { channel, data });
        Ok(())
    }

    /// Offers Unicode text as a rich format and also sends chunked plain text to mobile controllers.
    pub fn queue_clipboard_offer_for_peer(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        request_id: i64,
        text: &str,
    ) -> Result<(), PeerError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.ice_id.is_none() {
            return Err(PeerError::MissingIceId);
        }
        let seq = self.alloc_seq();
        let ts = i64::try_from(now.as_millis()).unwrap_or(i64::MAX);
        let data =
            encode_format_list_request(seq, ts, request_id, vec![windows_unicode_text_format()]);
        let mobile = self.controller_platform.is_some_and(is_mobile_platform);
        self.transmits.push_back(PeerTransmit::SendChannel {
            channel: channel.clone(),
            data,
        });
        if mobile {
            self.queue_clipboard_text(now, channel, text)?;
        }
        Ok(())
    }

    pub fn queue_file_request(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        request_id: i64,
        which: pb::file_transfer_ftp_request::Which,
    ) -> Result<(), PeerError> {
        self.queue_stamped_channel(now, channel, |seq, ts| {
            encode_file_request(seq, ts, request_id, which)
        })
    }

    pub fn queue_file_response(
        &mut self,
        now: Timestamp,
        request_id: i64,
        which: pb::file_transfer_ftp_response::Which,
    ) -> Result<(), PeerError> {
        self.queue_stamped_channel(now, FILE_REPLY_CHANNEL, |seq, ts| {
            encode_file_response(seq, ts, request_id, which)
        })
    }

    pub fn queue_tunnel_frame(&mut self, frame: &TunnelFrame) -> Result<(), PeerError> {
        self.queue_channel(TUNNEL_CHANNEL, |_| encode_tunnel_frame(frame))
    }

    pub fn queue_term_frame(&mut self, frame: &TermFrame) -> Result<(), PeerError> {
        self.queue_channel(TERMINAL_CHANNEL, |_| encode_term_frame(frame))
    }

    pub fn queue_terminal_session_changed(&mut self, now: Timestamp) -> Result<(), PeerError> {
        self.queue_stamped_channel(now, TERMINAL_CHANNEL, encode_session_changed)
    }

    pub fn queue_privacy_response(
        &mut self,
        now: Timestamp,
        request_id: i64,
        resp: pb::PrivateScreenSettingResponse,
    ) -> Result<(), PeerError> {
        self.queue_stamped_channel(now, PRIVACY_REPLY_CHANNEL, |seq, ts| {
            encode_privacy_response(seq, ts, request_id, resp)
        })
    }

    pub fn queue_draw_response(
        &mut self,
        now: Timestamp,
        request_id: i64,
        resp: pb::DrawResponse,
    ) -> Result<(), PeerError> {
        self.queue_stamped_channel(now, DRAW_REPLY_CHANNEL, |seq, ts| {
            encode_draw_response(seq, ts, request_id, resp)
        })
    }

    pub fn queue_virtual_audio_policy_response(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        request_id: i64,
        error_code: i32,
    ) -> Result<(), PeerError> {
        self.queue_stamped_channel(now, channel, |seq, ts| {
            encode_virtual_audio_policy_response(seq, ts, request_id, error_code)
        })
    }

    fn queue_stamped_channel(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        encode: impl FnOnce(i64, i64) -> Vec<u8>,
    ) -> Result<(), PeerError> {
        self.queue_channel(channel, |peer| {
            encode(
                peer.alloc_seq(),
                i64::try_from(now.as_millis()).unwrap_or(i64::MAX),
            )
        })
    }

    fn queue_channel(
        &mut self,
        channel: DataChannel,
        encode: impl FnOnce(&mut Self) -> Vec<u8>,
    ) -> Result<(), PeerError> {
        self.ice_id.as_ref().ok_or(PeerError::MissingIceId)?;
        let data = encode(self);
        self.transmits
            .push_back(PeerTransmit::SendChannel { channel, data });
        Ok(())
    }

    fn receive_clipboard_text(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.clipboard_ready {
            self.events.push_back(PeerEvent::ClipboardText(text));
        } else {
            self.early_clipboard = Some(text);
        }
    }

    /// Feeds bytes arriving on a specific WebRTC data channel.
    pub fn handle_channel_input(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        bytes: &[u8],
    ) -> Result<(), PeerError> {
        let event = match channel {
            DataChannel::Control | DataChannel::Text => {
                if bytes.first() == Some(&b'{') {
                    let input = decode_input(bytes).map_err(PeerError::Input)?;
                    self.events.push_back(PeerEvent::Input(input));
                    return Ok(());
                }
                let msg = decode_message(bytes).map_err(PeerError::Control)?;
                let ts = i64::try_from(now.as_millis()).unwrap_or(i64::MAX);
                match &msg {
                    ControlMessage::EchoRequest { seq, .. } => {
                        let reply_seq = self.alloc_seq();
                        let reply =
                            encode_message(&echo_response(reply_seq, ts, *seq, self.host_features));
                        self.transmits.push_back(PeerTransmit::SendChannel {
                            channel: channel.clone(),
                            data: reply,
                        });
                    }
                    ControlMessage::EchoResponse { request_seq, .. } => {
                        if let Some((expected, _)) = self.pending_echo
                            && *request_seq == Some(expected)
                        {
                            self.pending_echo = None;
                            if let Some(ice_id) = &self.ice_id {
                                self.events.push_back(PeerEvent::PeerReady {
                                    ice_id: ice_id.clone(),
                                });
                            }
                        }
                    }
                    ControlMessage::ClipboardChange { change, .. } => {
                        if let Some(text) = self
                            .clipboard_assembler
                            .accept(change.content_id, &change.content, change.to_be_continued)
                            .map_err(PeerError::Clipboard)?
                        {
                            self.receive_clipboard_text(text);
                        }
                    }
                    ControlMessage::RpcRequest { request, .. } => {
                        if let Some(pb::rpc_request::Which::TextChangeRequest(req)) = &request.which
                            && req.format_id == TEXT_FORMAT_ID
                        {
                            let request_id =
                                request.request_header.as_ref().map_or(0, |h| h.request_id);
                            let reply_seq = self.alloc_seq();
                            let reply = encode_text_rpc_response(
                                reply_seq,
                                ts,
                                request_id,
                                pb::ClipboardError::CbSucceeded as i32,
                            );
                            self.transmits.push_back(PeerTransmit::SendChannel {
                                channel: channel.clone(),
                                data: reply,
                            });
                            self.receive_clipboard_text(req.data.clone());
                        }
                    }
                    _ => {}
                }
                if let Some(file) = extract_file_message(&msg) {
                    self.events.push_back(PeerEvent::File(Box::new(file)));
                }
                if let Some(control) = extract_terminal_control(&msg) {
                    self.events.push_back(PeerEvent::TerminalControl(control));
                }
                if let Some(system) = extract_system_message(&msg) {
                    self.events.push_back(PeerEvent::System(Box::new(system)));
                }
                Some(PeerEvent::Control(Box::new(msg)))
            }
            DataChannel::File => decode_message(bytes).ok().and_then(|msg| {
                if let Some(file) = extract_file_message(&msg) {
                    Some(PeerEvent::File(Box::new(file)))
                } else {
                    extract_tunnel_frame(&msg).map(PeerEvent::Tunnel)
                }
            }),
            DataChannel::Binary if is_term_frame(bytes) => Some(PeerEvent::TermFrame(
                decode_term_frame(bytes).map_err(PeerError::Terminal)?,
            )),
            DataChannel::Binary => decode_message(bytes)
                .ok()
                .and_then(|msg| extract_terminal_control(&msg))
                .map(PeerEvent::TerminalControl),
            _ => None,
        };
        self.events
            .push_back(event.unwrap_or_else(|| PeerEvent::RawChannelData {
                channel,
                data: bytes.to_vec(),
            }));
        Ok(())
    }

    /// Ends the peer session and queues teardown.
    pub fn release(&mut self) {
        self.state = PeerState::Closed;
        self.pending_echo = None;
        self.early_candidates.clear();
        self.early_clipboard = None;
        if let Some(ice_id) = self.ice_id.take() {
            self.transmits.push_back(PeerTransmit::ClosePeer {
                ice_id: ice_id.clone(),
            });
            self.events.push_back(PeerEvent::Closed { ice_id });
        }
    }
}

fn extract_candidate(body: &Value) -> Option<(String, Option<String>, Option<u16>)> {
    let cand_val = body.get("candidate")?;
    if let Some(s) = cand_val.as_str() {
        let mid = body
            .get("sdpMid")
            .or_else(|| body.get("sdp_mid"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mline = body
            .get("sdpMLineIndex")
            .or_else(|| body.get("sdp_mline_index"))
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok());
        Some((s.to_owned(), mid, mline))
    } else if cand_val.is_object() {
        let cand = cand_val
            .get("candidate")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mid = cand_val
            .get("sdpMid")
            .or_else(|| cand_val.get("sdp_mid"))
            .or_else(|| body.get("sdpMid"))
            .or_else(|| body.get("sdp_mid"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mline = cand_val
            .get("sdpMLineIndex")
            .or_else(|| cand_val.get("sdp_mline_index"))
            .or_else(|| body.get("sdpMLineIndex"))
            .or_else(|| body.get("sdp_mline_index"))
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok());
        Some((cand, mid, mline))
    } else {
        None
    }
}

impl<R: Rng> Machine for PeerMachine<R> {
    type Transmit = PeerTransmit;
    type Event = PeerEvent;
    type Error = PeerError;

    fn handle_input(&mut self, now: Timestamp, input: &[u8]) -> Result<(), Self::Error> {
        self.handle_channel_input(now, DataChannel::Control, input)
    }

    fn handle_timeout(&mut self, now: Timestamp) {
        if let Some((request_seq, deadline)) = self.pending_echo
            && now >= deadline
        {
            self.pending_echo = None;
            if let Some(ice_id) = &self.ice_id {
                self.events.push_back(PeerEvent::EchoTimedOut {
                    ice_id: ice_id.clone(),
                    request_seq,
                });
            }
        }
    }

    fn poll_transmit(&mut self) -> Option<Self::Transmit> {
        self.transmits.pop_front()
    }

    fn poll_event(&mut self) -> Option<Self::Event> {
        self.events.pop_front()
    }

    fn poll_timeout(&self) -> Option<Timestamp> {
        self.pending_echo.map(|(_, deadline)| deadline)
    }
}

#[cfg(test)]
mod tests {
    use rand_core::{Infallible, TryRng};

    use super::*;
    use crate::session::clipboard::{ClipboardMessage, extract_clipboard_message};
    use crate::session::system::encode_privacy_request;
    use crate::session::terminal::{TERM_VERSION, encode_rename_session_request};
    use crate::session::tunnel::TunnelFrameType;
    use crate::signal::{Event, SignalEvent};

    struct DeterministicRng(u64);

    impl TryRng for DeterministicRng {
        type Error = Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Infallible> {
            Ok(self.try_next_u64()? as u32)
        }
        fn try_next_u64(&mut self) -> Result<u64, Infallible> {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            Ok(self.0)
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Infallible> {
            for chunk in dest.chunks_mut(8) {
                let bytes = self.try_next_u64()?.to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            Ok(())
        }
    }

    #[test]
    fn scripted_peer_lifecycle_echo_and_input() {
        let ff = pb::FeatureFlag {
            ff_capture_setting: 6,
            ..Default::default()
        };
        let mut peer = PeerMachine::new(DeterministicRng(11)).with_feature_flag(ff);
        let t0 = Timestamp::ZERO;

        peer.start(t0, "ice-1");
        assert_eq!(peer.state(), PeerState::Negotiating);
        peer.on_ice_connected(t0 + Duration::from_millis(50))
            .unwrap();
        assert_eq!(peer.state(), PeerState::Connected);
        assert_eq!(
            peer.poll_transmit(),
            Some(PeerTransmit::SendForwardSetting {
                ice_id: "ice-1".into()
            })
        );
        assert_eq!(
            peer.poll_transmit(),
            Some(PeerTransmit::SendIceFinishedLog {
                ice_id: "ice-1".into()
            })
        );

        let t1 = Timestamp::from_millis(100);
        peer.on_channel_open(t1, DataChannel::Control).unwrap();
        assert_eq!(
            peer.poll_transmit(),
            Some(PeerTransmit::SendControlInitBurst {
                ice_id: "ice-1".into()
            })
        );
        let Some(PeerTransmit::SendChannel { channel, data }) = peer.poll_transmit() else {
            panic!("expected echo probe");
        };
        assert_eq!(channel, DataChannel::Control);
        let ControlMessage::EchoRequest { seq: probe_seq, .. } = decode_message(&data).unwrap()
        else {
            panic!("expected echo request");
        };
        assert_eq!(
            peer.poll_event(),
            Some(PeerEvent::ControlChannelReady {
                ice_id: "ice-1".into()
            })
        );
        assert_eq!(peer.poll_timeout(), Some(t1 + ECHO_TIMEOUT));

        // Echo response clears pending timeout and emits PeerReady
        let resp = encode_message(&echo_response(1, 200, probe_seq, Some(ff)));
        peer.handle_input(t1 + Duration::from_millis(20), &resp)
            .unwrap();
        assert_eq!(peer.poll_timeout(), None);
        assert_eq!(
            peer.poll_event(),
            Some(PeerEvent::PeerReady {
                ice_id: "ice-1".into()
            })
        );
        assert!(matches!(
            peer.poll_event(),
            Some(PeerEvent::Control(msg)) if matches!(*msg, ControlMessage::EchoResponse { .. })
        ));

        // Inbound EchoRequest from remote triggers automatic EchoResponse transmit
        let remote_req = encode_message(&echo_request(42, 300, None));
        peer.handle_input(t1 + Duration::from_millis(30), &remote_req)
            .unwrap();
        let Some(PeerTransmit::SendChannel {
            data: reply_bytes, ..
        }) = peer.poll_transmit()
        else {
            panic!("expected automatic echo reply");
        };
        let ControlMessage::EchoResponse { request_seq, .. } =
            decode_message(&reply_bytes).unwrap()
        else {
            panic!("expected echo response");
        };
        assert_eq!(request_seq, Some(42));
        let _ = peer.poll_event();

        // Inbound JSON input on CONTROL
        peer.handle_input(
            t1 + Duration::from_millis(40),
            br#"{"action":"kbd_press","key":65}"#,
        )
        .unwrap();
        assert_eq!(
            peer.poll_event(),
            Some(PeerEvent::Input(InputMessage::Keyboard {
                key: 65,
                pressed: true
            }))
        );

        peer.release();
        assert_eq!(peer.state(), PeerState::Closed);
        assert_eq!(
            peer.poll_transmit(),
            Some(PeerTransmit::ClosePeer {
                ice_id: "ice-1".into()
            })
        );
    }

    #[test]
    fn buffers_early_candidates_until_remote_sdp_and_routes_clipboard() {
        let mut viewer = PeerMachine::new(DeterministicRng(7));
        let t0 = Timestamp::ZERO;
        viewer.start_session(
            t0,
            PeerRole::Viewer,
            "ice-v",
            Some("host-cid".into()),
            Some("app-1".into()),
            true,
        );

        viewer
            .on_local_offer("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\n")
            .unwrap();
        assert!(matches!(
            viewer.poll_transmit(),
            Some(PeerTransmit::SendSoac { ice_id, .. }) if ice_id == "ice-v"
        ));

        // Early candidate before answer arrives
        let early = SignalEvent::from_event(&Event {
            name: "soac".into(),
            args: vec![json!({
                "client_id": "host-cid",
                "data": {
                    "type": "candidate",
                    "ice_id": "ice-v",
                    "candidate": {
                        "candidate": "candidate:1 1 udp 1 127.0.0.1 9000 typ host",
                        "sdpMid": "0",
                        "sdpMLineIndex": 0
                    }
                }
            })],
            attachments: vec![],
            ack_id: None,
        });
        let SignalEvent::Soac(early_soac) = early else {
            panic!("expected soac");
        };
        viewer.handle_soac(t0, &early_soac).unwrap();
        assert!(viewer.poll_transmit().is_none());

        // Answer arrives: emits ApplyRemoteAnswer followed by flushed candidate
        let ans = SignalEvent::from_event(&Event {
            name: "soac".into(),
            args: vec![json!({
                "client_id": "host-cid",
                "data": {
                    "type": "answer",
                    "ice_id": "ice-v",
                    "sdp": "v=0\r\no=- 2 2 IN IP4 0.0.0.0\r\n"
                }
            })],
            attachments: vec![],
            ack_id: None,
        });
        let SignalEvent::Soac(ans_soac) = ans else {
            panic!("expected soac");
        };
        viewer.handle_soac(t0, &ans_soac).unwrap();
        assert!(matches!(
            viewer.poll_transmit(),
            Some(PeerTransmit::ApplyRemoteAnswer { .. })
        ));
        assert!(matches!(
            viewer.poll_transmit(),
            Some(PeerTransmit::AddRemoteCandidate {
                sdp_mline_index: Some(0),
                ..
            })
        ));

        // Outbound clipboard & inbound clipboard
        viewer
            .queue_clipboard_text(t0, DataChannel::Text, "secret-clip-text")
            .unwrap();
        let Some(PeerTransmit::SendChannel { channel, data }) = viewer.poll_transmit() else {
            panic!("expected queued clipboard transmit");
        };
        assert_eq!(channel, DataChannel::Text);
        viewer
            .handle_channel_input(t0, DataChannel::Text, &data)
            .unwrap();
        assert_eq!(
            viewer.poll_event(),
            Some(PeerEvent::ClipboardText("secret-clip-text".into()))
        );
    }

    #[test]
    fn empty_clipboard_sends_nothing_and_consumes_no_ids() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let now = Timestamp::ZERO;
        let seq = peer.next_seq;
        let content_id = peer.next_clipboard_id;
        peer.set_controller_platform(Some(2));
        for active in [false, true] {
            if active {
                peer.start(now, "ice");
            }
            peer.queue_clipboard_text(now, DataChannel::Text, "")
                .unwrap();
            peer.queue_clipboard_text_rpc(now, DataChannel::Text, 42, "")
                .unwrap();
            peer.queue_clipboard_offer_for_peer(now, DataChannel::Text, 42, "")
                .unwrap();
            assert_eq!(peer.poll_transmit(), None);
            assert_eq!(peer.next_seq, seq);
            assert_eq!(peer.next_clipboard_id, content_id);
        }
    }

    #[test]
    fn buffers_latest_nonempty_clipboard_until_bridge_is_ready() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let now = Timestamp::ZERO;
        peer.start(now, "ice");
        peer.set_clipboard_ready(false);
        for wire in [
            encode_clipboard_change(1, 0, 1, "previous ", true),
            encode_clipboard_change(2, 0, 1, "secret", false),
        ] {
            peer.handle_input(now, &wire).unwrap();
            assert!(matches!(peer.poll_event(), Some(PeerEvent::Control(_))));
        }
        assert_eq!(peer.poll_event(), None);
        assert!(format!("{peer:?}").contains("has_early_clipboard: true"));
        assert!(!format!("{peer:?}").contains("previous secret"));

        for wire in [
            encode_text_rpc_request(3, 0, 42, "newest secret"),
            encode_clipboard_change(4, 0, 2, "", false),
            encode_text_rpc_request(5, 0, 43, ""),
        ] {
            peer.handle_input(now, &wire).unwrap();
            assert!(matches!(peer.poll_event(), Some(PeerEvent::Control(_))));
        }
        assert_eq!(peer.poll_event(), None);
        assert!(!format!("{peer:?}").contains("newest secret"));
        peer.set_clipboard_ready(true);
        assert_eq!(
            peer.poll_event(),
            Some(PeerEvent::ClipboardText("newest secret".into()))
        );
        peer.set_clipboard_ready(true);
        assert_eq!(peer.poll_event(), None);
    }

    #[test]
    fn clears_early_clipboard_on_start_and_release() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let now = Timestamp::ZERO;
        peer.start(now, "initial");
        peer.set_clipboard_ready(false);
        let wire = encode_clipboard_change(1, 0, 1, "old text", false);
        peer.handle_input(now, &wire).unwrap();
        assert!(matches!(peer.poll_event(), Some(PeerEvent::Control(_))));
        peer.start(now, "replacement");
        peer.set_clipboard_ready(true);
        assert_eq!(peer.poll_event(), None);

        peer.set_clipboard_ready(false);
        peer.handle_input(now, &wire).unwrap();
        assert!(matches!(peer.poll_event(), Some(PeerEvent::Control(_))));
        peer.release();
        assert!(matches!(peer.poll_event(), Some(PeerEvent::Closed { .. })));
        peer.set_clipboard_ready(true);
        assert_eq!(peer.poll_event(), None);
    }

    #[test]
    fn queues_clipboard_text_rpc_on_the_requested_channel() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let now = Timestamp::from_millis(100);
        peer.start(now, "ice");
        let seq = peer.next_seq;
        peer.queue_clipboard_text_rpc(now, DataChannel::Text, 42, "rpc text")
            .unwrap();
        assert_eq!(
            poll_clipboard(&mut peer),
            Some(ClipboardMessage::TextRpcRequest {
                seq,
                timestamp: 100,
                request_id: 42,
                format_id: TEXT_FORMAT_ID,
                text: "rpc text".into(),
            })
        );
        assert_eq!(peer.poll_transmit(), None);
    }

    #[test]
    fn offers_rich_clipboard_to_all_and_chunked_text_only_to_mobile() {
        let now = Timestamp::from_millis(100);
        let first_chunk = "a".repeat(CLIPBOARD_CHUNK_BYTES);
        let text = format!("{first_chunk}末尾");
        for (platform, mobile) in [
            (None, false),
            (Some(1), false),
            (Some(2), true),
            (Some(3), true),
            (Some(4), false),
        ] {
            let mut peer = PeerMachine::new(DeterministicRng(1));
            peer.start(now, "ice");
            peer.set_controller_platform(platform);
            assert_eq!(peer.controller_platform(), platform);
            let seq = peer.next_seq;
            let content_id = peer.next_clipboard_id;
            peer.queue_clipboard_offer_for_peer(now, DataChannel::Text, 42, &text)
                .unwrap();
            assert_eq!(
                poll_clipboard(&mut peer),
                Some(ClipboardMessage::FormatListRequest {
                    seq,
                    timestamp: 100,
                    request_id: 42,
                    formats: vec![windows_unicode_text_format()],
                })
            );
            if mobile {
                for (offset, content, more) in [(1, first_chunk.as_str(), true), (2, "末尾", false)]
                {
                    assert_eq!(
                        poll_clipboard(&mut peer),
                        Some(ClipboardMessage::Change {
                            seq: seq + offset,
                            timestamp: 100,
                            content_id,
                            content: content.into(),
                            to_be_continued: more,
                        })
                    );
                }
            }
            assert_eq!(peer.poll_transmit(), None);
        }
    }

    fn poll_clipboard(peer: &mut PeerMachine<DeterministicRng>) -> Option<ClipboardMessage> {
        let PeerTransmit::SendChannel { channel, data } = peer.poll_transmit()? else {
            panic!("expected clipboard transmit");
        };
        assert_eq!(channel, DataChannel::Text);
        extract_clipboard_message(&decode_message(&data).unwrap())
    }

    #[test]
    fn control_and_text_emit_typed_events_before_legacy_control() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let request = pb::FileRemoveDir {
            path: "/private/file".into(),
            ..Default::default()
        };
        let which = pb::file_transfer_ftp_request::Which::RemoveDir(request);
        let privacy = pb::PrivateScreenSettingRequest {
            enable: true,
            setting_source: "private-source".into(),
            ..Default::default()
        };
        let cases = [
            (
                encode_file_request(1, 2, 3, which.clone()),
                PeerEvent::File(Box::new(FileMessage::Request {
                    seq: 1,
                    timestamp: 2,
                    request_id: 3,
                    request: pb::FileTransferFtpRequest { which: Some(which) },
                })),
                "/private/file",
            ),
            (
                encode_rename_session_request(1, 2, 3, 4, "private-name"),
                PeerEvent::TerminalControl(TerminalControlMessage::RenameSessionRequest {
                    seq: 1,
                    timestamp: 2,
                    request_id: 3,
                    session_id: 4,
                    name: "private-name".into(),
                }),
                "private-name",
            ),
            (
                encode_privacy_request(1, 2, 3, privacy.clone()),
                PeerEvent::System(Box::new(SystemMessage::PrivacyRequest {
                    seq: 1,
                    timestamp: 2,
                    request_id: 3,
                    request: privacy,
                })),
                "private-source",
            ),
        ];
        for channel in [DataChannel::Control, DataChannel::Text] {
            for (wire, expected, secret) in &cases {
                peer.handle_channel_input(Timestamp::ZERO, channel.clone(), wire)
                    .unwrap();
                let typed = peer.poll_event().unwrap();
                assert_eq!(&typed, expected);
                assert!(!format!("{typed:?}").contains(secret));
                let legacy = peer.poll_event().unwrap();
                assert_eq!(
                    legacy,
                    PeerEvent::Control(Box::new(decode_message(wire).unwrap()))
                );
                assert!(!format!("{legacy:?}").contains(secret));
                assert_eq!(peer.poll_event(), None);
            }
        }
    }

    #[test]
    fn file_and_binary_preserve_unrecognized_payloads() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let unsupported = encode_message(&echo_request(1, 2, None));
        for channel in [
            DataChannel::File,
            DataChannel::Binary,
            DataChannel::Streamer,
        ] {
            for bytes in [&[0xff][..], &unsupported] {
                peer.handle_channel_input(Timestamp::ZERO, channel.clone(), bytes)
                    .unwrap();
                assert_eq!(
                    peer.poll_event(),
                    Some(PeerEvent::RawChannelData {
                        channel: channel.clone(),
                        data: bytes.to_vec(),
                    })
                );
                assert_eq!(peer.poll_event(), None);
                assert_eq!(peer.poll_transmit(), None);
            }
        }
    }

    #[test]
    fn binary_rejects_malformed_term_frames() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let mut wire = encode_term_frame(&TermFrame {
            version: TERM_VERSION,
            frame_type: 1,
            flags: [0; 2],
            session_id: 1,
            payload: vec![1],
        });
        wire.pop();
        assert_eq!(
            peer.handle_channel_input(Timestamp::ZERO, DataChannel::Binary, &wire),
            Err(PeerError::Terminal(TermDecodeError::LengthMismatch {
                expected: 1,
                actual: 0,
            }))
        );
        assert_eq!(peer.poll_event(), None);
    }

    #[test]
    fn queues_session_codec_payloads_on_their_wire_channels() {
        let mut peer = PeerMachine::new(DeterministicRng(1));
        let now = Timestamp::from_millis(100);
        let request = pb::file_transfer_ftp_request::Which::RemoveDir(pb::FileRemoveDir::default());
        let result =
            pb::file_transfer_ftp_response::Which::Result(pb::FileTransferResult::default());
        let tunnel = TunnelFrame {
            seq: 123,
            timestamp: 456,
            session_id: 1,
            rule_id: 2,
            stream_id: 3,
            frame_type: TunnelFrameType::Data,
            payload: b"tunnel payload".to_vec(),
        };
        let term = TermFrame {
            version: TERM_VERSION,
            frame_type: 1,
            flags: [0; 2],
            session_id: 1,
            payload: b"terminal payload".to_vec(),
        };
        let privacy = pb::PrivateScreenSettingResponse::default();
        let draw = pb::DrawResponse {
            which: Some(pb::draw_response::Which::ToggleResponse(
                pb::DrawToggleResponse::default(),
            )),
        };
        let enqueue = |peer: &mut PeerMachine<DeterministicRng>| {
            [
                peer.queue_file_request(now, DataChannel::Control, 1, request.clone()),
                peer.queue_file_response(now, 1, result.clone()),
                peer.queue_tunnel_frame(&tunnel),
                peer.queue_term_frame(&term),
                peer.queue_terminal_session_changed(now),
                peer.queue_privacy_response(now, 2, privacy),
                peer.queue_draw_response(now, 3, draw),
                peer.queue_virtual_audio_policy_response(now, DataChannel::Control, 4, 0),
            ]
        };

        assert_eq!(
            enqueue(&mut peer),
            std::array::from_fn(|_| Err(PeerError::MissingIceId))
        );
        assert_eq!(peer.poll_transmit(), None);
        peer.start(now, "ice");
        let seq = peer.next_seq;
        for result in enqueue(&mut peer) {
            result.unwrap();
        }
        assert_eq!(
            std::iter::from_fn(|| peer.poll_transmit()).collect::<Vec<_>>(),
            [
                (
                    DataChannel::Control,
                    encode_file_request(seq, 100, 1, request)
                ),
                (
                    DataChannel::File,
                    encode_file_response(seq + 1, 100, 1, result)
                ),
                (DataChannel::File, encode_tunnel_frame(&tunnel)),
                (DataChannel::Binary, encode_term_frame(&term)),
                (DataChannel::Binary, encode_session_changed(seq + 2, 100)),
                (
                    DataChannel::Text,
                    encode_privacy_response(seq + 3, 100, 2, privacy)
                ),
                (
                    DataChannel::Text,
                    encode_draw_response(seq + 4, 100, 3, draw)
                ),
                (
                    DataChannel::Control,
                    encode_virtual_audio_policy_response(seq + 5, 100, 4, 0)
                ),
            ]
            .map(|(channel, data)| PeerTransmit::SendChannel { channel, data })
        );

        for (channel, wire, expected) in [
            (
                DataChannel::File,
                encode_file_response(
                    1,
                    2,
                    3,
                    pb::file_transfer_ftp_response::Which::Result(pb::FileTransferResult::default()),
                ),
                PeerEvent::File(Box::new(FileMessage::Response {
                    seq: 1,
                    timestamp: 2,
                    request_id: 3,
                    response: pb::FileTransferFtpResponse {
                        which: Some(pb::file_transfer_ftp_response::Which::Result(
                            pb::FileTransferResult::default(),
                        )),
                    },
                })),
            ),
            (
                DataChannel::File,
                encode_tunnel_frame(&tunnel),
                PeerEvent::Tunnel(tunnel),
            ),
            (
                DataChannel::Binary,
                encode_term_frame(&term),
                PeerEvent::TermFrame(term),
            ),
            (
                DataChannel::Binary,
                encode_session_changed(1, 2),
                PeerEvent::TerminalControl(TerminalControlMessage::SessionChanged {
                    seq: 1,
                    timestamp: 2,
                }),
            ),
        ] {
            peer.handle_channel_input(now, channel, &wire).unwrap();
            assert_eq!(peer.poll_event(), Some(expected));
            assert_eq!(peer.poll_event(), None);
        }
    }

    #[test]
    fn scripted_echo_timeout() {
        let mut peer = PeerMachine::new(DeterministicRng(3));
        peer.start(Timestamp::ZERO, "ice-t");
        peer.on_ice_connected(Timestamp::from_millis(10)).unwrap();
        peer.on_channel_open(Timestamp::from_millis(20), DataChannel::Control)
            .unwrap();
        while peer.poll_transmit().is_some() {}
        while peer.poll_event().is_some() {}

        let deadline = peer.poll_timeout().unwrap();
        peer.handle_timeout(deadline);
        assert!(matches!(
            peer.poll_event(),
            Some(PeerEvent::EchoTimedOut { ice_id, .. }) if ice_id == "ice-t"
        ));
        assert_eq!(peer.poll_timeout(), None);
    }

    #[test]
    fn parse_ice_servers_redacts_credentials_in_debug() {
        let servers = parse_ice_servers(&json!([
            { "urls": ["stun:stun.test:3478"] },
            { "urls": ["turn:turn.test:3478"], "username": "u1", "credential": "super-secret" }
        ]));
        assert_eq!(servers.len(), 2);
        let dbg = format!("{servers:?}");
        assert!(!dbg.contains("super-secret"));
        assert!(!dbg.contains("u1"));
    }
}
