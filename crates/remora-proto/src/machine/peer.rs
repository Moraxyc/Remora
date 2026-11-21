//! Sans-IO per-connection peer session state machine.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use rand_core::RngCore;

use super::{Machine, Timestamp};
use crate::pb;
use crate::session::DataChannel;
use crate::session::control::{
    ControlDecodeError, ControlMessage, decode_message, echo_request, echo_response, encode_message,
};
use crate::session::input::{InputDecodeError, InputMessage, decode_input};

const ECHO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PeerState {
    #[default]
    Idle,
    Negotiating,
    Connected,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerTransmit {
    SendForwardSetting { ice_id: String },
    SendIceFinishedLog { ice_id: String },
    SendControlInitBurst { ice_id: String },
    SendChannel { channel: DataChannel, data: Vec<u8> },
    ClosePeer { ice_id: String },
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PeerEvent {
    ControlChannelReady { ice_id: String },
    Control(Box<ControlMessage>),
    Input(InputMessage),
    RawChannelData { channel: DataChannel, data: Vec<u8> },
    EchoTimedOut { ice_id: String, request_seq: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerError {
    MissingIceId,
    Control(ControlDecodeError),
    Input(InputDecodeError),
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingIceId => f.write_str("peer session has no active ice_id"),
            Self::Control(err) => err.fmt(f),
            Self::Input(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for PeerError {}

/// Sans-IO per-`ice_id` peer session state machine.
pub struct PeerMachine<R: RngCore> {
    state: PeerState,
    ice_id: Option<String>,
    host_features: Option<pb::FeatureFlag>,
    rng: R,
    next_seq: i64,
    pending_echo: Option<(i64, Timestamp)>,
    transmits: VecDeque<PeerTransmit>,
    events: VecDeque<PeerEvent>,
}

impl<R: RngCore> fmt::Debug for PeerMachine<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerMachine")
            .field("state", &self.state)
            .field("ice_id", &self.ice_id)
            .finish()
    }
}

impl<R: RngCore> PeerMachine<R> {
    pub fn new(mut rng: R) -> Self {
        let initial_seq = i64::from((rng.next_u32() % 1000) + 1);
        Self {
            state: PeerState::Idle,
            ice_id: None,
            host_features: None,
            rng,
            next_seq: initial_seq,
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

    pub fn ice_id(&self) -> Option<&str> {
        self.ice_id.as_deref()
    }

    fn alloc_seq(&mut self) -> i64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Generates random bytes using the injected [`RngCore`].
    pub fn next_random_bytes(&mut self, buf: &mut [u8]) {
        self.rng.fill_bytes(buf);
    }

    /// Starts negotiating a peer connection for `ice_id`.
    pub fn start(&mut self, _now: Timestamp, ice_id: impl Into<String>) {
        self.ice_id = Some(ice_id.into());
        self.state = PeerState::Negotiating;
        self.pending_echo = None;
    }

    /// Marks the WebRTC ICE/DTLS transport as connected and queues initial signaling telemetry.
    pub fn on_ice_connected(&mut self, _now: Timestamp) -> Result<(), PeerError> {
        let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        self.state = PeerState::Connected;
        self.transmits.push_back(PeerTransmit::SendForwardSetting {
            ice_id: ice_id.clone(),
        });
        self.transmits
            .push_back(PeerTransmit::SendIceFinishedLog { ice_id });
        Ok(())
    }

    /// Announces that a WebRTC data channel opened; for `CONTROL_DATA_CHANNEL`, queues the
    /// initialization burst and sends an echo handshake probe.
    pub fn on_channel_open(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
    ) -> Result<(), PeerError> {
        let ice_id = self.ice_id.clone().ok_or(PeerError::MissingIceId)?;
        if channel == DataChannel::Control {
            self.transmits
                .push_back(PeerTransmit::SendControlInitBurst {
                    ice_id: ice_id.clone(),
                });
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

    /// Feeds bytes arriving on a specific WebRTC data channel.
    pub fn handle_channel_input(
        &mut self,
        now: Timestamp,
        channel: DataChannel,
        bytes: &[u8],
    ) -> Result<(), PeerError> {
        match channel {
            DataChannel::Control | DataChannel::Text => {
                if bytes.first() == Some(&b'{') {
                    let input = decode_input(bytes).map_err(PeerError::Input)?;
                    self.events.push_back(PeerEvent::Input(input));
                    return Ok(());
                }
                let msg = decode_message(bytes).map_err(PeerError::Control)?;
                match &msg {
                    ControlMessage::EchoRequest { seq, .. } => {
                        let reply_seq = self.alloc_seq();
                        let reply = encode_message(&echo_response(
                            reply_seq,
                            i64::try_from(now.as_millis()).unwrap_or(i64::MAX),
                            *seq,
                            self.host_features,
                        ));
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
                        }
                    }
                    _ => {}
                }
                self.events.push_back(PeerEvent::Control(Box::new(msg)));
                Ok(())
            }
            other => {
                self.events.push_back(PeerEvent::RawChannelData {
                    channel: other,
                    data: bytes.to_vec(),
                });
                Ok(())
            }
        }
    }

    /// Ends the peer session and queues teardown.
    pub fn release(&mut self) {
        self.state = PeerState::Closed;
        self.pending_echo = None;
        if let Some(ice_id) = self.ice_id.take() {
            self.transmits.push_back(PeerTransmit::ClosePeer { ice_id });
        }
    }
}

impl<R: RngCore> Machine for PeerMachine<R> {
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
    use super::*;

    struct DeterministicRng(u64);

    impl RngCore for DeterministicRng {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for chunk in dest.chunks_mut(8) {
                let bytes = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
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

        // Echo response clears pending timeout
        let resp = encode_message(&echo_response(1, 200, probe_seq, Some(ff)));
        peer.handle_input(t1 + Duration::from_millis(20), &resp)
            .unwrap();
        assert_eq!(peer.poll_timeout(), None);
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
}
