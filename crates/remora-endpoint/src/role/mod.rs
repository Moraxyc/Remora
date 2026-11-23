//! High-level endpoint role sessions combining signaling, WebRTC peers, and [`crate::platform`].

use remora_proto::session::file::FileMessage;
use remora_proto::session::system::SystemMessage;
use remora_proto::session::terminal::{TermFrame, TerminalControlMessage};
use remora_proto::session::tunnel::TunnelFrame;

pub mod host;
pub mod viewer;

/// Inbound tool-channel or system event forwarded by [`HostSession`] or [`ViewerSession`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ToolEvent {
    File(Box<FileMessage>),
    Tunnel(TunnelFrame),
    TermFrame(TermFrame),
    TerminalControl(TerminalControlMessage),
    System(Box<SystemMessage>),
}

/// Inbound [`ToolEvent`] annotated with host-side peer routing metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct HostToolEvent {
    pub ice_id: String,
    pub controller_platform: Option<u8>,
    pub event: ToolEvent,
}

pub use host::{
    AuthorizationDecision, AuthorizationRequest, HostAuthorizer, HostConfig, HostSession,
    HostStats, HostToolSender,
};
pub use viewer::{ViewerConfig, ViewerSession, ViewerStats};
