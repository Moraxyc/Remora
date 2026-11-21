//! Engine.IO v4 handshake and heartbeat constants.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const PING: &str = "2";
pub const PONG: &str = "3";

/// Largest message a server advertises it accepts, in bytes.
pub const MAX_PAYLOAD: u64 = 1_000_000;

/// Server handshake, the `0{…}` open packet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    pub sid: String,
    /// Milliseconds between server pings.
    pub ping_interval: u64,
    /// Milliseconds the server waits for a pong.
    pub ping_timeout: u64,
}

impl Handshake {
    /// The open packet a server sends: WebSocket only, so no upgrades.
    pub fn encode(&self) -> String {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Open<'a> {
            sid: &'a str,
            upgrades: [&'a str; 0],
            ping_interval: u64,
            ping_timeout: u64,
            max_payload: u64,
        }
        let open = Open {
            sid: &self.sid,
            upgrades: [],
            ping_interval: self.ping_interval,
            ping_timeout: self.ping_timeout,
            max_payload: MAX_PAYLOAD,
        };
        format!(
            "0{}",
            serde_json::to_string(&open).expect("open packet serializes")
        )
    }

    /// How long without a server ping before the connection counts as dead.
    pub fn liveness_timeout(&self) -> Duration {
        let ms = self.ping_interval.saturating_add(self.ping_timeout);
        Duration::from_millis(ms).clamp(Duration::from_secs(5), Duration::from_secs(300))
    }
}
