//! Authoritative room coordination (`RoomRegistry` / `RoomActor`) and the bus-backed
//! [`RoomClient`] used by control-plane handlers to query and control rooms.

pub(crate) mod registry;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::bus::{ActiveParticipant, Bus, BusMessage, Reply, RoomCommand, TOPIC_ROOM_COMMAND};

/// How long the sessions of a host that left signaling wait for it to reconnect before they
/// end. Hosts wait a few seconds and ask for a new room before reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectGrace(pub Duration);

impl Default for ReconnectGrace {
    fn default() -> Self {
        Self(Duration::from_secs(30))
    }
}

/// Bus-backed handle used by control-plane handlers to query or instruct room actors without
/// holding direct references to room state.
#[derive(Clone)]
pub struct RoomClient {
    bus: Arc<dyn Bus>,
}

impl RoomClient {
    pub fn new(bus: Arc<dyn Bus>) -> Self {
        Self { bus }
    }

    pub async fn is_online(&self, device_id: &str) -> bool {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::IsOnline {
                    device_id: device_id.to_owned(),
                    reply,
                }),
            )
            .await;
        if delivered == 0 {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn online_devices(&self, device_ids: Vec<String>) -> HashSet<String> {
        if device_ids.is_empty() {
            return HashSet::new();
        }
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::OnlineDevices { device_ids, reply }),
            )
            .await;
        if delivered == 0 {
            return HashSet::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn desktop_participants(
        &self,
        device_ids: Vec<String>,
    ) -> HashMap<String, Vec<ActiveParticipant>> {
        if device_ids.is_empty() {
            return HashMap::new();
        }
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::DesktopParticipants { device_ids, reply }),
            )
            .await;
        if delivered == 0 {
            return HashMap::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn controlled_elsewhere(&self, device_id: &str, from_device: Option<&str>) -> bool {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::ControlledElsewhere {
                    device_id: device_id.to_owned(),
                    from_device: from_device.map(str::to_owned),
                    reply,
                }),
            )
            .await;
        if delivered == 0 {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn kick_host(&self, device_id: &str) {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::KickHost {
                    device_id: device_id.to_owned(),
                    reply,
                }),
            )
            .await;
        if delivered > 0 {
            let _ = rx.await;
        }
    }

    pub async fn kick_shared_viewers(&self, device_id: &str) {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::KickSharedViewers {
                    device_id: device_id.to_owned(),
                    reply,
                }),
            )
            .await;
        if delivered > 0 {
            let _ = rx.await;
        }
    }

    pub async fn push_to_host(&self, device_id: &str, name: &str, body: Value) -> bool {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::PushToHost {
                    device_id: device_id.to_owned(),
                    name: name.to_owned(),
                    body,
                    reply,
                }),
            )
            .await;
        if delivered == 0 {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}
