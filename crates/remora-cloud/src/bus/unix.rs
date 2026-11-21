//! Cross-process [`Bus`] implementation over a local Unix domain socket hub.
//!
//! One process binds the hub (`--bus-listen`), and other role-split processes connect to it
//! (`--bus-connect`). All `BusMessage` variants and one-shot `Reply<T>` channels are routed
//! transparently between local and remote subscribers.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use remora_proto::profile;
use remora_proto::signal::{Decoder, Packet, SignalEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use super::{
    ActiveParticipant, Bus, BusMessage, EndpointEvent, EndpointFrame, PresenceChanged,
    RelayCommand, Reply, RoomCommand, Subscription, ToEndpoint,
};
use crate::api::auth::token::{Role, Room};
use crate::relay::ice::Grant;

const CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireFrame {
    Subscribe {
        topic: String,
    },
    Publish {
        req_id: u64,
        topic: String,
        message: WireBusMessage,
    },
    Sync {
        req_id: u64,
    },
    Reply {
        publisher_client_id: u64,
        req_id: u64,
        payload: WireReply,
    },
    Deliver {
        topic: String,
        message: WireBusMessage,
        reply_to: Option<(u64, u64)>,
    },
    Published {
        req_id: u64,
        delivered: usize,
    },
    SyncAck {
        req_id: u64,
    },
    ReplyDeliver {
        req_id: u64,
        payload: WireReply,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireEndpointFrame {
    Text(String),
    Binary(Vec<u8>),
}

impl From<&EndpointFrame> for WireEndpointFrame {
    fn from(frame: &EndpointFrame) -> Self {
        match frame {
            EndpointFrame::Text(t) => Self::Text(t.clone()),
            EndpointFrame::Binary(b) => Self::Binary(b.clone()),
        }
    }
}

impl From<WireEndpointFrame> for EndpointFrame {
    fn from(frame: WireEndpointFrame) -> Self {
        match frame {
            WireEndpointFrame::Text(t) => Self::Text(t),
            WireEndpointFrame::Binary(b) => Self::Binary(b),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireEndpointEvent {
    Frames {
        client_id: String,
        frames: Vec<WireEndpointFrame>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireBusMessage {
    ToEndpoint {
        device_id: String,
        event: WireEndpointEvent,
    },
    PresenceChanged {
        device_id: String,
        client_id: String,
        role: Role,
        online: bool,
    },
    RoomJoin {
        room: Room,
        client_id: String,
        profile_id: Option<String>,
    },
    RoomLeave {
        room: Room,
        client_id: String,
    },
    RoomEndpointEvent {
        room: Room,
        client_id: String,
        ack_id: Option<u64>,
        text: String,
        attachments: Vec<Vec<u8>>,
    },
    RoomKickHost {
        device_id: String,
    },
    RoomPushToHost {
        device_id: String,
        name: String,
        body: Value,
    },
    RoomIsOnline {
        device_id: String,
    },
    RoomOnlineDevices {
        device_ids: Vec<String>,
    },
    RoomDesktopParticipants {
        device_ids: Vec<String>,
    },
    RoomControlledElsewhere {
        device_id: String,
        from_device: Option<String>,
    },
    RelayGrant {
        user_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireParticipant {
    client_id: String,
    from_device: Option<String>,
    controlled_secs: u64,
}

impl From<ActiveParticipant> for WireParticipant {
    fn from(p: ActiveParticipant) -> Self {
        Self {
            client_id: p.client_id,
            from_device: p.from_device,
            controlled_secs: p.controlled_secs,
        }
    }
}

impl From<WireParticipant> for ActiveParticipant {
    fn from(p: WireParticipant) -> Self {
        Self {
            client_id: p.client_id,
            from_device: p.from_device,
            controlled_secs: p.controlled_secs,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireReply {
    Unit,
    Bool(bool),
    Set(HashSet<String>),
    Participants(HashMap<String, Vec<WireParticipant>>),
    Grant(Grant),
    Closed,
}

enum PendingReply {
    Unit(Reply<()>),
    Bool(Reply<bool>),
    Set(Reply<HashSet<String>>),
    Participants(Reply<HashMap<String, Vec<ActiveParticipant>>>),
    Grant(Reply<Grant>),
}

impl PendingReply {
    fn resolve(self, payload: WireReply) {
        match (self, payload) {
            (Self::Unit(r), WireReply::Unit) => r.send(()),
            (Self::Bool(r), WireReply::Bool(b)) => r.send(b),
            (Self::Set(r), WireReply::Set(s)) => r.send(s),
            (Self::Participants(r), WireReply::Participants(map)) => {
                let converted = map
                    .into_iter()
                    .map(|(k, v)| (k, v.into_iter().map(Into::into).collect()))
                    .collect();
                r.send(converted);
            }
            (Self::Grant(r), WireReply::Grant(g)) => r.send(g),
            _ => {}
        }
    }
}

impl WireBusMessage {
    fn expects_reply(&self) -> bool {
        matches!(
            self,
            Self::RoomJoin { .. }
                | Self::RoomKickHost { .. }
                | Self::RoomPushToHost { .. }
                | Self::RoomIsOnline { .. }
                | Self::RoomOnlineDevices { .. }
                | Self::RoomDesktopParticipants { .. }
                | Self::RoomControlledElsewhere { .. }
                | Self::RelayGrant { .. }
        )
    }

    fn from_bus_message(msg: BusMessage) -> (Self, Option<PendingReply>) {
        match msg {
            BusMessage::ToEndpoint(ToEndpoint { device_id, event }) => {
                let wire_event = match event {
                    EndpointEvent::Frames { client_id, frames } => WireEndpointEvent::Frames {
                        client_id,
                        frames: frames.iter().map(WireEndpointFrame::from).collect(),
                    },
                    EndpointEvent::Event {
                        client_id,
                        name,
                        body,
                    } => WireEndpointEvent::Event {
                        client_id,
                        name,
                        body,
                    },
                    EndpointEvent::Ack {
                        client_id,
                        ack_id,
                        args,
                    } => WireEndpointEvent::Ack {
                        client_id,
                        ack_id,
                        args,
                    },
                    EndpointEvent::Kick { client_id } => WireEndpointEvent::Kick { client_id },
                };
                (
                    Self::ToEndpoint {
                        device_id,
                        event: wire_event,
                    },
                    None,
                )
            }
            BusMessage::PresenceChanged(PresenceChanged {
                device_id,
                client_id,
                role,
                online,
            }) => (
                Self::PresenceChanged {
                    device_id,
                    client_id,
                    role,
                    online,
                },
                None,
            ),
            BusMessage::RoomCommand(cmd) => match cmd {
                RoomCommand::Join {
                    room,
                    client_id,
                    profile,
                    reply,
                } => (
                    Self::RoomJoin {
                        room,
                        client_id,
                        profile_id: profile.map(|p| p.id.clone()),
                    },
                    Some(PendingReply::Unit(reply)),
                ),
                RoomCommand::Leave { room, client_id } => {
                    (Self::RoomLeave { room, client_id }, None)
                }
                RoomCommand::EndpointEvent {
                    room,
                    client_id,
                    ack_id,
                    event,
                } => {
                    let (text, attachments) = event.encode(ack_id);
                    (
                        Self::RoomEndpointEvent {
                            room,
                            client_id,
                            ack_id,
                            text,
                            attachments,
                        },
                        None,
                    )
                }
                RoomCommand::KickHost { device_id, reply } => (
                    Self::RoomKickHost { device_id },
                    Some(PendingReply::Unit(reply)),
                ),
                RoomCommand::PushToHost {
                    device_id,
                    name,
                    body,
                    reply,
                } => (
                    Self::RoomPushToHost {
                        device_id,
                        name,
                        body,
                    },
                    Some(PendingReply::Bool(reply)),
                ),
                RoomCommand::IsOnline { device_id, reply } => (
                    Self::RoomIsOnline { device_id },
                    Some(PendingReply::Bool(reply)),
                ),
                RoomCommand::OnlineDevices { device_ids, reply } => (
                    Self::RoomOnlineDevices { device_ids },
                    Some(PendingReply::Set(reply)),
                ),
                RoomCommand::DesktopParticipants { device_ids, reply } => (
                    Self::RoomDesktopParticipants { device_ids },
                    Some(PendingReply::Participants(reply)),
                ),
                RoomCommand::ControlledElsewhere {
                    device_id,
                    from_device,
                    reply,
                } => (
                    Self::RoomControlledElsewhere {
                        device_id,
                        from_device,
                    },
                    Some(PendingReply::Bool(reply)),
                ),
            },
            BusMessage::RelayCommand(RelayCommand::Grant { user_id, reply }) => (
                Self::RelayGrant { user_id },
                Some(PendingReply::Grant(reply)),
            ),
        }
    }

    fn into_bus_message(
        self,
        reply_to: Option<(u64, u64)>,
        out_tx: &mpsc::Sender<WireFrame>,
    ) -> Option<BusMessage> {
        Some(match self {
            Self::ToEndpoint { device_id, event } => BusMessage::ToEndpoint(ToEndpoint {
                device_id,
                event: match event {
                    WireEndpointEvent::Frames { client_id, frames } => EndpointEvent::Frames {
                        client_id,
                        frames: frames.into_iter().map(Into::into).collect(),
                    },
                    WireEndpointEvent::Event {
                        client_id,
                        name,
                        body,
                    } => EndpointEvent::Event {
                        client_id,
                        name,
                        body,
                    },
                    WireEndpointEvent::Ack {
                        client_id,
                        ack_id,
                        args,
                    } => EndpointEvent::Ack {
                        client_id,
                        ack_id,
                        args,
                    },
                    WireEndpointEvent::Kick { client_id } => EndpointEvent::Kick { client_id },
                },
            }),
            Self::PresenceChanged {
                device_id,
                client_id,
                role,
                online,
            } => BusMessage::PresenceChanged(PresenceChanged {
                device_id,
                client_id,
                role,
                online,
            }),
            Self::RoomJoin {
                room,
                client_id,
                profile_id,
            } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(reply_to, out_tx.clone(), rx, |()| WireReply::Unit);
                BusMessage::RoomCommand(RoomCommand::Join {
                    room,
                    client_id,
                    profile: profile_id.as_deref().and_then(profile::by_id),
                    reply,
                })
            }
            Self::RoomLeave { room, client_id } => {
                BusMessage::RoomCommand(RoomCommand::Leave { room, client_id })
            }
            Self::RoomEndpointEvent {
                room,
                client_id,
                ack_id,
                text,
                attachments,
            } => {
                let event = decode_signal_event(&text, &attachments)?;
                BusMessage::RoomCommand(RoomCommand::EndpointEvent {
                    room,
                    client_id,
                    ack_id,
                    event: Box::new(event),
                })
            }
            Self::RoomKickHost { device_id } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(reply_to, out_tx.clone(), rx, |()| WireReply::Unit);
                BusMessage::RoomCommand(RoomCommand::KickHost { device_id, reply })
            }
            Self::RoomPushToHost {
                device_id,
                name,
                body,
            } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(reply_to, out_tx.clone(), rx, WireReply::Bool);
                BusMessage::RoomCommand(RoomCommand::PushToHost {
                    device_id,
                    name,
                    body,
                    reply,
                })
            }
            Self::RoomIsOnline { device_id } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(reply_to, out_tx.clone(), rx, WireReply::Bool);
                BusMessage::RoomCommand(RoomCommand::IsOnline { device_id, reply })
            }
            Self::RoomOnlineDevices { device_ids } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(reply_to, out_tx.clone(), rx, WireReply::Set);
                BusMessage::RoomCommand(RoomCommand::OnlineDevices { device_ids, reply })
            }
            Self::RoomDesktopParticipants { device_ids } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(
                    reply_to,
                    out_tx.clone(),
                    rx,
                    |map: HashMap<String, Vec<ActiveParticipant>>| {
                        WireReply::Participants(
                            map.into_iter()
                                .map(|(k, v)| (k, v.into_iter().map(Into::into).collect()))
                                .collect(),
                        )
                    },
                );
                BusMessage::RoomCommand(RoomCommand::DesktopParticipants { device_ids, reply })
            }
            Self::RoomControlledElsewhere {
                device_id,
                from_device,
            } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(reply_to, out_tx.clone(), rx, WireReply::Bool);
                BusMessage::RoomCommand(RoomCommand::ControlledElsewhere {
                    device_id,
                    from_device,
                    reply,
                })
            }
            Self::RelayGrant { user_id } => {
                let (reply, rx) = Reply::channel();
                bridge_reply(reply_to, out_tx.clone(), rx, WireReply::Grant);
                BusMessage::RelayCommand(RelayCommand::Grant { user_id, reply })
            }
        })
    }
}

fn decode_signal_event(text: &str, attachments: &[Vec<u8>]) -> Option<SignalEvent> {
    let mut decoder = Decoder::new();
    let mut packet = decoder.text(text).ok()?;
    for att in attachments {
        if packet.is_some() {
            break;
        }
        packet = decoder.binary(att).ok()?;
    }
    match packet? {
        Packet::Event(ev) => Some(SignalEvent::from_event(&ev)),
        _ => None,
    }
}

fn bridge_reply<T: Send + 'static>(
    reply_to: Option<(u64, u64)>,
    out_tx: mpsc::Sender<WireFrame>,
    rx: oneshot::Receiver<T>,
    wrap: impl FnOnce(T) -> WireReply + Send + 'static,
) {
    let Some((publisher_client_id, req_id)) = reply_to else {
        return;
    };
    tokio::spawn(async move {
        let payload = match rx.await {
            Ok(val) => wrap(val),
            Err(_) => WireReply::Closed,
        };
        let _ = out_tx
            .send(WireFrame::Reply {
                publisher_client_id,
                req_id,
                payload,
            })
            .await;
    });
}

enum HubControl {
    Register {
        client_id: u64,
        to_client: mpsc::Sender<WireFrame>,
    },
    Unregister {
        client_id: u64,
    },
    FromClient {
        client_id: u64,
        frame: Box<WireFrame>,
    },
}

struct HubState {
    clients: HashMap<u64, mpsc::Sender<WireFrame>>,
    topics: HashMap<String, HashSet<u64>>,
    pending_replies: HashMap<(u64, u64), usize>,
}

impl HubState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            topics: HashMap::new(),
            pending_replies: HashMap::new(),
        }
    }

    async fn handle(&mut self, msg: HubControl) {
        match msg {
            HubControl::Register {
                client_id,
                to_client,
            } => {
                self.clients.insert(client_id, to_client);
            }
            HubControl::Unregister { client_id } => {
                self.clients.remove(&client_id);
                self.topics.retain(|_, subs| {
                    subs.remove(&client_id);
                    !subs.is_empty()
                });
            }
            HubControl::FromClient { client_id, frame } => match *frame {
                WireFrame::Subscribe { topic } => {
                    self.topics.entry(topic).or_default().insert(client_id);
                }
                WireFrame::Sync { req_id } => {
                    if let Some(tx) = self.clients.get(&client_id) {
                        let _ = tx.send(WireFrame::SyncAck { req_id }).await;
                    }
                }
                WireFrame::Publish {
                    req_id,
                    topic,
                    message,
                } => {
                    let sub_ids: Vec<u64> = self
                        .topics
                        .get(&topic)
                        .map(|s| s.iter().copied().collect())
                        .unwrap_or_default();
                    let reply_to = if message.expects_reply() && !sub_ids.is_empty() {
                        Some((client_id, req_id))
                    } else {
                        None
                    };
                    let mut delivered = 0usize;
                    for sub_id in sub_ids {
                        if let Some(tx) = self.clients.get(&sub_id)
                            && tx
                                .send(WireFrame::Deliver {
                                    topic: topic.clone(),
                                    message: message.clone(),
                                    reply_to,
                                })
                                .await
                                .is_ok()
                        {
                            delivered += 1;
                        }
                    }
                    if delivered > 0 && reply_to.is_some() {
                        self.pending_replies.insert((client_id, req_id), delivered);
                    }
                    if let Some(tx) = self.clients.get(&client_id) {
                        let _ = tx.send(WireFrame::Published { req_id, delivered }).await;
                    }
                }
                WireFrame::Reply {
                    publisher_client_id,
                    req_id,
                    payload,
                } => {
                    let key = (publisher_client_id, req_id);
                    let Some(remaining) = self.pending_replies.get_mut(&key) else {
                        return;
                    };
                    match payload {
                        WireReply::Closed => {
                            *remaining = remaining.saturating_sub(1);
                            if *remaining == 0 {
                                self.pending_replies.remove(&key);
                            }
                        }
                        concrete => {
                            self.pending_replies.remove(&key);
                            if let Some(tx) = self.clients.get(&publisher_client_id) {
                                let _ = tx
                                    .send(WireFrame::ReplyDeliver {
                                        req_id,
                                        payload: concrete,
                                    })
                                    .await;
                            }
                        }
                    }
                }
                _ => {}
            },
        }
    }
}

/// Starts a Unix-socket bus hub listening at `path`, returning an in-process [`UnixBus`] client
/// for the host process.
pub fn bind_hub(path: &Path) -> io::Result<Arc<UnixBus>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(path)?;
    let (hub_tx, mut hub_rx) = mpsc::channel::<HubControl>(CHANNEL_CAPACITY);
    let next_client_id = Arc::new(AtomicU64::new(1));

    tokio::spawn(async move {
        let mut state = HubState::new();
        while let Some(msg) = hub_rx.recv().await {
            state.handle(msg).await;
        }
    });

    let hub_tx_accept = hub_tx.clone();
    let next_id_accept = Arc::clone(&next_client_id);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let client_id = next_id_accept.fetch_add(1, Ordering::Relaxed);
            let hub_tx = hub_tx_accept.clone();
            tokio::spawn(serve_hub_connection(client_id, stream, hub_tx));
        }
    });

    // Register local in-process client with the hub.
    let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);
    let (to_client_tx, mut to_client_rx) = mpsc::channel::<WireFrame>(CHANNEL_CAPACITY);
    let (from_client_tx, mut from_client_rx) = mpsc::channel::<WireFrame>(CHANNEL_CAPACITY);
    let hub_tx_local = hub_tx.clone();
    tokio::spawn(async move {
        if hub_tx_local
            .send(HubControl::Register {
                client_id,
                to_client: to_client_tx,
            })
            .await
            .is_err()
        {
            return;
        }
        while let Some(frame) = from_client_rx.recv().await {
            if hub_tx_local
                .send(HubControl::FromClient {
                    client_id,
                    frame: Box::new(frame),
                })
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = hub_tx_local
            .send(HubControl::Unregister { client_id })
            .await;
    });

    let bus = Arc::new(UnixBus::new_with_transport(from_client_tx));
    let bus_inbound = Arc::clone(&bus);
    tokio::spawn(async move {
        while let Some(frame) = to_client_rx.recv().await {
            bus_inbound.handle_inbound(frame);
        }
    });
    Ok(bus)
}

async fn serve_hub_connection(
    client_id: u64,
    stream: UnixStream,
    hub_tx: mpsc::Sender<HubControl>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let (to_client_tx, mut to_client_rx) = mpsc::channel::<WireFrame>(CHANNEL_CAPACITY);
    if hub_tx
        .send(HubControl::Register {
            client_id,
            to_client: to_client_tx,
        })
        .await
        .is_err()
    {
        return;
    }

    let writer = tokio::spawn(async move {
        while let Some(frame) = to_client_rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&frame) else {
                continue;
            };
            line.push(b'\n');
            if write_half.write_all(&line).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(frame) = serde_json::from_str::<WireFrame>(&line)
            && hub_tx
                .send(HubControl::FromClient {
                    client_id,
                    frame: Box::new(frame),
                })
                .await
                .is_err()
        {
            break;
        }
    }

    writer.abort();
    let _ = hub_tx.send(HubControl::Unregister { client_id }).await;
}

/// Bus client connected to a [`bind_hub`] Unix domain socket (either locally or remotely).
pub struct UnixBus {
    out_tx: mpsc::Sender<WireFrame>,
    next_req_id: AtomicU64,
    topics: Mutex<HashMap<String, Vec<mpsc::Sender<BusMessage>>>>,
    pending_publishes: Mutex<HashMap<u64, oneshot::Sender<usize>>>,
    pending_syncs: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    pending_replies: Mutex<HashMap<u64, PendingReply>>,
}

impl UnixBus {
    fn new_with_transport(out_tx: mpsc::Sender<WireFrame>) -> Self {
        Self {
            out_tx,
            next_req_id: AtomicU64::new(1),
            topics: Mutex::new(HashMap::new()),
            pending_publishes: Mutex::new(HashMap::new()),
            pending_syncs: Mutex::new(HashMap::new()),
            pending_replies: Mutex::new(HashMap::new()),
        }
    }

    /// Connects to an existing Unix-socket bus hub at `path`.
    pub async fn connect(path: &Path) -> io::Result<Arc<Self>> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, mut write_half) = stream.into_split();
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(CHANNEL_CAPACITY);
        let bus = Arc::new(Self::new_with_transport(out_tx));

        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                let Ok(mut line) = serde_json::to_vec(&frame) else {
                    continue;
                };
                line.push(b'\n');
                if write_half.write_all(&line).await.is_err() {
                    break;
                }
            }
        });

        let bus_reader = Arc::clone(&bus);
        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(frame) = serde_json::from_str::<WireFrame>(&line) {
                    bus_reader.handle_inbound(frame);
                }
            }
        });

        Ok(bus)
    }

    /// Waits until the hub has processed all prior `subscribe` calls from this client.
    pub async fn sync(&self) {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_syncs
            .lock()
            .expect("sync lock poisoned")
            .insert(req_id, tx);
        if self.out_tx.send(WireFrame::Sync { req_id }).await.is_ok() {
            let _ = rx.await;
        } else {
            self.pending_syncs
                .lock()
                .expect("sync lock poisoned")
                .remove(&req_id);
        }
    }

    fn handle_inbound(&self, frame: WireFrame) {
        match frame {
            WireFrame::Published { req_id, delivered } => {
                if let Some(tx) = self
                    .pending_publishes
                    .lock()
                    .expect("publish lock poisoned")
                    .remove(&req_id)
                {
                    let _ = tx.send(delivered);
                }
            }
            WireFrame::SyncAck { req_id } => {
                if let Some(tx) = self
                    .pending_syncs
                    .lock()
                    .expect("sync lock poisoned")
                    .remove(&req_id)
                {
                    let _ = tx.send(());
                }
            }
            WireFrame::ReplyDeliver { req_id, payload } => {
                if let Some(pending) = self
                    .pending_replies
                    .lock()
                    .expect("reply lock poisoned")
                    .remove(&req_id)
                {
                    pending.resolve(payload);
                }
            }
            WireFrame::Deliver {
                topic,
                message,
                reply_to,
            } => {
                let Some(bus_msg) = message.into_bus_message(reply_to, &self.out_tx) else {
                    return;
                };
                let senders = {
                    let mut topics = self.topics.lock().expect("topics lock poisoned");
                    let Some(list) = topics.get_mut(&topic) else {
                        return;
                    };
                    list.retain(|tx| !tx.is_closed());
                    if list.is_empty() {
                        topics.remove(&topic);
                        return;
                    }
                    list.clone()
                };
                let mut msg_opt = Some(bus_msg);
                let last = senders.len().saturating_sub(1);
                for (idx, tx) in senders.into_iter().enumerate() {
                    let msg = if idx == last {
                        msg_opt.take().expect("last iteration")
                    } else {
                        msg_opt.as_ref().expect("not last").clone()
                    };
                    let _ = tx.try_send(msg);
                }
            }
            _ => {}
        }
    }
}

impl Bus for UnixBus {
    fn publish(
        &self,
        topic: &str,
        message: BusMessage,
    ) -> Pin<Box<dyn Future<Output = usize> + Send + '_>> {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (wire_msg, pending_reply) = WireBusMessage::from_bus_message(message);
        if let Some(pending) = pending_reply {
            self.pending_replies
                .lock()
                .expect("reply lock poisoned")
                .insert(req_id, pending);
        }
        let topic = topic.to_owned();
        let (tx, rx) = oneshot::channel();
        self.pending_publishes
            .lock()
            .expect("publish lock poisoned")
            .insert(req_id, tx);

        Box::pin(async move {
            if self
                .out_tx
                .send(WireFrame::Publish {
                    req_id,
                    topic,
                    message: wire_msg,
                })
                .await
                .is_err()
            {
                self.pending_publishes
                    .lock()
                    .expect("publish lock poisoned")
                    .remove(&req_id);
                self.pending_replies
                    .lock()
                    .expect("reply lock poisoned")
                    .remove(&req_id);
                return 0;
            }
            let delivered = rx.await.unwrap_or(0);
            if delivered == 0 {
                self.pending_replies
                    .lock()
                    .expect("reply lock poisoned")
                    .remove(&req_id);
            }
            delivered
        })
    }

    fn subscribe(&self, topic: &str) -> Subscription {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        {
            let mut topics = self.topics.lock().expect("topics lock poisoned");
            let list = topics.entry(topic.to_owned()).or_default();
            list.retain(|existing| !existing.is_closed());
            list.push(tx);
        }
        let _ = self.out_tx.try_send(WireFrame::Subscribe {
            topic: topic.to_owned(),
        });
        Subscription::new(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Reply, TOPIC_RELAY, TOPIC_ROOM_COMMAND};

    #[tokio::test]
    async fn hub_and_remote_client_route_messages_and_replies() {
        let dir = std::env::temp_dir().join(format!("remora-unix-bus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("bus.sock");

        let hub_bus = bind_hub(&sock_path).unwrap();
        let mut relay_sub = hub_bus.subscribe(TOPIC_RELAY);
        hub_bus.sync().await;

        let remote_bus = UnixBus::connect(&sock_path).await.unwrap();
        let mut room_sub = remote_bus.subscribe(TOPIC_ROOM_COMMAND);
        remote_bus.sync().await;

        tokio::spawn(async move {
            if let Some(BusMessage::RelayCommand(RelayCommand::Grant { user_id, reply })) =
                relay_sub.recv().await
            {
                assert_eq!(user_id, "u1");
                reply.send(Grant {
                    servers: serde_json::json!(["turn:test:3478"]),
                    force_relay: true,
                });
            }
        });

        tokio::spawn(async move {
            if let Some(BusMessage::RoomCommand(RoomCommand::IsOnline { device_id, reply })) =
                room_sub.recv().await
            {
                reply.send(device_id == "dev-1");
            }
        });

        // Hub queries remote room subscriber.
        let (reply, rx) = Reply::channel();
        let delivered = hub_bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::IsOnline {
                    device_id: "dev-1".into(),
                    reply,
                }),
            )
            .await;
        assert_eq!(delivered, 1);
        assert_eq!(rx.await, Ok(true));

        // Remote queries hub relay subscriber.
        let (reply, rx) = Reply::channel();
        let delivered = remote_bus
            .publish(
                TOPIC_RELAY,
                BusMessage::RelayCommand(RelayCommand::Grant {
                    user_id: "u1".into(),
                    reply,
                }),
            )
            .await;
        assert_eq!(delivered, 1);
        let grant = rx.await.unwrap();
        assert!(grant.force_relay);
        assert_eq!(grant.servers, serde_json::json!(["turn:test:3478"]));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
