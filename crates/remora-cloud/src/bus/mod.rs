//! Internal message bus between cloud roles (`api`, `gateway`, `room`, `relay`).
//!
//! All bus message types stay internal to `remora-cloud` and never enter `remora-proto`.

pub mod local;
pub mod unix;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use remora_proto::profile::Profile;
use remora_proto::signal::SignalEvent;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

pub use local::LocalBus;

use crate::api::auth::token::{Role, Room};
use crate::relay::ice::Grant;

/// Topic for messages delivered from rooms or API handlers to connected endpoints via gateways.
pub const TOPIC_TO_ENDPOINT: &str = "to_endpoint";

/// Topic for commands addressed to room actors.
pub const TOPIC_ROOM_COMMAND: &str = "room_command";

/// Topic for endpoint connection presence updates emitted by gateways.
pub const TOPIC_PRESENCE: &str = "presence";

/// Topic for ICE/TURN credential grant requests handled by the `relay` role.
pub const TOPIC_RELAY: &str = "relay";

/// One-shot reply handle carried inside request-style bus commands.
pub struct Reply<T>(Arc<Mutex<Option<oneshot::Sender<T>>>>);

impl<T> Clone for Reply<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reply(..)")
    }
}

impl<T> Reply<T> {
    pub fn channel() -> (Self, oneshot::Receiver<T>) {
        let (tx, rx) = oneshot::channel();
        (Self(Arc::new(Mutex::new(Some(tx)))), rx)
    }

    pub fn send(&self, value: T) {
        if let Some(tx) = self.0.lock().expect("reply lock poisoned").take() {
            let _ = tx.send(value);
        }
    }
}

/// Messages carried across the cloud-internal bus.
#[derive(Debug, Clone)]
pub enum BusMessage {
    ToEndpoint(ToEndpoint),
    RoomCommand(RoomCommand),
    PresenceChanged(PresenceChanged),
    RelayCommand(RelayCommand),
}

/// Outbound delivery from a room or API handler to a connected endpoint on a gateway.
#[derive(Debug, Clone, PartialEq)]
pub struct ToEndpoint {
    pub device_id: String,
    pub event: EndpointEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndpointFrame {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndpointEvent {
    Frames {
        client_id: String,
        frames: Vec<EndpointFrame>,
    },
    Event {
        client_id: String,
        name: String,
        body: Value,
    },
    Ack {
        client_id: String,
        ack_id: u64,
        args: Vec<Value>,
    },
    Kick {
        client_id: String,
    },
}

/// Commands processed by room actors.
#[derive(Debug, Clone)]
pub enum RoomCommand {
    Join {
        room: Room,
        client_id: String,
        profile: Option<&'static Profile>,
        reply: Reply<()>,
    },
    Leave {
        room: Room,
        client_id: String,
    },
    EndpointEvent {
        room: Room,
        client_id: String,
        ack_id: Option<u64>,
        event: Box<SignalEvent>,
    },
    KickHost {
        device_id: String,
        reply: Reply<()>,
    },
    KickSharedViewers {
        device_id: String,
        reply: Reply<()>,
    },
    PushToHost {
        device_id: String,
        name: String,
        body: Value,
        reply: Reply<bool>,
    },
    IsOnline {
        device_id: String,
        reply: Reply<bool>,
    },
    OnlineDevices {
        device_ids: Vec<String>,
        reply: Reply<HashSet<String>>,
    },
    DesktopParticipants {
        device_ids: Vec<String>,
        reply: Reply<HashMap<String, Vec<ActiveParticipant>>>,
    },
    ControlledElsewhere {
        device_id: String,
        from_device: Option<String>,
        reply: Reply<bool>,
    },
}

/// Active desktop viewer session reported by a room actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveParticipant {
    pub client_id: String,
    pub from_device: Option<String>,
    pub controlled_secs: u64,
}

/// Presence transition published by a gateway when an endpoint connects or disconnects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceChanged {
    pub device_id: String,
    pub client_id: String,
    pub role: Role,
    pub online: bool,
}

/// Commands handled by the `relay` role.
#[derive(Debug, Clone)]
pub enum RelayCommand {
    Grant {
        user_id: String,
        reply: Reply<Grant>,
    },
}

/// Topic subscription stream.
pub struct Subscription {
    rx: mpsc::Receiver<BusMessage>,
}

impl Subscription {
    pub fn new(rx: mpsc::Receiver<BusMessage>) -> Self {
        Self { rx }
    }

    pub async fn recv(&mut self) -> Option<BusMessage> {
        self.rx.recv().await
    }
}

/// Topic-based publish/subscribe interface connecting cloud roles.
pub trait Bus: Send + Sync {
    /// Publishes `message` to `topic` and returns the number of active subscribers that received it.
    fn publish(
        &self,
        topic: &str,
        message: BusMessage,
    ) -> Pin<Box<dyn Future<Output = usize> + Send + '_>>;

    /// Subscribes to messages published on `topic`.
    fn subscribe(&self, topic: &str) -> Subscription;
}
