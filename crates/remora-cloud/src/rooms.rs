//! Who is in each device's room, and how signaling messages are routed between them.
//!
//! A room holds the device's host and any viewers that joined it. A viewer's `control` opens a
//! session with the host, named by an `ice_id` the cloud assigns. After that, `soac` and
//! `forward_setting` pass between the two, each addressed by the receiver's `client_id`. On
//! the way, `client_id` is rewritten to the sender's, which is what the receiver answers to.

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
    viewers: HashMap<String, Peer>,
    /// Open sessions: the viewer's client id, by `ice_id`.
    sessions: HashMap<String, String>,
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
    /// Another connection took over this device.
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

/// Why a `control` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    HostOffline,
}

impl Rooms {
    pub fn join(&self, room: &Room, peer: Peer) {
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let members = rooms.entry(room.device_id.clone()).or_default();
        match room.role {
            Role::Host => {
                if let Some(previous) = members.host.replace(peer) {
                    // A full outbox means that connection is stuck; it times out by itself.
                    let _ = previous.outbox.try_send(Outbound::Kicked);
                    members.end_sessions_with_host(&previous.id);
                }
            }
            Role::Viewer => {
                members.viewers.insert(peer.id.clone(), peer);
            }
        }
    }

    /// Removes a connection and tells the other side of each of its sessions.
    pub fn leave(&self, room: &Room, id: &str) {
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let Some(members) = rooms.get_mut(&room.device_id) else {
            return;
        };
        match room.role {
            Role::Host if members.host.as_ref().is_some_and(|host| host.id == id) => {
                members.host = None;
                members.end_sessions_with_host(id);
            }
            Role::Host => {}
            Role::Viewer => {
                members.viewers.remove(id);
                let host = members.host.as_ref();
                members.sessions.retain(|ice_id, viewer| {
                    if viewer != id {
                        return true;
                    }
                    if let Some(host) = host {
                        host.event("released", json!({ "ice_id": ice_id, "client_id": id }));
                    }
                    false
                });
            }
        }
        if members.host.is_none() && members.viewers.is_empty() {
            rooms.remove(&room.device_id);
        }
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        let rooms = self.0.lock().expect("rooms lock poisoned");
        rooms
            .get(device_id)
            .is_some_and(|members| members.host.is_some())
    }

    /// A viewer asks to control the host: forwards `be-controlled` with the viewer's
    /// `ConnectOptions`, and returns what the viewer's `control` is acknowledged with.
    pub fn control(
        &self,
        device_id: &str,
        viewer_id: &str,
        header: &Value,
        connect_options: &[u8],
        ice: &Grant,
    ) -> Result<Value, Refused> {
        let mut rooms = self.0.lock().expect("rooms lock poisoned");
        let members = rooms.get_mut(device_id).ok_or(Refused::HostOffline)?;
        let host = members.host.as_ref().ok_or(Refused::HostOffline)?;

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
        members.sessions.insert(ice_id, viewer_id.to_owned());
        Ok(ack)
    }

    /// Passes an event from connection `from` to the other side of its session. Events that do
    /// not belong to one of the sender's sessions are dropped; returns whether it was passed.
    pub fn relay(&self, room: &Room, from: &str, name: &str, mut body: Value) -> bool {
        let rooms = self.0.lock().expect("rooms lock poisoned");
        let Some(members) = rooms.get(&room.device_id) else {
            return false;
        };
        let target = match room.role {
            Role::Host => body
                .get("client_id")
                .and_then(Value::as_str)
                .and_then(|id| members.viewers.get(id)),
            Role::Viewer => {
                let ice_id = body.pointer("/data/ice_id").and_then(Value::as_str);
                let own = ice_id.is_some_and(|ice_id| {
                    members.sessions.get(ice_id).map(String::as_str) == Some(from)
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
    /// The host went away: every viewer in a session with it is told the host left.
    fn end_sessions_with_host(&mut self, host_id: &str) {
        for (ice_id, viewer) in self.sessions.drain() {
            if let Some(viewer) = self.viewers.get(&viewer) {
                viewer.event("left", json!({ "ice_id": ice_id, "client_id": host_id }));
            }
        }
    }
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
        let mut viewer = member(&rooms, Role::Viewer, "viewer");
        let header =
            json!({ "app_control_id": "app", "streamer_data": "{}", "app_data": placeholder(0) });
        let options = windows_viewer_options();

        let ack = rooms
            .control("d", &viewer.id, &header, &options, &no_ice())
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
        let viewer = member(&rooms, Role::Viewer, "viewer");
        let options = windows_viewer_options();
        let refused = rooms.control("d", &viewer.id, &json!({}), &options, &no_ice());
        assert_eq!(refused, Err(Refused::HostOffline));
        assert_eq!(
            rooms.control("other", &viewer.id, &json!({}), &options, &no_ice()),
            Err(Refused::HostOffline)
        );
    }

    #[test]
    fn relays_only_within_a_session() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let mut viewer = member(&rooms, Role::Viewer, "viewer");
        let mut other = member(&rooms, Role::Viewer, "other");
        let ack = rooms
            .control("d", &viewer.id, &json!({}), &[], &no_ice())
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

    #[test]
    fn leaving_ends_the_other_sides_sessions() {
        let rooms = Rooms::default();
        let mut host = member(&rooms, Role::Host, "host");
        let viewer = member(&rooms, Role::Viewer, "viewer");
        let ice_id = rooms
            .control("d", &viewer.id, &json!({}), &[], &no_ice())
            .unwrap()["ice_id"]
            .clone();
        host.event();

        rooms.leave(&viewer.room, &viewer.id);
        let released = host.event();
        assert_eq!(released.name, "released");
        assert_eq!(
            released.args[0],
            json!({ "ice_id": ice_id, "client_id": "viewer" })
        );

        let mut viewer = member(&rooms, Role::Viewer, "viewer");
        let ice_id = rooms
            .control("d", &viewer.id, &json!({}), &[], &no_ice())
            .unwrap()["ice_id"]
            .clone();
        host.event();
        rooms.leave(&host.room, &host.id);
        let left = viewer.event();
        assert_eq!(left.name, "left");
        assert_eq!(
            left.args[0],
            json!({ "ice_id": ice_id, "client_id": "host" })
        );
        assert!(!rooms.is_online("d"));
    }

    #[test]
    fn a_newer_host_kicks_the_older_one() {
        let rooms = Rooms::default();
        let mut older = member(&rooms, Role::Host, "older");
        let mut viewer = member(&rooms, Role::Viewer, "viewer");
        rooms
            .control("d", &viewer.id, &json!({}), &[], &no_ice())
            .unwrap();
        older.event();

        let _newer = member(&rooms, Role::Host, "newer");
        assert_eq!(older.inbox.try_recv(), Ok(Outbound::Kicked));
        assert_eq!(viewer.event().name, "left");

        rooms.leave(&older.room, &older.id);
        assert!(
            rooms.is_online("d"),
            "the older host leaving keeps the newer one"
        );
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
