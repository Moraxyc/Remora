//! Authoritative room state: one Tokio actor task per room (`device_id`), coordinated by a
//! registry that maps `room_id` (`device_id`) to its actor handle.
//!
//! All room state mutations happen strictly inside each room's actor task.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use prost::Message as _;
use remora_proto::pb::ConnectOptions;
use remora_proto::profile::Profile;
use remora_proto::signal::events::names;
use remora_proto::signal::{SignalEvent, encode_binary_event, placeholder};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

use crate::api::auth::token::{Role, Room, random};
use crate::bus::{
    ActiveParticipant, Bus, BusMessage, EndpointEvent, EndpointFrame, RelayCommand, Reply,
    RoomCommand, TOPIC_RELAY, TOPIC_ROOM_COMMAND, TOPIC_TO_ENDPOINT, ToEndpoint,
};
use crate::relay::ice::Grant;
use crate::room::ReconnectGrace;

/// Capacity of each room actor's command inbox.
const ACTOR_INBOX_CAPACITY: usize = 128;

/// Spawns the room registry loop, subscribing to [`TOPIC_ROOM_COMMAND`] synchronously before
/// returning so callers never race with task startup.
pub fn spawn(bus: Arc<dyn Bus>, reconnect_grace: ReconnectGrace) -> tokio::task::JoinHandle<()> {
    let mut sub = bus.subscribe(TOPIC_ROOM_COMMAND);
    tokio::spawn(async move {
        let mut registry = RoomRegistry::new(bus, reconnect_grace);
        loop {
            tokio::select! {
                msg = sub.recv() => {
                    let Some(msg) = msg else { break; };
                    if let BusMessage::RoomCommand(cmd) = msg {
                        registry.dispatch(cmd).await;
                    }
                }
                Some((device_id, actor_id)) = registry.exited_rx.recv() => {
                    if registry
                        .handles
                        .get(&device_id)
                        .is_some_and(|handle| handle.id == actor_id)
                    {
                        registry.handles.remove(&device_id);
                    }
                }
            }
        }
    })
}

struct RoomHandle {
    id: u64,
    tx: mpsc::Sender<ActorMessage>,
}

struct RoomRegistry {
    bus: Arc<dyn Bus>,
    reconnect_grace: ReconnectGrace,
    handles: HashMap<String, RoomHandle>,
    next_actor_id: u64,
    exited_tx: mpsc::Sender<(String, u64)>,
    exited_rx: mpsc::Receiver<(String, u64)>,
}

impl RoomRegistry {
    fn new(bus: Arc<dyn Bus>, reconnect_grace: ReconnectGrace) -> Self {
        let (exited_tx, exited_rx) = mpsc::channel(256);
        Self {
            bus,
            reconnect_grace,
            handles: HashMap::new(),
            next_actor_id: 1,
            exited_tx,
            exited_rx,
        }
    }

    fn spawn_actor(&mut self, device_id: &str) -> &RoomHandle {
        let actor_id = self.next_actor_id;
        self.next_actor_id += 1;
        let (tx, rx) = mpsc::channel(ACTOR_INBOX_CAPACITY);
        let actor = RoomActor {
            actor_id,
            device_id: device_id.to_owned(),
            bus: Arc::clone(&self.bus),
            reconnect_grace: self.reconnect_grace,
            exited_tx: self.exited_tx.clone(),
            rx,
            host: None,
            departed: None,
            grace_deadline: None,
            viewers: HashMap::new(),
            sessions: HashMap::new(),
        };
        tokio::spawn(actor.run());
        self.handles
            .insert(device_id.to_owned(), RoomHandle { id: actor_id, tx });
        self.handles.get(device_id).expect("just inserted")
    }

    async fn send_or_spawn(&mut self, device_id: &str, msg: ActorMessage) {
        if let Some(handle) = self.handles.get(device_id) {
            match handle.tx.send(msg).await {
                Ok(()) => return,
                Err(mpsc::error::SendError(unsent)) => {
                    self.handles.remove(device_id);
                    let fresh = self.spawn_actor(device_id);
                    let _ = fresh.tx.send(unsent).await;
                    return;
                }
            }
        }
        let fresh = self.spawn_actor(device_id);
        let _ = fresh.tx.send(msg).await;
    }

    /// Sends `msg` to an existing room actor; returns `Some(msg)` if no live actor exists.
    async fn send_if_exists(&mut self, device_id: &str, msg: ActorMessage) -> Option<ActorMessage> {
        let Some(handle) = self.handles.get(device_id) else {
            return Some(msg);
        };
        match handle.tx.send(msg).await {
            Ok(()) => None,
            Err(mpsc::error::SendError(unsent)) => {
                self.handles.remove(device_id);
                Some(unsent)
            }
        }
    }

    async fn dispatch(&mut self, cmd: RoomCommand) {
        match cmd {
            RoomCommand::Join {
                room,
                client_id,
                profile,
                reply,
            } => {
                let device_id = room.device_id.clone();
                self.send_or_spawn(
                    &device_id,
                    ActorMessage::Join {
                        room,
                        client_id,
                        profile,
                        reply,
                    },
                )
                .await;
            }
            RoomCommand::Leave { room, client_id } => {
                let device_id = room.device_id.clone();
                let _ = self
                    .send_if_exists(&device_id, ActorMessage::Leave { room, client_id })
                    .await;
            }
            RoomCommand::EndpointEvent {
                room,
                client_id,
                ack_id,
                event,
            } => {
                let device_id = room.device_id.clone();
                if let Some(ActorMessage::EndpointEvent {
                    client_id,
                    ack_id,
                    event,
                    ..
                }) = self
                    .send_if_exists(
                        &device_id,
                        ActorMessage::EndpointEvent {
                            room,
                            client_id,
                            ack_id,
                            event,
                        },
                    )
                    .await
                    && matches!(*event, SignalEvent::Control(_))
                    && let Some(ack_id) = ack_id
                {
                    let _ = self
                        .bus
                        .publish(
                            TOPIC_TO_ENDPOINT,
                            BusMessage::ToEndpoint(ToEndpoint {
                                device_id,
                                event: EndpointEvent::Ack {
                                    client_id,
                                    ack_id,
                                    args: vec![
                                        json!("failure"),
                                        json!({ "reason": Refused::HostOffline.reason() }),
                                    ],
                                },
                            }),
                        )
                        .await;
                }
            }
            RoomCommand::KickHost { device_id, reply } => {
                if let Some(ActorMessage::KickHost { reply }) = self
                    .send_if_exists(&device_id, ActorMessage::KickHost { reply })
                    .await
                {
                    reply.send(());
                }
            }
            RoomCommand::KickSharedViewers { device_id, reply } => {
                if let Some(ActorMessage::KickSharedViewers { reply }) = self
                    .send_if_exists(&device_id, ActorMessage::KickSharedViewers { reply })
                    .await
                {
                    reply.send(());
                }
            }
            RoomCommand::PushToHost {
                device_id,
                name,
                body,
                reply,
            } => {
                if let Some(ActorMessage::PushToHost { reply, .. }) = self
                    .send_if_exists(&device_id, ActorMessage::PushToHost { name, body, reply })
                    .await
                {
                    reply.send(false);
                }
            }
            RoomCommand::IsOnline { device_id, reply } => {
                if let Some(ActorMessage::IsOnline { reply }) = self
                    .send_if_exists(&device_id, ActorMessage::IsOnline { reply })
                    .await
                {
                    reply.send(false);
                }
            }
            RoomCommand::OnlineDevices { device_ids, reply } => {
                let mut receivers = Vec::new();
                for device_id in device_ids {
                    if self.handles.contains_key(&device_id) {
                        let (item_reply, rx) = oneshot::channel();
                        if self
                            .send_if_exists(
                                &device_id,
                                ActorMessage::CheckOnline { reply: item_reply },
                            )
                            .await
                            .is_none()
                        {
                            receivers.push((device_id, rx));
                        }
                    }
                }
                if receivers.is_empty() {
                    reply.send(HashSet::new());
                    return;
                }
                tokio::spawn(async move {
                    let mut online = HashSet::new();
                    for (device_id, rx) in receivers {
                        if rx.await == Ok(true) {
                            online.insert(device_id);
                        }
                    }
                    reply.send(online);
                });
            }
            RoomCommand::DesktopParticipants { device_ids, reply } => {
                let mut receivers = Vec::new();
                for device_id in device_ids {
                    if self.handles.contains_key(&device_id) {
                        let (item_reply, rx) = oneshot::channel();
                        if self
                            .send_if_exists(
                                &device_id,
                                ActorMessage::DesktopParticipants { reply: item_reply },
                            )
                            .await
                            .is_none()
                        {
                            receivers.push((device_id, rx));
                        }
                    }
                }
                if receivers.is_empty() {
                    reply.send(HashMap::new());
                    return;
                }
                tokio::spawn(async move {
                    let mut map = HashMap::new();
                    for (device_id, rx) in receivers {
                        if let Ok(list) = rx.await
                            && !list.is_empty()
                        {
                            map.insert(device_id, list);
                        }
                    }
                    reply.send(map);
                });
            }
            RoomCommand::ControlledElsewhere {
                device_id,
                from_device,
                reply,
            } => {
                if let Some(ActorMessage::ControlledElsewhere { reply, .. }) = self
                    .send_if_exists(
                        &device_id,
                        ActorMessage::ControlledElsewhere { from_device, reply },
                    )
                    .await
                {
                    reply.send(false);
                }
            }
        }
    }
}

enum ActorMessage {
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
        reply: Reply<()>,
    },
    KickSharedViewers {
        reply: Reply<()>,
    },
    PushToHost {
        name: String,
        body: Value,
        reply: Reply<bool>,
    },
    IsOnline {
        reply: Reply<bool>,
    },
    CheckOnline {
        reply: oneshot::Sender<bool>,
    },
    DesktopParticipants {
        reply: oneshot::Sender<Vec<ActiveParticipant>>,
    },
    ControlledElsewhere {
        from_device: Option<String>,
        reply: Reply<bool>,
    },
}

#[derive(Debug)]
struct PeerInfo {
    id: String,
    shared: bool,
    #[allow(
        dead_code,
        reason = "retained for capability-aware session negotiation"
    )]
    profile: Option<&'static Profile>,
}

#[derive(Debug)]
struct Session {
    /// The viewer connection's client id.
    viewer: String,
    /// The device the viewer runs on, if known.
    from_device: Option<String>,
    desktop: bool,
    shared: bool,
    started_at: Instant,
}

impl Session {
    /// Whether this is a desktop session held from a device other than `device`. A viewer that
    /// did not name its device counts as another device.
    fn holds_desktop_against(&self, device: Option<&str>) -> bool {
        self.desktop && (self.from_device.is_none() || self.from_device.as_deref() != device)
    }
}

/// Why a `control` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    HostOffline,
    /// Another device controls the desktop, and this viewer did not ask to take over.
    ControlledElsewhere,
}

impl Refused {
    fn reason(self) -> &'static str {
        match self {
            Self::HostOffline => "host offline",
            Self::ControlledElsewhere => "controlled from another device",
        }
    }
}

struct RoomActor {
    actor_id: u64,
    device_id: String,
    bus: Arc<dyn Bus>,
    reconnect_grace: ReconnectGrace,
    exited_tx: mpsc::Sender<(String, u64)>,
    rx: mpsc::Receiver<ActorMessage>,
    host: Option<PeerInfo>,
    /// The host connection that left its sessions behind, while they wait for a new one.
    departed: Option<String>,
    grace_deadline: Option<Instant>,
    viewers: HashMap<String, PeerInfo>,
    /// Open sessions, by `ice_id`.
    sessions: HashMap<String, Session>,
}

impl RoomActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                msg = self.rx.recv() => {
                    let Some(msg) = msg else { break; };
                    self.handle(msg).await;
                    if self.is_empty() {
                        break;
                    }
                }
                () = sleep_until_opt(self.grace_deadline) => {
                    self.expire_departure().await;
                    if self.is_empty() {
                        break;
                    }
                }
            }
        }
        let _ = self.exited_tx.send((self.device_id, self.actor_id)).await;
    }

    fn is_empty(&self) -> bool {
        // Sessions belong to viewers, so a room without viewers has none to keep.
        self.host.is_none() && self.viewers.is_empty()
    }

    async fn handle(&mut self, msg: ActorMessage) {
        match msg {
            ActorMessage::Join {
                room,
                client_id,
                profile,
                reply,
            } => {
                self.join(&room, client_id, profile).await;
                reply.send(());
            }
            ActorMessage::Leave { room, client_id } => {
                self.leave(&room, &client_id).await;
            }
            ActorMessage::EndpointEvent {
                room,
                client_id,
                ack_id,
                event,
            } => {
                self.handle_endpoint_event(&room, client_id, ack_id, *event)
                    .await;
            }
            ActorMessage::KickHost { reply } => {
                self.kick_host().await;
                reply.send(());
            }
            ActorMessage::KickSharedViewers { reply } => {
                self.kick_shared_viewers().await;
                reply.send(());
            }
            ActorMessage::PushToHost { name, body, reply } => {
                let sent = if let Some(host) = &self.host {
                    let host_id = host.id.clone();
                    self.send_event(&host_id, &name, body).await;
                    true
                } else {
                    false
                };
                reply.send(sent);
            }
            ActorMessage::IsOnline { reply } => {
                reply.send(self.host.is_some());
            }
            ActorMessage::CheckOnline { reply } => {
                let _ = reply.send(self.host.is_some());
            }
            ActorMessage::DesktopParticipants { reply } => {
                let now = Instant::now();
                let list = self
                    .sessions
                    .values()
                    .filter(|session| session.desktop)
                    .map(|session| ActiveParticipant {
                        client_id: session.viewer.clone(),
                        from_device: session.from_device.clone(),
                        controlled_secs: now
                            .saturating_duration_since(session.started_at)
                            .as_secs(),
                    })
                    .collect();
                let _ = reply.send(list);
            }
            ActorMessage::ControlledElsewhere { from_device, reply } => {
                let held = self
                    .sessions
                    .values()
                    .any(|session| session.holds_desktop_against(from_device.as_deref()));
                reply.send(held);
            }
        }
    }

    async fn join(&mut self, room: &Room, client_id: String, profile: Option<&'static Profile>) {
        let peer = PeerInfo {
            id: client_id.clone(),
            shared: room.shared,
            profile,
        };
        match &room.role {
            Role::Host => {
                if let Some(previous) = self.host.replace(peer) {
                    self.send_kick(&previous.id).await;
                }
                self.departed = None;
                self.grace_deadline = None;
                if !self.sessions.is_empty() {
                    tracing::info!(
                        device_id = %self.device_id,
                        sessions = self.sessions.len(),
                        "host reconnected to its sessions"
                    );
                }
            }
            Role::Viewer { .. } => {
                self.viewers.insert(client_id, peer);
            }
        }
    }

    async fn leave(&mut self, room: &Room, id: &str) {
        match room.role {
            Role::Host if self.host.as_ref().is_some_and(|host| host.id == id) => {
                self.host = None;
                if !self.sessions.is_empty() {
                    self.departed = Some(id.to_owned());
                    self.grace_deadline = Some(Instant::now() + self.reconnect_grace.0);
                }
            }
            Role::Host => {}
            Role::Viewer { .. } => {
                self.viewers.remove(id);
                let host_id = self.host.as_ref().map(|h| h.id.clone());
                let mut released = Vec::new();
                self.sessions.retain(|ice_id, session| {
                    if session.viewer != id {
                        return true;
                    }
                    released.push(ice_id.clone());
                    false
                });
                if let Some(host_id) = host_id {
                    for ice_id in released {
                        self.send_event(
                            &host_id,
                            names::RELEASED,
                            json!({ "ice_id": ice_id, "client_id": id }),
                        )
                        .await;
                    }
                }
            }
        }
    }

    async fn expire_departure(&mut self) {
        self.grace_deadline = None;
        let Some(host_id) = self.departed.take() else {
            return;
        };
        tracing::info!(
            device_id = %self.device_id,
            sessions = self.sessions.len(),
            "host did not reconnect to its sessions"
        );
        self.end_sessions_with_host(&host_id).await;
    }

    async fn kick_host(&mut self) {
        self.grace_deadline = None;
        let host_id = match &self.host {
            Some(host) => {
                let id = host.id.clone();
                self.send_kick(&id).await;
                id
            }
            None => match self.departed.take() {
                Some(departed) => departed,
                None => return,
            },
        };
        self.end_sessions_with_host(&host_id).await;
    }

    async fn handle_endpoint_event(
        &mut self,
        room: &Room,
        client_id: String,
        ack_id: Option<u64>,
        event: SignalEvent,
    ) {
        match (&room.role, event) {
            (Role::Host, SignalEvent::RoomInfo(_)) => {
                if let Some(ack_id) = ack_id {
                    let participant = json!({
                        "client_id": client_id,
                        "device_id": self.device_id,
                        "user_id": room.sub,
                        "role": "publisher",
                    });
                    let info = json!({
                        "room_id": self.device_id,
                        "publisher": participant,
                        "you": participant,
                        "subscribers": [],
                        "iceServers": [],
                        "reconnect_buffer": 0,
                    });
                    self.send_ack(&client_id, ack_id, vec![info]).await;
                }
                let ready = json!({
                    "data": { "status": "online", "publisher_availability_status": "ready" }
                });
                self.send_event(&client_id, names::BMSG_PUSH, ready).await;
            }
            (_, SignalEvent::RefreshReconnectKey(_)) => {
                if let Some(ack_id) = ack_id {
                    let key = hex::encode(random::<16>());
                    self.send_ack(
                        &client_id,
                        ack_id,
                        vec![json!("success"), json!({ "reconnect_key": key })],
                    )
                    .await;
                }
            }
            (
                Role::Viewer {
                    from_device,
                    take_over,
                },
                SignalEvent::Control(ctrl),
            ) => {
                let result = self
                    .control(
                        &room.sub,
                        &client_id,
                        from_device.as_deref(),
                        *take_over,
                        room.shared,
                        ctrl.app_control_id.as_deref(),
                        ctrl.streamer_data.as_ref(),
                        &ctrl.connect_options,
                    )
                    .await;
                let reply = match result {
                    Ok(granted) => {
                        tracing::info!(device_id = %self.device_id, "control granted");
                        vec![json!("success"), granted]
                    }
                    Err(refused) => {
                        let reason = refused.reason();
                        tracing::info!(device_id = %self.device_id, reason, "control refused");
                        vec![json!("failure"), json!({ "reason": reason })]
                    }
                };
                if let Some(ack_id) = ack_id {
                    self.send_ack(&client_id, ack_id, reply).await;
                }
            }
            (_, SignalEvent::Soac(mut soac)) => {
                let target_id = match &room.role {
                    Role::Host => soac
                        .client_id
                        .as_deref()
                        .and_then(|id| self.viewers.get(id))
                        .map(|p| p.id.clone()),
                    Role::Viewer { .. } => {
                        let ice_id = soac.ice_id();
                        let own = ice_id.is_some_and(|ice_id| {
                            self.sessions
                                .get(ice_id)
                                .is_some_and(|session| session.viewer == client_id)
                        });
                        self.host.as_ref().filter(|_| own).map(|p| p.id.clone())
                    }
                };
                let Some(target_id) = target_id else {
                    tracing::debug!(event = names::SOAC, "dropped an event outside any session");
                    return;
                };
                soac.client_id = Some(client_id);
                let body = serde_json::to_value(soac).expect("soac serializes");
                self.send_event(&target_id, names::SOAC, body).await;
            }
            (_, SignalEvent::ForwardSetting(mut fs)) => {
                let target_id = match &room.role {
                    Role::Host => fs
                        .client_id
                        .as_deref()
                        .and_then(|id| self.viewers.get(id))
                        .map(|p| p.id.clone()),
                    Role::Viewer { .. } => {
                        let ice_id = fs
                            .data
                            .get("ice_id")
                            .or_else(|| fs.extra.get("ice_id"))
                            .and_then(Value::as_str);
                        let own = ice_id.is_some_and(|ice_id| {
                            self.sessions
                                .get(ice_id)
                                .is_some_and(|session| session.viewer == client_id)
                        });
                        self.host.as_ref().filter(|_| own).map(|p| p.id.clone())
                    }
                };
                let Some(target_id) = target_id else {
                    tracing::debug!(
                        event = names::FORWARD_SETTING,
                        "dropped an event outside any session"
                    );
                    return;
                };
                fs.client_id = Some(client_id);
                let body = serde_json::to_value(fs).expect("forward_setting serializes");
                self.send_event(&target_id, names::FORWARD_SETTING, body)
                    .await;
            }
            (_, other) => {
                tracing::debug!(event = other.name(), "ignored event");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn control(
        &mut self,
        user_id: &str,
        viewer_id: &str,
        from_device: Option<&str>,
        take_over: bool,
        shared: bool,
        app_control_id: Option<&str>,
        streamer_data: Option<&Value>,
        connect_options: &[u8],
    ) -> Result<Value, Refused> {
        let host_id = self.host.as_ref().ok_or(Refused::HostOffline)?.id.clone();

        let desktop = is_desktop(connect_options);
        if desktop {
            let held: Vec<String> = self
                .sessions
                .iter()
                .filter(|(_, session)| session.holds_desktop_against(from_device))
                .map(|(ice_id, _)| ice_id.clone())
                .collect();
            if !held.is_empty() && !take_over {
                return Err(Refused::ControlledElsewhere);
            }
            for ice_id in held {
                self.end_session(&ice_id, &host_id).await;
            }
        }

        let ice = self.request_ice_grant(user_id).await;
        let ice_id = hex::encode(random::<16>());
        let app_control_id_value = app_control_id.map_or(Value::Null, |id| json!(id));
        let mut be_controlled = json!({
            "ice_id": ice_id,
            "client_id": viewer_id,
            "app_control_id": app_control_id_value,
            "iceServers": ice.servers,
            "force_relay": ice.force_relay,
            "app_data": placeholder(0),
        });
        if let Some(streamer_data) = streamer_data {
            be_controlled["streamer_data"] = streamer_data.clone();
        }
        if let Some(platform) = controller_platform(connect_options) {
            be_controlled["controller_platform"] = platform.into();
        }
        let frames = vec![
            EndpointFrame::Text(encode_binary_event(
                names::BE_CONTROLLED,
                &[be_controlled],
                1,
                None,
            )),
            EndpointFrame::Binary(connect_options.to_vec()),
        ];
        if self.send_frames(&host_id, frames).await == 0 {
            return Err(Refused::HostOffline);
        }
        let ack = json!({
            "ice_id": ice_id,
            "client_id": host_id,
            "app_control_id": app_control_id_value,
            "iceServers": ice.servers,
            "force_relay": ice.force_relay,
        });
        let session = Session {
            viewer: viewer_id.to_owned(),
            from_device: from_device.map(str::to_owned),
            desktop,
            shared,
            started_at: Instant::now(),
        };
        self.sessions.insert(ice_id, session);
        Ok(ack)
    }

    async fn request_ice_grant(&self, user_id: &str) -> Grant {
        let (reply, rx) = Reply::channel();
        let receivers = self
            .bus
            .publish(
                TOPIC_RELAY,
                BusMessage::RelayCommand(RelayCommand::Grant {
                    user_id: user_id.to_owned(),
                    reply,
                }),
            )
            .await;
        if receivers == 0 {
            return Grant::default();
        }
        rx.await.unwrap_or_default()
    }

    /// The host is gone for good: every viewer in a session with it is told the host left.
    async fn end_sessions_with_host(&mut self, host_id: &str) {
        let sessions: Vec<(String, Session)> = self.sessions.drain().collect();
        for (ice_id, session) in sessions {
            if self.viewers.contains_key(&session.viewer) {
                self.send_event(
                    &session.viewer,
                    names::LEFT,
                    json!({ "ice_id": ice_id, "client_id": host_id }),
                )
                .await;
            }
        }
    }

    /// Ends one session as if each side had left the other.
    async fn end_session(&mut self, ice_id: &str, host_id: &str) {
        let Some(session) = self.sessions.remove(ice_id) else {
            return;
        };
        self.send_event(
            host_id,
            names::RELEASED,
            json!({ "ice_id": ice_id, "client_id": session.viewer }),
        )
        .await;
        if self.viewers.contains_key(&session.viewer) {
            self.send_event(
                &session.viewer,
                names::LEFT,
                json!({ "ice_id": ice_id, "client_id": host_id }),
            )
            .await;
        }
    }

    /// Terminates all shared (remote-assist) sessions and disconnects shared viewers, leaving any
    /// account-owner sessions untouched.
    async fn kick_shared_viewers(&mut self) {
        let shared_ice: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.shared)
            .map(|(ice_id, _)| ice_id.clone())
            .collect();
        let host_id = self
            .host
            .as_ref()
            .map(|h| h.id.clone())
            .or_else(|| self.departed.clone());
        for ice_id in shared_ice {
            if let Some(host_id) = host_id.as_deref() {
                self.end_session(&ice_id, host_id).await;
            } else if let Some(session) = self.sessions.remove(&ice_id)
                && self.viewers.contains_key(&session.viewer)
            {
                self.send_event(&session.viewer, names::LEFT, json!({ "ice_id": ice_id }))
                    .await;
            }
        }
        let shared_viewers: Vec<String> = self
            .viewers
            .iter()
            .filter(|(_, peer)| peer.shared)
            .map(|(id, _)| id.clone())
            .collect();
        for viewer_id in shared_viewers {
            self.viewers.remove(&viewer_id);
            self.send_kick(&viewer_id).await;
        }
    }

    async fn send_frames(&self, client_id: &str, frames: Vec<EndpointFrame>) -> usize {
        self.bus
            .publish(
                TOPIC_TO_ENDPOINT,
                BusMessage::ToEndpoint(ToEndpoint {
                    device_id: self.device_id.clone(),
                    event: EndpointEvent::Frames {
                        client_id: client_id.to_owned(),
                        frames,
                    },
                }),
            )
            .await
    }

    async fn send_event(&self, client_id: &str, name: &str, body: Value) -> usize {
        self.bus
            .publish(
                TOPIC_TO_ENDPOINT,
                BusMessage::ToEndpoint(ToEndpoint {
                    device_id: self.device_id.clone(),
                    event: EndpointEvent::Event {
                        client_id: client_id.to_owned(),
                        name: name.to_owned(),
                        body,
                    },
                }),
            )
            .await
    }

    async fn send_ack(&self, client_id: &str, ack_id: u64, args: Vec<Value>) -> usize {
        self.bus
            .publish(
                TOPIC_TO_ENDPOINT,
                BusMessage::ToEndpoint(ToEndpoint {
                    device_id: self.device_id.clone(),
                    event: EndpointEvent::Ack {
                        client_id: client_id.to_owned(),
                        ack_id,
                        args,
                    },
                }),
            )
            .await
    }

    async fn send_kick(&self, client_id: &str) -> usize {
        self.bus
            .publish(
                TOPIC_TO_ENDPOINT,
                BusMessage::ToEndpoint(ToEndpoint {
                    device_id: self.device_id.clone(),
                    event: EndpointEvent::Kick {
                        client_id: client_id.to_owned(),
                    },
                }),
            )
            .await
    }
}

fn sleep_until_opt(deadline: Option<Instant>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    match deadline {
        Some(instant) => Box::pin(sleep_until(instant)),
        None => Box::pin(std::future::pending()),
    }
}

/// Whether `ConnectOptions` ask for the desktop rather than a tool: capture types unknown (0),
/// desktop (1) and window (2).
fn is_desktop(connect_options: &[u8]) -> bool {
    ConnectOptions::decode(connect_options).map_or(true, |options| options.capture_type <= 2)
}

/// The viewer's platform as `be-controlled` names it, from the `ConnectOptions` client type.
fn controller_platform(connect_options: &[u8]) -> Option<u8> {
    const IOS: i32 = 1;
    const ANDROID: i32 = 2;
    const WINDOWS: i32 = 3;
    const MAC: i32 = 4;
    let options = ConnectOptions::decode(connect_options).ok()?;
    Some(match options.client_type {
        WINDOWS => 1,
        ANDROID => 2,
        IOS => 3,
        MAC => 4,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use remora_proto::signal::{ControlAck, Decoder, Packet, encode_event};

    use super::*;
    use crate::bus::{LocalBus, Subscription};
    use crate::room::RoomClient;

    struct Harness {
        bus: Arc<LocalBus>,
        client: RoomClient,
        outbound: Subscription,
        buffered: VecDeque<ToEndpoint>,
        next_ack: u64,
    }

    impl Harness {
        fn start(grace: Duration) -> Self {
            let bus = Arc::new(LocalBus::new());
            let outbound = bus.subscribe(TOPIC_TO_ENDPOINT);
            let registry_bus: Arc<dyn Bus> = bus.clone();
            spawn(registry_bus, ReconnectGrace(grace));
            let client = RoomClient::new(bus.clone());
            Self {
                bus,
                client,
                outbound,
                buffered: VecDeque::new(),
                next_ack: 1,
            }
        }

        async fn join(&self, role: Role, id: &str) -> Member {
            let room = Room {
                sub: "1".into(),
                device_id: "d".into(),
                role,
                guest: false,
                shared: false,
            };
            let (reply, rx) = Reply::channel();
            self.bus
                .publish(
                    TOPIC_ROOM_COMMAND,
                    BusMessage::RoomCommand(RoomCommand::Join {
                        room: room.clone(),
                        client_id: id.to_owned(),
                        profile: None,
                        reply,
                    }),
                )
                .await;
            rx.await.unwrap();
            Member {
                room,
                id: id.to_owned(),
            }
        }

        async fn leave(&self, member: &Member) {
            self.bus
                .publish(
                    TOPIC_ROOM_COMMAND,
                    BusMessage::RoomCommand(RoomCommand::Leave {
                        room: member.room.clone(),
                        client_id: member.id.clone(),
                    }),
                )
                .await;
        }

        async fn send_control(
            &mut self,
            viewer: &Member,
            header: Value,
            connect_options: &[u8],
        ) -> ControlAck {
            let ack_id = self.next_ack;
            self.next_ack += 1;
            let mut decoder = Decoder::new();
            let raw_header = encode_binary_event(names::CONTROL, &[header], 1, Some(ack_id));
            assert!(decoder.text(&raw_header).unwrap().is_none());
            let Some(Packet::Event(ev)) = decoder.binary(connect_options).unwrap() else {
                panic!("expected event");
            };
            let signal = SignalEvent::from_event(&ev);
            self.bus
                .publish(
                    TOPIC_ROOM_COMMAND,
                    BusMessage::RoomCommand(RoomCommand::EndpointEvent {
                        room: viewer.room.clone(),
                        client_id: viewer.id.clone(),
                        ack_id: Some(ack_id),
                        event: Box::new(signal),
                    }),
                )
                .await;

            // Collect outbound messages until the ack for `viewer` arrives, buffering others.
            let mut deferred = VecDeque::new();
            loop {
                let to = if let Some(item) = self.buffered.pop_front() {
                    item
                } else {
                    let Some(BusMessage::ToEndpoint(to)) = self.outbound.recv().await else {
                        panic!("expected ToEndpoint");
                    };
                    to
                };
                if let EndpointEvent::Ack {
                    client_id,
                    ack_id: got_id,
                    args,
                } = &to.event
                    && client_id == &viewer.id
                    && *got_id == ack_id
                {
                    self.buffered = deferred;
                    return ControlAck::decode(args);
                }
                deferred.push_back(to);
            }
        }

        async fn relay(&self, from: &Member, name: &str, body: Value) {
            let mut decoder = Decoder::new();
            let Some(Packet::Event(ev)) = decoder.text(&encode_event(name, &[body], None)).unwrap()
            else {
                panic!("expected event");
            };
            let signal = SignalEvent::from_event(&ev);
            self.bus
                .publish(
                    TOPIC_ROOM_COMMAND,
                    BusMessage::RoomCommand(RoomCommand::EndpointEvent {
                        room: from.room.clone(),
                        client_id: from.id.clone(),
                        ack_id: None,
                        event: Box::new(signal),
                    }),
                )
                .await;
        }

        async fn next_for(&mut self, target_id: &str) -> EndpointEvent {
            if let Some(pos) = self.buffered.iter().position(|to| match &to.event {
                EndpointEvent::Frames { client_id, .. }
                | EndpointEvent::Event { client_id, .. }
                | EndpointEvent::Ack { client_id, .. }
                | EndpointEvent::Kick { client_id } => client_id == target_id,
            }) {
                return self.buffered.remove(pos).unwrap().event;
            }
            loop {
                let to = tokio::time::timeout(Duration::from_secs(2), self.outbound.recv())
                    .await
                    .expect("timed out waiting for outbound")
                    .expect("outbound open");
                let BusMessage::ToEndpoint(to) = to else {
                    continue;
                };
                let matches = match &to.event {
                    EndpointEvent::Frames { client_id, .. }
                    | EndpointEvent::Event { client_id, .. }
                    | EndpointEvent::Ack { client_id, .. }
                    | EndpointEvent::Kick { client_id } => client_id == target_id,
                };
                if matches {
                    return to.event;
                }
                self.buffered.push_back(to);
            }
        }

        async fn next_event_decoded(&mut self, target_id: &str) -> remora_proto::signal::Event {
            match self.next_for(target_id).await {
                EndpointEvent::Frames { frames, .. } => {
                    let mut decoder = Decoder::new();
                    let mut packet = None;
                    for frame in frames {
                        packet = match frame {
                            EndpointFrame::Text(text) => decoder.text(&text).unwrap(),
                            EndpointFrame::Binary(bytes) => decoder.binary(&bytes).unwrap(),
                        };
                    }
                    let Some(Packet::Event(event)) = packet else {
                        panic!("expected an event");
                    };
                    event
                }
                EndpointEvent::Event { name, body, .. } => remora_proto::signal::Event {
                    ack_id: None,
                    name,
                    args: vec![body],
                    attachments: Vec::new(),
                },
                other => panic!("expected event or frames, got {other:?}"),
            }
        }

        async fn assert_idle(&mut self) {
            tokio::task::yield_now().await;
            assert!(
                self.buffered.is_empty(),
                "expected idle, buffered: {:?}",
                self.buffered
            );
            let got = tokio::time::timeout(Duration::from_millis(20), self.outbound.recv()).await;
            assert!(got.is_err(), "expected idle, got {got:?}");
        }
    }

    struct Member {
        room: Room,
        id: String,
    }

    fn viewer_on(device: Option<&str>) -> Role {
        Role::Viewer {
            from_device: device.map(str::to_owned),
            take_over: false,
        }
    }

    fn windows_viewer_options() -> Vec<u8> {
        ConnectOptions {
            capture_type: 1,
            client_type: 3,
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn options(capture_type: i32) -> Vec<u8> {
        ConnectOptions {
            capture_type,
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[tokio::test]
    async fn control_reaches_the_host_and_soac_flows_both_ways() {
        let mut h = Harness::start(Duration::from_secs(30));
        let host = h.join(Role::Host, "host").await;
        let viewer = h.join(viewer_on(None), "viewer").await;
        let header =
            json!({ "app_control_id": "app", "streamer_data": "{}", "app_data": placeholder(0) });
        let options = windows_viewer_options();

        let ack = h.send_control(&viewer, header, &options).await;
        let ControlAck::Success {
            ice_id,
            client_id,
            app_control_id,
            ice_servers,
            ..
        } = ack
        else {
            panic!("expected control success, got {ack:?}");
        };
        assert_eq!(client_id, "host");
        assert_eq!(app_control_id.as_deref(), Some("app"));
        assert_eq!(ice_servers, json!([]));

        let be_controlled = h.next_event_decoded("host").await;
        assert_eq!(be_controlled.name, "be-controlled");
        let hdr = &be_controlled.args[0];
        assert_eq!(hdr["ice_id"], ice_id.as_str());
        assert_eq!(hdr["client_id"], "viewer");
        assert_eq!(hdr["app_control_id"], "app");
        assert_eq!(hdr["streamer_data"], "{}");
        assert_eq!(hdr["controller_platform"], 1);
        assert_eq!(
            be_controlled.attachment(&hdr["app_data"]),
            Some(&options[..])
        );

        let offer = json!({ "client_id": "host", "data": { "type": "offer", "ice_id": ice_id } });
        h.relay(&viewer, "soac", offer).await;
        let offer = h.next_event_decoded("host").await;
        assert_eq!(offer.args[0]["client_id"], "viewer");
        assert_eq!(offer.args[0]["data"]["type"], "offer");

        let answer =
            json!({ "client_id": "viewer", "data": { "type": "answer", "ice_id": ice_id } });
        h.relay(&host, "soac", answer).await;
        let answer = h.next_event_decoded("viewer").await;
        assert_eq!(answer.args[0]["client_id"], "host");
        assert_eq!(answer.args[0]["data"]["type"], "answer");
    }

    #[tokio::test]
    async fn control_needs_the_host_online() {
        let mut h = Harness::start(Duration::from_secs(30));
        let viewer = h.join(viewer_on(None), "viewer").await;
        let options = windows_viewer_options();
        let ack = h.send_control(&viewer, json!({}), &options).await;
        assert!(matches!(ack, ControlAck::Failure { .. }));

        let elsewhere = Member {
            room: Room {
                device_id: "other".into(),
                ..viewer.room.clone()
            },
            id: "viewer2".into(),
        };
        let ack = h.send_control(&elsewhere, json!({}), &options).await;
        assert!(matches!(ack, ControlAck::Failure { .. }));
    }

    #[tokio::test]
    async fn relays_only_within_a_session() {
        let mut h = Harness::start(Duration::from_secs(30));
        let host = h.join(Role::Host, "host").await;
        let viewer = h.join(viewer_on(None), "viewer").await;
        let other = h.join(viewer_on(None), "other").await;
        let ControlAck::Success { ice_id, .. } = h.send_control(&viewer, json!({}), &[]).await
        else {
            panic!("expected success");
        };
        h.next_event_decoded("host").await;

        let soac = |id: &str| json!({ "data": { "type": "candidate", "ice_id": id } });
        h.relay(&other, "soac", soac(&ice_id)).await;
        h.relay(&viewer, "soac", soac("unknown")).await;
        h.relay(&host, "soac", json!({ "client_id": "nobody", "data": {} }))
            .await;
        h.assert_idle().await;
    }

    async fn open_session(h: &mut Harness, host_id: &str, viewer: &Member) -> String {
        let ControlAck::Success { ice_id, .. } = h.send_control(viewer, json!({}), &[]).await
        else {
            panic!("expected success");
        };
        assert_eq!(h.next_event_decoded(host_id).await.name, "be-controlled");
        ice_id
    }

    #[tokio::test]
    async fn a_viewer_leaving_releases_its_sessions() {
        let mut h = Harness::start(Duration::from_secs(30));
        let _host = h.join(Role::Host, "host").await;
        let viewer = h.join(viewer_on(None), "viewer").await;
        let ice_id = open_session(&mut h, "host", &viewer).await;

        h.leave(&viewer).await;
        let released = h.next_event_decoded("host").await;
        assert_eq!(released.name, "released");
        assert_eq!(
            released.args[0],
            json!({ "ice_id": ice_id, "client_id": "viewer" })
        );
        assert!(!h.client.controlled_elsewhere("d", None).await);
    }

    #[tokio::test]
    async fn a_host_that_does_not_reconnect_ends_its_sessions() {
        let mut h = Harness::start(Duration::from_millis(50));
        let host = h.join(Role::Host, "host").await;
        let _viewer = h.join(viewer_on(None), "viewer").await;
        let ice_id = open_session(&mut h, "host", &_viewer).await;

        h.leave(&host).await;
        tokio::task::yield_now().await;
        assert!(!h.client.is_online("d").await);
        assert!(h.client.controlled_elsewhere("d", None).await);

        let left = h.next_event_decoded("viewer").await;
        assert_eq!(left.name, "left");
        assert_eq!(
            left.args[0],
            json!({ "ice_id": ice_id, "client_id": "host" })
        );
        assert!(!h.client.controlled_elsewhere("d", None).await);
    }

    #[tokio::test]
    async fn a_reconnecting_host_takes_its_sessions_back() {
        let mut h = Harness::start(Duration::from_millis(80));
        let host = h.join(Role::Host, "host").await;
        let viewer = h.join(viewer_on(None), "viewer").await;
        let ice_id = open_session(&mut h, "host", &viewer).await;

        h.leave(&host).await;
        let _back = h.join(Role::Host, "back").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.assert_idle().await;

        let candidate = json!({ "data": { "type": "candidate", "ice_id": ice_id } });
        h.relay(&viewer, "soac", candidate).await;
        assert_eq!(h.next_event_decoded("back").await.name, "soac");
        h.leave(&viewer).await;
        assert_eq!(h.next_event_decoded("back").await.name, "released");
    }

    #[tokio::test]
    async fn a_newer_host_kicks_the_older_one_and_takes_its_sessions() {
        let mut h = Harness::start(Duration::from_secs(30));
        let older = h.join(Role::Host, "older").await;
        let viewer = h.join(viewer_on(None), "viewer").await;
        let ice_id = open_session(&mut h, "older", &viewer).await;

        let _newer = h.join(Role::Host, "newer").await;
        assert_eq!(
            h.next_for("older").await,
            EndpointEvent::Kick {
                client_id: "older".into()
            }
        );
        h.leave(&older).await;
        assert!(h.client.is_online("d").await);

        let candidate = json!({ "data": { "type": "candidate", "ice_id": ice_id } });
        h.relay(&viewer, "soac", candidate).await;
        assert_eq!(h.next_event_decoded("newer").await.name, "soac");
    }

    #[tokio::test]
    async fn kicking_the_host_ends_its_sessions_at_once() {
        let mut h = Harness::start(Duration::from_secs(30));
        let host = h.join(Role::Host, "host").await;
        let viewer = h.join(viewer_on(None), "viewer").await;
        open_session(&mut h, "host", &viewer).await;

        h.client.kick_host("d").await;
        assert_eq!(
            h.next_for("host").await,
            EndpointEvent::Kick {
                client_id: "host".into()
            }
        );
        assert_eq!(h.next_event_decoded("viewer").await.name, "left");
        h.leave(&host).await;

        let host = h.join(Role::Host, "host2").await;
        let ControlAck::Success { .. } = h.send_control(&viewer, json!({}), &[]).await else {
            panic!("expected control success");
        };
        assert_eq!(h.next_event_decoded("host2").await.name, "be-controlled");
        h.leave(&host).await;
        h.client.kick_host("d").await;
        assert_eq!(h.next_event_decoded("viewer").await.name, "left");
    }

    #[tokio::test]
    async fn one_device_at_a_time_controls_the_desktop() {
        const DESKTOP: i32 = 1;
        const TERMINAL: i32 = 8;
        let mut h = Harness::start(Duration::from_secs(30));
        let _host = h.join(Role::Host, "host").await;
        let phone = h.join(viewer_on(Some("phone")), "phone").await;
        let laptop = h.join(viewer_on(Some("laptop")), "laptop").await;

        assert!(matches!(
            h.send_control(&phone, json!({}), &options(DESKTOP)).await,
            ControlAck::Success { .. }
        ));
        h.next_event_decoded("host").await;
        assert!(h.client.controlled_elsewhere("d", Some("laptop")).await);
        assert!(h.client.controlled_elsewhere("d", None).await);
        assert!(!h.client.controlled_elsewhere("d", Some("phone")).await);

        assert!(matches!(
            h.send_control(&laptop, json!({}), &options(DESKTOP)).await,
            ControlAck::Failure { .. }
        ));
        h.assert_idle().await;
        assert!(matches!(
            h.send_control(&laptop, json!({}), &options(TERMINAL)).await,
            ControlAck::Success { .. }
        ));
        h.next_event_decoded("host").await;
        assert!(matches!(
            h.send_control(&phone, json!({}), &options(DESKTOP)).await,
            ControlAck::Success { .. }
        ));
        h.next_event_decoded("host").await;
    }

    #[tokio::test]
    async fn taking_over_ends_the_other_devices_desktop_sessions() {
        let mut h = Harness::start(Duration::from_secs(30));
        let _host = h.join(Role::Host, "host").await;
        let phone = h.join(viewer_on(Some("phone")), "phone").await;
        let laptop = h
            .join(
                Role::Viewer {
                    from_device: Some("laptop".into()),
                    take_over: true,
                },
                "laptop",
            )
            .await;
        let desktop = options(1);

        let ControlAck::Success { ice_id, .. } = h.send_control(&phone, json!({}), &desktop).await
        else {
            panic!("expected success");
        };
        h.next_event_decoded("host").await;

        assert!(matches!(
            h.send_control(&laptop, json!({}), &desktop).await,
            ControlAck::Success { .. }
        ));
        let released = h.next_event_decoded("host").await;
        assert_eq!(released.name, "released");
        assert_eq!(
            released.args[0],
            json!({ "ice_id": ice_id, "client_id": "phone" })
        );
        assert_eq!(h.next_event_decoded("host").await.name, "be-controlled");
        let left = h.next_event_decoded("phone").await;
        assert_eq!(left.name, "left");
        assert_eq!(
            left.args[0],
            json!({ "ice_id": ice_id, "client_id": "host" })
        );
        assert!(!h.client.controlled_elsewhere("d", Some("laptop")).await);
    }

    #[test]
    fn controller_platform_follows_the_client_type() {
        let options = |client_type| {
            ConnectOptions {
                client_type,
                ..Default::default()
            }
            .encode_to_vec()
        };
        assert_eq!(controller_platform(&options(3)), Some(1));
        assert_eq!(controller_platform(&options(2)), Some(2));
        assert_eq!(controller_platform(&options(1)), Some(3));
        assert_eq!(controller_platform(&options(4)), Some(4));
        assert_eq!(controller_platform(&options(0)), None);
        assert_eq!(controller_platform(b"\xff"), None);
    }
}
