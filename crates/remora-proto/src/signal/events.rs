//! Socket.IO signaling event names and opaque relay payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Event names exchanged on the signaling Socket.IO namespace.
pub mod names {
    pub const ROOM_INFO: &str = "room_info";
    pub const CONTROL: &str = "control";
    pub const BE_CONTROLLED: &str = "be-controlled";
    pub const SOAC: &str = "soac";
    pub const RELEASED: &str = "released";
    pub const LEFT: &str = "left";
    pub const LEAVE: &str = "leave";
    pub const BMSG_PUSH: &str = "bmsg_push";
    pub const FORWARD_SETTING: &str = "forward_setting";
    pub const ICE_FINISHED_LOG: &str = "ice_finished_log";
    pub const REFRESH_RECONNECT_KEY: &str = "refresh_reconnect_key";
}

/// Opaque SDP/ICE signaling payload relayed between viewer and host by `ice_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoacPayload {
    pub ice_id: String,
    #[serde(flatten)]
    pub body: Value,
}
