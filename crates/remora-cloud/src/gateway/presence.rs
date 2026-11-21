//! Local gateway connection registry, outbound bus delivery, and online presence tracking.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use remora_proto::signal::{encode_ack, encode_event};
use tokio::sync::mpsc;

use crate::api::auth::token::Role;
use crate::bus::{
    Bus, BusMessage, EndpointEvent, EndpointFrame, TOPIC_PRESENCE, TOPIC_TO_ENDPOINT,
};

#[derive(Debug, PartialEq)]
pub enum Outbound {
    Frames(Vec<EndpointFrame>),
    /// The connection lost the room: another took over the device, or the device was unbound.
    Kicked,
}

/// Active WebSocket connections hosted by this gateway instance, keyed by `client_id`.
#[derive(Debug, Default, Clone)]
pub struct Connections(Arc<Mutex<HashMap<String, mpsc::Sender<Outbound>>>>);

impl Connections {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, client_id: String, outbox: mpsc::Sender<Outbound>) {
        self.0
            .lock()
            .expect("connections lock poisoned")
            .insert(client_id, outbox);
    }

    pub fn remove(&self, client_id: &str) {
        self.0
            .lock()
            .expect("connections lock poisoned")
            .remove(client_id);
    }

    fn dispatch(&self, client_id: &str, outbound: Outbound) {
        let sender = self
            .0
            .lock()
            .expect("connections lock poisoned")
            .get(client_id)
            .cloned();
        if let Some(tx) = sender {
            // A full outbox means the connection stopped draining and will time out on heartbeat.
            let _ = tx.try_send(outbound);
        }
    }
}

/// Spawns the outbound router task, subscribing to [`TOPIC_TO_ENDPOINT`] synchronously before
/// returning.
pub fn spawn_outbound(bus: Arc<dyn Bus>, connections: Connections) -> tokio::task::JoinHandle<()> {
    let mut sub = bus.subscribe(TOPIC_TO_ENDPOINT);
    tokio::spawn(async move {
        while let Some(msg) = sub.recv().await {
            let BusMessage::ToEndpoint(to) = msg else {
                continue;
            };
            match to.event {
                EndpointEvent::Frames { client_id, frames } => {
                    connections.dispatch(&client_id, Outbound::Frames(frames));
                }
                EndpointEvent::Event {
                    client_id,
                    name,
                    body,
                } => {
                    let frame = EndpointFrame::Text(encode_event(&name, &[body], None));
                    connections.dispatch(&client_id, Outbound::Frames(vec![frame]));
                }
                EndpointEvent::Ack {
                    client_id,
                    ack_id,
                    args,
                } => {
                    let frame = EndpointFrame::Text(encode_ack(ack_id, &args));
                    connections.dispatch(&client_id, Outbound::Frames(vec![frame]));
                }
                EndpointEvent::Kick { client_id } => {
                    connections.dispatch(&client_id, Outbound::Kicked);
                }
            }
        }
    })
}

/// Tracks which host `client_id` is currently online per `device_id` from [`TOPIC_PRESENCE`].
#[derive(Debug, Default, Clone)]
pub struct PresenceTracker(Arc<Mutex<HashMap<String, String>>>);

#[allow(dead_code, reason = "available to presence subscribers and tests")]
impl PresenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        self.0
            .lock()
            .expect("presence lock poisoned")
            .contains_key(device_id)
    }

    pub fn spawn(self, bus: Arc<dyn Bus>) -> tokio::task::JoinHandle<()> {
        let mut sub = bus.subscribe(TOPIC_PRESENCE);
        tokio::spawn(async move {
            while let Some(msg) = sub.recv().await {
                let BusMessage::PresenceChanged(change) = msg else {
                    continue;
                };
                if change.role != Role::Host {
                    continue;
                }
                let mut hosts = self.0.lock().expect("presence lock poisoned");
                if change.online {
                    hosts.insert(change.device_id, change.client_id);
                } else if hosts.get(&change.device_id) == Some(&change.client_id) {
                    hosts.remove(&change.device_id);
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{LocalBus, PresenceChanged};

    #[tokio::test]
    async fn presence_tracker_ignores_superseded_host_disconnect() {
        let bus = Arc::new(LocalBus::new());
        let tracker = PresenceTracker::new();
        tracker.clone().spawn(bus.clone());

        bus.publish(
            TOPIC_PRESENCE,
            BusMessage::PresenceChanged(PresenceChanged {
                device_id: "d1".into(),
                client_id: "c1".into(),
                role: Role::Host,
                online: true,
            }),
        )
        .await;
        tokio::task::yield_now().await;
        assert!(tracker.is_online("d1"));

        bus.publish(
            TOPIC_PRESENCE,
            BusMessage::PresenceChanged(PresenceChanged {
                device_id: "d1".into(),
                client_id: "c2".into(),
                role: Role::Host,
                online: true,
            }),
        )
        .await;
        // Stale disconnect from c1 must not mark d1 offline.
        bus.publish(
            TOPIC_PRESENCE,
            BusMessage::PresenceChanged(PresenceChanged {
                device_id: "d1".into(),
                client_id: "c1".into(),
                role: Role::Host,
                online: false,
            }),
        )
        .await;
        tokio::task::yield_now().await;
        assert!(tracker.is_online("d1"));

        bus.publish(
            TOPIC_PRESENCE,
            BusMessage::PresenceChanged(PresenceChanged {
                device_id: "d1".into(),
                client_id: "c2".into(),
                role: Role::Host,
                online: false,
            }),
        )
        .await;
        tokio::task::yield_now().await;
        assert!(!tracker.is_online("d1"));
    }
}
