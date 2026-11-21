//! In-process [`Bus`] implementation backed by Tokio MPSC channels.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use tokio::sync::mpsc;

use super::{Bus, BusMessage, Subscription};

const CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Default)]
pub struct LocalBus {
    topics: Mutex<HashMap<String, Vec<mpsc::Sender<BusMessage>>>>,
}

impl LocalBus {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Bus for LocalBus {
    fn publish(
        &self,
        topic: &str,
        message: BusMessage,
    ) -> Pin<Box<dyn Future<Output = usize> + Send + '_>> {
        let senders = {
            let mut topics = self.topics.lock().expect("local bus lock poisoned");
            let Some(list) = topics.get_mut(topic) else {
                return Box::pin(std::future::ready(0));
            };
            list.retain(|tx| !tx.is_closed());
            if list.is_empty() {
                topics.remove(topic);
                return Box::pin(std::future::ready(0));
            }
            list.clone()
        };
        Box::pin(async move {
            let mut delivered = 0;
            let mut message = Some(message);
            let last = senders.len().saturating_sub(1);
            for (index, tx) in senders.into_iter().enumerate() {
                let msg = if index == last {
                    message.take().expect("last iteration")
                } else {
                    message.as_ref().expect("not last iteration").clone()
                };
                if tx.send(msg).await.is_ok() {
                    delivered += 1;
                }
            }
            delivered
        })
    }

    fn subscribe(&self, topic: &str) -> Subscription {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let mut topics = self.topics.lock().expect("local bus lock poisoned");
        let list = topics.entry(topic.to_owned()).or_default();
        list.retain(|existing| !existing.is_closed());
        list.push(tx);
        Subscription::new(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::token::Role;
    use crate::bus::{PresenceChanged, TOPIC_PRESENCE, TOPIC_ROOM_COMMAND};

    fn presence(device_id: &str, online: bool) -> BusMessage {
        BusMessage::PresenceChanged(PresenceChanged {
            device_id: device_id.to_owned(),
            client_id: "c1".to_owned(),
            role: Role::Host,
            online,
        })
    }

    #[tokio::test]
    async fn delivers_to_all_topic_subscribers_only() {
        let bus = LocalBus::new();
        assert_eq!(bus.publish(TOPIC_PRESENCE, presence("d1", true)).await, 0);

        let mut s1 = bus.subscribe(TOPIC_PRESENCE);
        let mut s2 = bus.subscribe(TOPIC_PRESENCE);
        let mut other = bus.subscribe(TOPIC_ROOM_COMMAND);

        assert_eq!(bus.publish(TOPIC_PRESENCE, presence("d1", true)).await, 2);

        let m1 = s1.recv().await.unwrap();
        let m2 = s2.recv().await.unwrap();
        assert!(matches!(m1, BusMessage::PresenceChanged(p) if p.device_id == "d1" && p.online));
        assert!(matches!(m2, BusMessage::PresenceChanged(p) if p.device_id == "d1" && p.online));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), other.recv())
                .await
                .is_err()
        );

        drop(s2);
        assert_eq!(bus.publish(TOPIC_PRESENCE, presence("d1", false)).await, 1);
        let m1 = s1.recv().await.unwrap();
        assert!(matches!(m1, BusMessage::PresenceChanged(p) if !p.online));
    }
}
