//! High-level endpoint role sessions combining signaling, WebRTC peers, and [`crate::platform`].

pub mod host;
pub mod viewer;

pub use host::{
    AuthorizationDecision, AuthorizationRequest, HostAuthorizer, HostConfig, HostSession, HostStats,
};
pub use viewer::{ViewerConfig, ViewerSession, ViewerStats};
