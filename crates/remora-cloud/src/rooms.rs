//! Who is in each device's room, and how signaling messages are routed between them.
//!
//! A room holds the device's host and any viewers that joined it. A viewer's `control` opens a
//! session with the host, named by an `ice_id` the cloud assigns. After that, `soac` and
//! `forward_setting` pass between the two, each addressed by the receiver's `client_id`. On
//! the way, `client_id` is rewritten to the sender's, which is what the receiver answers to.
//!
//! Only one viewer device at a time controls the desktop. Tool sessions (files, terminal, port
//! mapping) and further sessions from the controlling device are not limited.
//!
//! Sessions outlive the host's signaling connection: their media flows between host and viewer
//! directly, and hosts keep it up while they reconnect, which they also do to renew the room.
//! A host connection that replaces the previous one takes over its sessions. If the host left
//! without a replacement, its sessions wait for one until [`Rooms::expire`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use prost::Message as _;
use remora_proto::gv_pb::ConnectOptions;
use remora_proto::socketio::{encode_binary_event, encode_event, placeholder};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::ice::Grant;
use crate::token::{Role, Room, random};

/// Signaling connections in rooms, by device id.
#[derive(Debug, Default, Clone)]
pub struct Rooms(Arc<Mutex<HashMap<String, Members>>>);

#[derive(Debug, Default)]
struct Members {
    host: Option<Peer>,
    /// The host connection that left its sessions behind, while they wait for a new one.
    departed: Option<String>,
    viewers: HashMap<String, Peer>,
    /// Open sessions, by `ice_id`.
    sessions: HashMap<String, Session>,
}

#[derive(Debug)]
struct Session {
    /// The viewer connection's client id.
    viewer: String,
    /// The device the viewer runs on, if known.
    from_device: Option<String>,
    desktop: bool,
}

impl Session {
    /// Whether this is a desktop session held from a device other than `device`. A viewer that
    /// did not name its device counts as another device.
    fn holds_desktop_against(&self, device: Option<&str>) -> bool {
        self.desktop && (self.from_device.is_none() || self.from_device.as_deref() != device)
    }
}

/// One signaling connection.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Its `client_id`, unique per connection.
    pub id: String,
    pub outbox: mpsc::Sender<Outbound>,
}

#[derive(Debug, PartialEq)]
pub enum Outbound {
    Frames(Vec<Frame>),
    /// The connection lost the room: another took over the device, or the device was unbound.
    Kicked,
}

#[derive(Debug, PartialEq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

impl Peer {
    /// Queues frames for the connection. A connection that stopped draining its queue is
    /// timing out anyway, so frames for it are dropped rather than waited for.
    fn send(&self, frames: Vec<Frame>) -> bool {
        self.outbox.try_send(Outbound::Frames(frames)).is_ok()
    }

    fn event(&self, name: &str, body: Value) -> bool {
        self.send(vec![Frame::Text(encode_event(name, &[body], None))])
    }
}

/// A host connection that left sessions behind; see [`Rooms::expire`].
#[derive(Debug, PartialEq, Eq)]
#[must_use = "the sessions left behind wait until expired"]
pub struct Departure {
    device_id: String,
    host_id: String,
}

/// Why a `control` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    HostOffline,
    /// Another device controls the desktop, and this viewer did not ask to take over.
    ControlledElsewhere,
}

impl Rooms {
    pub fn join(&self, room: &Room, peer: Peer) {
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let members = rooms.entry(room.device_id.clone()).or_default();
        match &room.role {
            Role::Host => {
                if let Some(previous) = members.host.replace(peer) {
                    // A full outbox means that connection is stuck; it times out by itself.
                    let _ = previous.outbox.try_send(Outbound::Kicked);
                }
                members.departed = None;
                if !members.sessions.is_empty() {
                    tracing::info!(
                        device_id = %room.device_id,
                        sessions = members.sessions.len(),
                        "host reconnected to its sessions"
                    );
                }
            }
            Role::Viewer { .. } => {
                members.viewers.insert(peer.id.clone(), peer);
            }
        }
    }

    /// Removes a connection. A viewer's sessions end, and the host is told. A host's sessions
    /// are left behind for the next host connection, unless they expire first.
    pub fn leave(&self, room: &Room, id: &str) -> Option<Departure> {
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let members = rooms.get_mut(&room.device_id)?;
        let mut departure = None;
        match room.role {
            Role::Host if members.host.as_ref().is_some_and(|host| host.id == id) => {
                members.host = None;
                if !members.sessions.is_empty() {
                    members.departed = Some(id.to_owned());
                    departure = Some(Departure {
                        device_id: room.device_id.clone(),
                        host_id: id.to_owned(),
                    });
                }
            }
            Role::Host => {}
            Role::Viewer { .. } => {
                members.viewers.remove(id);
                let host = members.host.as_ref();
                members.sessions.retain(|ice_id, session| {
                    if session.viewer != id {
                        return true;
                    }
                    if let Some(host) = host {
                        host.event("released", json!({ "ice_id": ice_id, "client_id": id }));
                    }
                    false
                });
            }
        }
        // Sessions belong to viewers, so a room without viewers has none to keep.
        if members.host.is_none() && members.viewers.is_empty() {
            rooms.remove(&room.device_id);
        }
        departure
    }

    /// Ends the sessions a host connection left behind, unless another took them over since.
    /// Each viewer is told the host left.
    pub fn expire(&self, departure: &Departure) {
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let Some(members) = rooms.get_mut(&departure.device_id) else {
            return;
        };
        if members.departed.as_ref() != Some(&departure.host_id) {
            return;
        }
        members.departed = None;
        tracing::info!(
            device_id = %departure.device_id,
            sessions = members.sessions.len(),
            "host did not reconnect to its sessions"
        );
        members.end_sessions_with_host(&departure.host_id);
    }

    /// Whether a device other than `from_device` controls the desktop of `device_id`.
    pub fn controlled_elsewhere(&self, device_id: &str, from_device: Option<&str>) -> bool {
        let rooms = self.0.lock().expect("rooms lock poisoned");
        rooms.get(device_id).is_some_and(|members| {
            members
                .sessions
                .values()
                .any(|session| session.holds_desktop_against(from_device))
        })
    }

    /// Drops the device's host from signaling and ends its sessions; its connection closes
    /// and leaves the room.
    pub fn kick_host(&self, device_id: &str) {
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let Some(members) = rooms.get_mut(device_id) else {
            return;
        };
        let host_id = match &members.host {
            Some(host) => {
                // A full outbox means that connection is stuck; it times out by itself.
                let _ = host.outbox.try_send(Outbound::Kicked);
                host.id.clone()
            }
            None => match members.departed.take() {
                Some(departed) => departed,
                None => return,
            },
        };
        members.end_sessions_with_host(&host_id);
    }

    /// Sends an event to the device's host; `false` if it is not on signaling or not draining
    /// its queue.
    pub fn push_to_host(&self, device_id: &str, name: &str, body: Value) -> bool {
        let rooms = self.0.lock().expect("rooms lock poisoned");
        rooms
            .get(device_id)
            .and_then(|members| members.host.as_ref())
            .is_some_and(|host| host.event(name, body))
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        let rooms = self.0.lock().expect("rooms lock poisoned");
        rooms
            .get(device_id)
            .is_some_and(|members| members.host.is_some())
    }

    /// The viewer `viewer_id`, connected with `room`, asks to control the host: forwards
    /// `be-controlled` with the viewer's `ConnectOptions`, and returns what the viewer's
    /// `control` is acknowledged with.
    pub fn control(
        &self,
        room: &Room,
        viewer_id: &str,
        header: &Value,
        connect_options: &[u8],
        ice: &Grant,
    ) -> Result<Value, Refused> {
        let (from_device, take_over) = match &room.role {
            Role::Viewer {
                from_device,
                take_over,
            } => (from_device.as_deref(), *take_over),
            Role::Host => (None, false),
        };
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let members = rooms.get_mut(&room.device_id).ok_or(Refused::HostOffline)?;
        let host = members.host.clone().ok_or(Refused::HostOffline)?;

        let desktop = is_desktop(connect_options);
        if desktop {
            let held: Vec<String> = members
                .sessions
                .iter()
                .filter(|(_, session)| session.holds_desktop_against(from_device))
                .map(|(ice_id, _)| ice_id.clone())
                .collect();
            if !held.is_empty() && !take_over {
                return Err(Refused::ControlledElsewhere);
            }
            for ice_id in held {
                members.end_session(&ice_id, &host);
            }
        }

        let ice_id = hex::encode(random::<16>());
        let app_control_id = header.get("app_control_id").cloned().unwrap_or_default();
        let mut be_controlled = json!({
            "ice_id": ice_id,
            "client_id": viewer_id,
            "app_control_id": app_control_id,
            "iceServers": ice.servers,
            "force_relay": ice.force_relay,
            "app_data": placeholder(0),
        });
        if let Some(streamer_data) = header.get("streamer_data") {
            be_controlled["streamer_data"] = streamer_data.clone();
        }
        if let Some(platform) = controller_platform(connect_options) {
            be_controlled["controller_platform"] = platform.into();
        }
        let frames = vec![
            Frame::Text(encode_binary_event(
                "be-controlled",
                &[be_controlled],
                1,
                None,
            )),
            Frame::Binary(connect_options.to_vec()),
        ];
        if !host.send(frames) {
            return Err(Refused::HostOffline);
        }
        let ack = json!({
            "ice_id": ice_id,
            "client_id": host.id,
            "app_control_id": app_control_id,
            "iceServers": ice.servers,
            "force_relay": ice.force_relay,
        });
        let session = Session {
            viewer: viewer_id.to_owned(),
            from_device: from_device.map(str::to_owned),
            desktop,
        };
        members.sessions.insert(ice_id, session);
        Ok(ack)
    }

    /// Passes an event from connection `from` to the other side of its session. Events that do
    /// not belong to one of the sender's sessions are dropped; returns whether it was passed.
    pub fn relay(&self, room: &Room, from: &str, name: &str, mut body: Value) -> bool {
        let rooms = self.0.lock().expect("rooms lock poisoned");
        let Some(members) = rooms.get(&room.device_id) else {
            return false;
        };
        let target = match &room.role {
            Role::Host => body
                .get("client_id")
                .and_then(Value::as_str)
                .and_then(|id| members.viewers.get(id)),
            Role::Viewer { .. } => {
                let ice_id = body.pointer("/data/ice_id").and_then(Value::as_str);
                let own = ice_id.is_some_and(|ice_id| {
                    members
                        .sessions
                        .get(ice_id)
                        .is_some_and(|session| session.viewer == from)
                });
                members.host.as_ref().filter(|_| own)
            }
        };
        let Some(target) = target else {
            return false;
        };
        body["client_id"] = from.into();
        target.event(name, body)
    }
}

impl Members {
    /// The host is gone for good: every viewer in a session with it is told the host left.
    fn end_sessions_with_host(&mut self, host_id: &str) {
        for (ice_id, session) in self.sessions.drain() {
            if let Some(viewer) = self.viewers.get(&session.viewer) {
                viewer.event("left", json!({ "ice_id": ice_id, "client_id": host_id }));
            }
        }
    }

    /// Ends one session as if each side had left the other.
    fn end_session(&mut self, ice_id: &str, host: &Peer) {
        let Some(session) = self.sessions.remove(ice_id) else {
            return;
        };
        host.event(
            "released",
            json!({ "ice_id": ice_id, "client_id": session.viewer }),
        );
        if let Some(viewer) = self.viewers.get(&session.viewer) {
            viewer.event("left", json!({ "ice_id": ice_id, "client_id": host.id }));
        }
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
    use remora_proto::socketio::{Decoder, Packet};

    use super::*;

    struct Member {
        room: Room,
        id: String,
        inbox: mpsc::Receiver<Outbound>,
    }

    fn member(rooms: &Rooms, role: Role, id: &str) -> Member {
        let room = Room {
            sub: "1".into(),
            device_id: "d".into(),
            role,
        };
        let (outbox, inbox) = mpsc::channel(8);
        rooms.join(
            &room,
            Peer {
                id: id.into(),
                outbox,
            },
        );
        Member {
            room,
            id: id.into(),
            inbox,
        }
    }

    impl Member {
        /// The next queued event, decoded, with its attachments.
        fn event(&mut self) -> remora_proto::socketio::Event {
            let Ok(Outbound::Frames(frames)) = self.inbox.try_recv() else {
                panic!("expected frames");
            };
            let mut decoder = Decoder::new();
            let mut packet = None;
            for frame in frames {
                packet = match frame {
                    Frame::Text(text) => decoder.text(&text).unwrap(),
                    Frame::Binary(bytes) => decoder.binary(&bytes).unwrap(),
                };
            }
            let Some(Packet::Event(event)) = packet else {
                panic!("expected an event");
            };
            event
        }

        fn idle(&mut self) -> bool {
            self.inbox.try_recv().is_err()
        }
    }

    fn viewer_on(device: Option<&str>) -> Role {
        Role::Viewer {
            from_device: device.map(str::to_owned),
            take_over: false,
        }
    }

    fn no_ice() -> Grant {
        Grant {
            servers: json!([]),
            force_relay: false,
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

    #[test]
    fn control_reaches_the_host_and_soac_flows_both_ways() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let mut viewer = member(&rooms, viewer_on(None), "viewer");
        let header =
            json!({ "app_control_id": "app", "streamer_data": "{}", "app_data": placeholder(0) });
        let options = windows_viewer_options();

        let ack = rooms
            .control(&viewer.room, &viewer.id, &header, &options, &no_ice())
            .unwrap();
        let ice_id = ack["ice_id"].as_str().unwrap().to_owned();
        assert_eq!(ack["client_id"], "host");
        assert_eq!(ack["app_control_id"], "app");
        assert_eq!(ack["iceServers"], json!([]));

        let be_controlled = host.event();
        assert_eq!(be_controlled.name, "be-controlled");
        let header = &be_controlled.args[0];
        assert_eq!(header["ice_id"], ice_id.as_str());
        assert_eq!(header["client_id"], "viewer");
        assert_eq!(header["app_control_id"], "app");
        assert_eq!(header["streamer_data"], "{}");
        assert_eq!(header["controller_platform"], 1);
        assert_eq!(
            be_controlled.attachment(&header["app_data"]),
            Some(&options[..])
        );

        let offer = json!({ "client_id": "host", "data": { "type": "offer", "ice_id": ice_id } });
        assert!(rooms.relay(&viewer.room, &viewer.id, "soac", offer));
        let offer = host.event();
        assert_eq!(
            offer.args[0]["client_id"], "viewer",
            "rewritten to the sender"
        );
        assert_eq!(offer.args[0]["data"]["type"], "offer");

        let answer =
            json!({ "client_id": "viewer", "data": { "type": "answer", "ice_id": ice_id } });
        assert!(rooms.relay(&host.room, &host.id, "soac", answer));
        let answer = viewer.event();
        assert_eq!(answer.args[0]["client_id"], "host");
        assert_eq!(answer.args[0]["data"]["type"], "answer");
    }

    #[test]
    fn control_needs_the_host_online() {
        let rooms = Rooms::default();
        let viewer = member(&rooms, viewer_on(None), "viewer");
        let options = windows_viewer_options();
        let refused = rooms.control(&viewer.room, &viewer.id, &json!({}), &options, &no_ice());
        assert_eq!(refused, Err(Refused::HostOffline));
        let elsewhere = Room {
            device_id: "other".into(),
            ..viewer.room.clone()
        };
        assert_eq!(
            rooms.control(&elsewhere, &viewer.id, &json!({}), &options, &no_ice()),
            Err(Refused::HostOffline)
        );
    }

    #[test]
    fn relays_only_within_a_session() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let mut viewer = member(&rooms, viewer_on(None), "viewer");
        let mut other = member(&rooms, viewer_on(None), "other");
        let ack = rooms
            .control(&viewer.room, &viewer.id, &json!({}), &[], &no_ice())
            .unwrap();
        let ice_id = ack["ice_id"].clone();
        host.event();

        let soac = |ice_id: &Value| json!({ "data": { "type": "candidate", "ice_id": ice_id } });
        assert!(!rooms.relay(&other.room, &other.id, "soac", soac(&ice_id)));
        assert!(!rooms.relay(&viewer.room, &viewer.id, "soac", soac(&json!("unknown"))));
        assert!(host.idle());

        let to_nobody = json!({ "client_id": "nobody", "data": {} });
        assert!(!rooms.relay(&host.room, &host.id, "soac", to_nobody));
        assert!(viewer.idle() && other.idle());
    }

    /// Opens a session from `viewer`, and returns its `ice_id`.
    fn open_session(rooms: &Rooms, host: &mut Member, viewer: &Member) -> Value {
        let ice_id = rooms
            .control(&viewer.room, &viewer.id, &json!({}), &[], &no_ice())
            .unwrap()["ice_id"]
            .clone();
        assert_eq!(host.event().name, "be-controlled");
        ice_id
    }

    #[test]
    fn a_viewer_leaving_releases_its_sessions() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let viewer = member(&rooms, viewer_on(None), "viewer");
        let ice_id = open_session(&rooms, &mut host, &viewer);

        assert_eq!(rooms.leave(&viewer.room, &viewer.id), None);
        let released = host.event();
        assert_eq!(released.name, "released");
        assert_eq!(
            released.args[0],
            json!({ "ice_id": ice_id, "client_id": "viewer" })
        );
        assert!(!rooms.controlled_elsewhere("d", None));
    }

    #[test]
    fn a_host_that_does_not_reconnect_ends_its_sessions() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let mut viewer = member(&rooms, viewer_on(None), "viewer");
        let ice_id = open_session(&rooms, &mut host, &viewer);

        let departure = rooms.leave(&host.room, &host.id).unwrap();
        assert!(!rooms.is_online("d"));
        assert!(viewer.idle(), "the session waits for the host");
        assert!(rooms.controlled_elsewhere("d", None));

        rooms.expire(&departure);
        let left = viewer.event();
        assert_eq!(left.name, "left");
        assert_eq!(
            left.args[0],
            json!({ "ice_id": ice_id, "client_id": "host" })
        );
        assert!(!rooms.controlled_elsewhere("d", None));
    }

    #[test]
    fn a_host_without_sessions_leaves_nothing_behind() {
        let rooms = Rooms::default();
        let host = member(&rooms, Role::Host, "host");
        assert_eq!(rooms.leave(&host.room, &host.id), None);
    }

    #[test]
    fn a_reconnecting_host_takes_its_sessions_back() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let mut viewer = member(&rooms, viewer_on(None), "viewer");
        let ice_id = open_session(&rooms, &mut host, &viewer);
        let departure = rooms.leave(&host.room, &host.id).unwrap();

        let mut back = member(&rooms, Role::Host, "back");
        rooms.expire(&departure);
        assert!(
            viewer.idle(),
            "expiring a departure taken over does nothing"
        );

        let candidate = json!({ "data": { "type": "candidate", "ice_id": ice_id } });
        assert!(rooms.relay(&viewer.room, &viewer.id, "soac", candidate));
        assert_eq!(back.event().name, "soac");
        assert!(rooms.leave(&viewer.room, &viewer.id).is_none());
        assert_eq!(back.event().name, "released");
    }

    #[test]
    fn a_newer_host_kicks_the_older_one_and_takes_its_sessions() {
        let rooms = Rooms::default();
        let mut older = member(&rooms, Role::Host, "older");
        let mut viewer = member(&rooms, viewer_on(None), "viewer");
        let ice_id = open_session(&rooms, &mut older, &viewer);

        let mut newer = member(&rooms, Role::Host, "newer");
        assert_eq!(older.inbox.try_recv(), Ok(Outbound::Kicked));
        assert!(viewer.idle());

        assert_eq!(rooms.leave(&older.room, &older.id), None);
        assert!(
            rooms.is_online("d"),
            "the older host leaving keeps the newer one"
        );
        let candidate = json!({ "data": { "type": "candidate", "ice_id": ice_id } });
        assert!(rooms.relay(&viewer.room, &viewer.id, "soac", candidate));
        assert_eq!(newer.event().name, "soac");
    }

    #[test]
    fn kicking_the_host_ends_its_sessions_at_once() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let mut viewer = member(&rooms, viewer_on(None), "viewer");
        open_session(&rooms, &mut host, &viewer);

        rooms.kick_host("d");
        assert_eq!(host.inbox.try_recv(), Ok(Outbound::Kicked));
        assert_eq!(viewer.event().name, "left");
        assert_eq!(rooms.leave(&host.room, &host.id), None);

        let mut host = member(&rooms, Role::Host, "host");
        open_session(&rooms, &mut host, &viewer);
        let _departure = rooms.leave(&host.room, &host.id).unwrap();
        rooms.kick_host("d");
        assert_eq!(viewer.event().name, "left", "also while the host is away");
    }

    fn options(capture_type: i32) -> Vec<u8> {
        ConnectOptions {
            capture_type,
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn one_device_at_a_time_controls_the_desktop() {
        const DESKTOP: i32 = 1;
        const TERMINAL: i32 = 8;
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let phone = member(&rooms, viewer_on(Some("phone")), "phone");
        let laptop = member(&rooms, viewer_on(Some("laptop")), "laptop");
        let control = |viewer: &Member, capture_type| {
            rooms.control(
                &viewer.room,
                &viewer.id,
                &json!({}),
                &options(capture_type),
                &no_ice(),
            )
        };

        control(&phone, DESKTOP).unwrap();
        host.event();
        assert!(rooms.controlled_elsewhere("d", Some("laptop")));
        assert!(rooms.controlled_elsewhere("d", None));
        assert!(!rooms.controlled_elsewhere("d", Some("phone")));

        assert_eq!(control(&laptop, DESKTOP), Err(Refused::ControlledElsewhere));
        assert!(host.idle());
        control(&laptop, TERMINAL).unwrap();
        host.event();
        control(&phone, DESKTOP).unwrap();
        host.event();
    }

    #[test]
    fn taking_over_ends_the_other_devices_desktop_sessions() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let mut phone = member(&rooms, viewer_on(Some("phone")), "phone");
        let laptop = member(
            &rooms,
            Role::Viewer {
                from_device: Some("laptop".into()),
                take_over: true,
            },
            "laptop",
        );
        let desktop = options(1);
        let ice_id = rooms
            .control(&phone.room, &phone.id, &json!({}), &desktop, &no_ice())
            .unwrap()["ice_id"]
            .clone();
        host.event();

        rooms
            .control(&laptop.room, &laptop.id, &json!({}), &desktop, &no_ice())
            .unwrap();
        let released = host.event();
        assert_eq!(released.name, "released");
        assert_eq!(
            released.args[0],
            json!({ "ice_id": ice_id, "client_id": "phone" })
        );
        assert_eq!(host.event().name, "be-controlled");
        let left = phone.event();
        assert_eq!(left.name, "left");
        assert_eq!(
            left.args[0],
            json!({ "ice_id": ice_id, "client_id": "host" })
        );
        assert!(!rooms.controlled_elsewhere("d", Some("laptop")));
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
