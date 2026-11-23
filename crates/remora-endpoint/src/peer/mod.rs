//! WebRTC peer transport (`str0m`), TURN/STUN ICE, and data-channel/media codecs.

pub mod channels;
mod governor;
pub mod turn;
pub mod webrtc;

pub use channels::{ChannelError, ChannelRegistry, DEFAULT_DATA_CHANNELS};
pub use governor::{GovernorConfig, GovernorDecision, GovernorSample, VideoGovernor};
pub use turn::{TurnAllocation, TurnError, stun_binding_address};
pub use webrtc::{
    ChannelSignalingRelay, PeerEventReceiver, PeerHandle, PeerSender, PeerSessionConfig,
    PeerSessionError, PeerSessionEvent, PeerSignalingOut, PeerStats, SignalingRelay,
};
