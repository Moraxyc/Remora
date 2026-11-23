//! Sans-IO Socket.IO signaling state machine.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::time::Duration;

use rand_core::Rng;
use serde_json::{Value, json};

use super::{Machine, Timestamp};
use crate::profile::SignalProfile;
use crate::signal::events::{ControlAck, RefreshReconnectKeyAck, SignalEvent, names};
use crate::signal::{
    CONNECT, DecodeError, Decoder, Handshake, PONG, Packet, encode_binary_event, encode_event,
};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_LIVENESS_WINDOW: Duration = Duration::from_secs(45);
const REFRESH_FIRST_DELAY: Duration = Duration::from_secs(30);
const REFRESH_SUCCESS_INTERVAL: Duration = Duration::from_secs(300);
const REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SignalingState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    RoomReady,
}

#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalingTransmit {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

impl fmt::Debug for SignalingTransmit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => {
                // Only show packet type prefix / length so tokens or SDP text never appear in logs.
                let prefix: String = text.chars().take(12).collect();
                f.debug_struct("Text")
                    .field("prefix", &prefix)
                    .field("len", &text.len())
                    .finish()
            }
            Self::Binary(bytes) => f.debug_struct("Binary").field("len", &bytes.len()).finish(),
            Self::Close => f.write_str("Close"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SignalingEvent {
    Connected,
    Signal(SignalEvent),
    ReconnectKeyRefreshed,
    ControlResult(ControlAck),
    Ack { id: u64, args: Vec<Value> },
    ConnectError(Value),
    TimedOut,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalingError {
    InvalidUtf8,
    Protocol(DecodeError),
}

impl fmt::Display for SignalingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => f.write_str("text signaling frame is not valid UTF-8"),
            Self::Protocol(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for SignalingError {}

/// Sans-IO Socket.IO client signaling state machine.
pub struct SignalingMachine<R: Rng> {
    state: SignalingState,
    handshake: Option<Handshake>,
    decoder: Decoder,
    rng: R,
    next_ack_id: u64,
    pending_refresh_acks: HashSet<u64>,
    pending_control_acks: HashSet<u64>,
    handshake_timeout: Duration,
    default_liveness_window: Duration,
    liveness_window: Duration,
    refresh_success_interval: Duration,
    liveness_deadline: Option<Timestamp>,
    next_refresh_at: Option<Timestamp>,
    transmits: VecDeque<SignalingTransmit>,
    events: VecDeque<SignalingEvent>,
}

impl<R: Rng> fmt::Debug for SignalingMachine<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalingMachine")
            .field("state", &self.state)
            .field("has_handshake", &self.handshake.is_some())
            .finish()
    }
}

impl<R: Rng> SignalingMachine<R> {
    pub fn new(mut rng: R) -> Self {
        let initial_ack = u64::from(rng.next_u32() % 1000) + 1;
        Self {
            state: SignalingState::Disconnected,
            handshake: None,
            decoder: Decoder::new(),
            rng,
            next_ack_id: initial_ack,
            pending_refresh_acks: HashSet::new(),
            pending_control_acks: HashSet::new(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            default_liveness_window: DEFAULT_LIVENESS_WINDOW,
            liveness_window: DEFAULT_LIVENESS_WINDOW,
            refresh_success_interval: REFRESH_SUCCESS_INTERVAL,
            liveness_deadline: None,
            next_refresh_at: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn with_signal_profile(mut rng: R, signal: &SignalProfile) -> Self {
        let initial_ack = u64::from(rng.next_u32() % 1000) + 1;
        let liveness = signal.liveness_timeout();
        Self {
            state: SignalingState::Disconnected,
            handshake: None,
            decoder: Decoder::new(),
            rng,
            next_ack_id: initial_ack,
            pending_refresh_acks: HashSet::new(),
            pending_control_acks: HashSet::new(),
            handshake_timeout: signal.handshake_timeout(),
            default_liveness_window: liveness,
            liveness_window: liveness,
            refresh_success_interval: signal.reconnect_key_refresh(),
            liveness_deadline: None,
            next_refresh_at: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn state(&self) -> SignalingState {
        self.state
    }

    pub fn handshake(&self) -> Option<&Handshake> {
        self.handshake.as_ref()
    }

    fn alloc_ack_id(&mut self) -> u64 {
        let id = self.next_ack_id;
        self.next_ack_id = self.next_ack_id.wrapping_add(1).max(1);
        id
    }

    /// Generates a random 16-byte hex identifier using the injected [`Rng`].
    pub fn next_random_hex_id(&mut self) -> String {
        let mut bytes = [0u8; 16];
        self.rng.fill_bytes(&mut bytes);
        hex::encode(bytes)
    }

    /// Transitions to `Connecting` and arms the initial handshake timeout.
    pub fn connect(&mut self, now: Timestamp) {
        self.state = SignalingState::Connecting;
        self.decoder = Decoder::new();
        self.handshake = None;
        self.pending_refresh_acks.clear();
        self.pending_control_acks.clear();
        self.liveness_window = self.default_liveness_window;
        self.liveness_deadline = Some(now + self.handshake_timeout);
        self.next_refresh_at = None;
    }

    /// Closes the signaling session from the local side.
    pub fn disconnect(&mut self) {
        self.state = SignalingState::Disconnected;
        self.liveness_deadline = None;
        self.next_refresh_at = None;
        self.transmits.push_back(SignalingTransmit::Close);
        self.events.push_back(SignalingEvent::Closed);
    }

    /// Queues a `room_info` event (host readiness announcement).
    pub fn send_room_info(&mut self) -> u64 {
        let ack_id = self.alloc_ack_id();
        self.transmits
            .push_back(SignalingTransmit::Text(encode_event(
                names::ROOM_INFO,
                &[],
                Some(ack_id),
            )));
        ack_id
    }

    /// Queues a `refresh_reconnect_key` request.
    pub fn send_refresh_reconnect_key(&mut self) -> u64 {
        let ack_id = self.alloc_ack_id();
        self.pending_refresh_acks.insert(ack_id);
        self.transmits
            .push_back(SignalingTransmit::Text(encode_event(
                names::REFRESH_RECONNECT_KEY,
                &[],
                Some(ack_id),
            )));
        ack_id
    }

    /// Queues a binary `control` request carrying serialized `ConnectOptions`.
    pub fn send_control(&mut self, header: Value, connect_options: Vec<u8>) -> u64 {
        let ack_id = self.alloc_ack_id();
        self.pending_control_acks.insert(ack_id);
        let text = encode_binary_event(names::CONTROL, &[header], 1, Some(ack_id));
        self.transmits.push_back(SignalingTransmit::Text(text));
        self.transmits
            .push_back(SignalingTransmit::Binary(connect_options));
        ack_id
    }

    /// Queues a `soac` SDP/ICE signaling event.
    pub fn send_soac(&mut self, payload: Value) {
        self.transmits
            .push_back(SignalingTransmit::Text(encode_event(
                names::SOAC,
                &[payload],
                None,
            )));
    }

    /// Queues a `forward_setting` event.
    pub fn send_forward_setting(&mut self, payload: Value) {
        self.transmits
            .push_back(SignalingTransmit::Text(encode_event(
                names::FORWARD_SETTING,
                &[payload],
                None,
            )));
    }

    /// Queues an `ice_finished_log` event.
    pub fn send_ice_finished(&mut self, ice_id: &str, app_control_id: &str) {
        self.transmits
            .push_back(SignalingTransmit::Text(encode_event(
                names::ICE_FINISHED_LOG,
                &[json!({
                    "ice_id": ice_id,
                    "app_control_id": app_control_id,
                })],
                None,
            )));
    }

    fn handle_packet(&mut self, now: Timestamp, packet: Packet) {
        self.liveness_deadline = Some(now + self.liveness_window);
        match packet {
            Packet::Open(handshake) => {
                self.liveness_window = handshake.liveness_timeout();
                self.liveness_deadline = Some(now + self.liveness_window);
                self.handshake = Some(handshake);
                self.transmits
                    .push_back(SignalingTransmit::Text(CONNECT.to_owned()));
            }
            Packet::Connected => {
                self.state = SignalingState::Connected;
                self.next_refresh_at = Some(now + REFRESH_FIRST_DELAY);
                self.events.push_back(SignalingEvent::Connected);
            }
            Packet::Ping => {
                self.transmits
                    .push_back(SignalingTransmit::Text(PONG.to_owned()));
            }
            Packet::Pong | Packet::Noop => {}
            Packet::ConnectError(val) => {
                self.state = SignalingState::Disconnected;
                self.liveness_deadline = None;
                self.next_refresh_at = None;
                self.events.push_back(SignalingEvent::ConnectError(val));
            }
            Packet::Close | Packet::Disconnected => {
                self.state = SignalingState::Disconnected;
                self.liveness_deadline = None;
                self.next_refresh_at = None;
                self.events.push_back(SignalingEvent::Closed);
            }
            Packet::Ack(ack) => {
                if self.pending_refresh_acks.remove(&ack.id) {
                    match RefreshReconnectKeyAck::decode(&ack.args) {
                        RefreshReconnectKeyAck::Success { .. } => {
                            self.next_refresh_at = Some(now + self.refresh_success_interval);
                            self.events.push_back(SignalingEvent::ReconnectKeyRefreshed);
                        }
                        _ => {
                            self.next_refresh_at = Some(now + REFRESH_RETRY_INTERVAL);
                        }
                    }
                } else if self.pending_control_acks.remove(&ack.id) {
                    self.events
                        .push_back(SignalingEvent::ControlResult(ControlAck::decode(&ack.args)));
                } else {
                    self.events.push_back(SignalingEvent::Ack {
                        id: ack.id,
                        args: ack.args,
                    });
                }
            }
            Packet::Event(event) => {
                let signal = SignalEvent::from_event(&event);
                if let SignalEvent::BmsgPush(ref push) = signal
                    && push.publisher_availability_status() == Some("ready")
                {
                    self.state = SignalingState::RoomReady;
                }
                self.events.push_back(SignalingEvent::Signal(signal));
            }
        }
    }
}

impl<R: Rng> Machine for SignalingMachine<R> {
    type Transmit = SignalingTransmit;
    type Event = SignalingEvent;
    type Error = SignalingError;

    fn handle_input(&mut self, now: Timestamp, input: &[u8]) -> Result<(), Self::Error> {
        let decoded = if self.decoder.is_awaiting_binary() {
            self.decoder
                .binary(input)
                .map_err(SignalingError::Protocol)?
        } else {
            let text = std::str::from_utf8(input).map_err(|_| SignalingError::InvalidUtf8)?;
            self.decoder.text(text).map_err(SignalingError::Protocol)?
        };
        if let Some(packet) = decoded {
            self.handle_packet(now, packet);
        }
        Ok(())
    }

    fn handle_timeout(&mut self, now: Timestamp) {
        if self.state == SignalingState::Disconnected {
            return;
        }
        if self.liveness_deadline.is_some_and(|d| now >= d) {
            self.state = SignalingState::Disconnected;
            self.liveness_deadline = None;
            self.next_refresh_at = None;
            self.transmits.push_back(SignalingTransmit::Close);
            self.events.push_back(SignalingEvent::TimedOut);
            return;
        }
        if matches!(
            self.state,
            SignalingState::Connected | SignalingState::RoomReady
        ) && self.next_refresh_at.is_some_and(|t| now >= t)
        {
            self.send_refresh_reconnect_key();
            self.next_refresh_at = Some(now + REFRESH_RETRY_INTERVAL);
        }
    }

    fn poll_transmit(&mut self) -> Option<Self::Transmit> {
        self.transmits.pop_front()
    }

    fn poll_event(&mut self) -> Option<Self::Event> {
        self.events.pop_front()
    }

    fn poll_timeout(&self) -> Option<Timestamp> {
        match (self.liveness_deadline, self.next_refresh_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use rand_core::{Infallible, TryRng};

    use super::*;

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
    fn scripted_handshake_ping_refresh_and_binary_event() {
        let mut machine = SignalingMachine::new(DeterministicRng(99));
        let t0 = Timestamp::ZERO;

        machine.connect(t0);
        assert_eq!(machine.state(), SignalingState::Connecting);
        assert_eq!(machine.poll_timeout(), Some(Timestamp::from_secs(20)));

        // Open -> emits CONNECT ("40")
        machine
            .handle_input(
                t0 + Duration::from_millis(10),
                br#"0{"sid":"s1","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#,
            )
            .unwrap();
        assert_eq!(
            machine.poll_transmit(),
            Some(SignalingTransmit::Text(CONNECT.into()))
        );

        // Connected -> emits SignalingEvent::Connected, arms 30s refresh & 45s liveness
        let t1 = Timestamp::from_millis(20);
        machine.handle_input(t1, br#"40{"sid":"s1"}"#).unwrap();
        assert_eq!(machine.state(), SignalingState::Connected);
        assert_eq!(machine.poll_event(), Some(SignalingEvent::Connected));
        assert_eq!(machine.poll_timeout(), Some(t1 + Duration::from_secs(30)));

        // Host announces room_info and receives bmsg_push ready
        let room_ack = machine.send_room_info();
        assert!(matches!(
            machine.poll_transmit(),
            Some(SignalingTransmit::Text(ref t)) if t.contains("room_info")
        ));
        machine
            .handle_input(
                Timestamp::from_millis(30),
                format!("43{room_ack}[{{}}]").as_bytes(),
            )
            .unwrap();
        let _ = machine.poll_event();
        machine
            .handle_input(
                Timestamp::from_millis(40),
                br#"42["bmsg_push",{"data":{"status":"online","publisher_availability_status":"ready"}}]"#,
            )
            .unwrap();
        assert_eq!(machine.state(), SignalingState::RoomReady);
        assert!(matches!(
            machine.poll_event(),
            Some(SignalingEvent::Signal(SignalEvent::BmsgPush(_)))
        ));

        // Server ping -> emits PONG ("3") and extends liveness deadline
        let t_ping = Timestamp::from_secs(25);
        machine.handle_input(t_ping, b"2").unwrap();
        assert_eq!(
            machine.poll_transmit(),
            Some(SignalingTransmit::Text(PONG.into()))
        );

        // 30s refresh timeout -> emits refresh_reconnect_key
        let t_refresh = t1 + Duration::from_secs(30);
        machine.handle_timeout(t_refresh);
        let Some(SignalingTransmit::Text(refresh_frame)) = machine.poll_transmit() else {
            panic!("expected refresh_reconnect_key frame");
        };
        assert!(refresh_frame.contains("refresh_reconnect_key"));
        let ack_id: u64 = refresh_frame[2..refresh_frame.find('[').unwrap()]
            .parse()
            .unwrap();
        machine
            .handle_input(
                t_refresh + Duration::from_millis(50),
                format!(r#"43{ack_id}["success",{{"reconnect_key":"rk-1"}}]"#).as_bytes(),
            )
            .unwrap();
        assert_eq!(
            machine.poll_event(),
            Some(SignalingEvent::ReconnectKeyRefreshed)
        );

        // Binary be-controlled event across two frames (header + attachment)
        machine
            .handle_input(
                t_refresh + Duration::from_millis(60),
                br#"451-["be-controlled",{"ice_id":"ice-1","client_id":"v-1","iceServers":[],"force_relay":false,"app_data":{"_placeholder":true,"num":0}}]"#,
            )
            .unwrap();
        assert_eq!(machine.poll_event(), None);
        machine
            .handle_input(t_refresh + Duration::from_millis(61), &[0x08, 0x01])
            .unwrap();
        let Some(SignalingEvent::Signal(SignalEvent::BeControlled(bc))) = machine.poll_event()
        else {
            panic!("expected BeControlled");
        };
        assert_eq!(bc.ice_id, "ice-1");
        assert_eq!(bc.connect_options, vec![0x08, 0x01]);
    }

    #[test]
    fn scripted_liveness_timeout_closes_connection() {
        let mut machine = SignalingMachine::new(DeterministicRng(1));
        machine.connect(Timestamp::ZERO);
        machine
            .handle_input(
                Timestamp::from_millis(5),
                br#"0{"sid":"s1","upgrades":[],"pingInterval":5000,"pingTimeout":5000,"maxPayload":1000000}"#,
            )
            .unwrap();
        let _ = machine.poll_transmit();
        machine
            .handle_input(Timestamp::from_millis(10), br#"40{"sid":"s1"}"#)
            .unwrap();
        let _ = machine.poll_event();

        // Advance past 10s liveness deadline without pings.
        machine.handle_timeout(Timestamp::from_secs(11));
        assert_eq!(machine.state(), SignalingState::Disconnected);
        assert_eq!(machine.poll_transmit(), Some(SignalingTransmit::Close));
        assert_eq!(machine.poll_event(), Some(SignalingEvent::TimedOut));
        assert_eq!(machine.poll_timeout(), None);
    }
}
