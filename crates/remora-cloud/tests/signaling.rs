mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use remora_cloud::{Config, Ice, Ping, TurnSecret};
use remora_proto::cloud::{ApiError, SignKey};
use remora_proto::socketio::{Decoder, Event, Packet};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Serves `config` on a free port. The returned router shares the served state.
async fn start(config: Config) -> (Router, SocketAddr) {
    let app = remora_cloud::router(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    (app, addr)
}

/// A room token for a freshly registered, logged-in host.
async fn room_token(app: &Router, client_id: &str) -> String {
    let device_id = common::register(app, client_id).await;
    let token = common::log_in(app).await;
    common::create_room(app, &token, &device_id).await
}

struct Client {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    decoder: Decoder,
}

impl Client {
    async fn connect(addr: SocketAddr, token: &str) -> Result<Self, Error> {
        let url = format!("ws://{addr}/socket.io/?transport=websocket&EIO=4");
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("X-NRD-AUTH", token.parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await?;
        Ok(Self {
            socket,
            decoder: Decoder::new(),
        })
    }

    async fn send(&mut self, frame: &str) {
        self.socket.send(Message::text(frame)).await.unwrap();
    }

    async fn send_binary(&mut self, bytes: &[u8]) {
        self.socket
            .send(Message::binary(bytes.to_vec()))
            .await
            .unwrap();
    }

    /// The next event, skipping pings.
    async fn event(&mut self) -> Event {
        loop {
            match self.recv().await {
                Some(Packet::Event(event)) => return event,
                Some(Packet::Ping) => self.send("3").await,
                other => panic!("expected an event, got {other:?}"),
            }
        }
    }

    /// The next packet, or `None` once the server closed the connection.
    async fn recv(&mut self) -> Option<Packet> {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.socket.next())
                .await
                .expect("server answered in time");
            let packet = match frame {
                Some(Ok(Message::Text(text))) => self.decoder.text(&text).unwrap(),
                Some(Ok(Message::Binary(bytes))) => self.decoder.binary(&bytes).unwrap(),
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                Some(Ok(_)) => continue,
            };
            if packet.is_some() {
                return packet;
            }
        }
    }

    /// Completes the Engine.IO and Socket.IO handshakes.
    async fn handshake(&mut self) {
        assert!(matches!(self.recv().await, Some(Packet::Open(_))));
        self.send("40").await;
        assert_eq!(self.recv().await, Some(Packet::Connected));
    }
}

#[tokio::test]
async fn host_comes_online() {
    let (app, addr) = start(common::config()).await;
    let token = room_token(&app, "client-a").await;
    let mut host = Client::connect(addr, &token).await.unwrap();

    let Some(Packet::Open(open)) = host.recv().await else {
        panic!("expected the open packet");
    };
    assert_eq!((open.ping_interval, open.ping_timeout), (25_000, 20_000));
    host.send("40").await;
    assert_eq!(host.recv().await, Some(Packet::Connected));

    host.send(r#"4211["room_info"]"#).await;
    let Some(Packet::Ack(ack)) = host.recv().await else {
        panic!("expected the room_info ack");
    };
    assert_eq!(ack.id, 11);
    let Some(Packet::Event(status)) = host.recv().await else {
        panic!("expected the device status");
    };
    assert_eq!(status.name, "bmsg_push");
    assert_eq!(
        status.args[0]["data"]["publisher_availability_status"],
        "ready"
    );
    assert!(status.args[0].get("type").is_none());

    host.send(r#"4212["refresh_reconnect_key"]"#).await;
    let Some(Packet::Ack(ack)) = host.recv().await else {
        panic!("expected the reconnect key ack");
    };
    assert_eq!(ack.id, 12);
    assert_eq!(ack.args[0], "success");
    assert!(
        ack.args[1]["reconnect_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );
}

#[tokio::test]
async fn the_device_list_shows_hosts_on_signaling_as_connected() {
    let (app, addr) = start(common::config()).await;
    let device_id = common::register(&app, "client-a").await;
    let token = common::log_in(&app).await;
    let room = common::create_room(&app, &token, &device_id).await;
    let status = async || {
        let list: Value = common::get_as(&app, "/api/v1/device/list", &token, &device_id)
            .await
            .unwrap();
        list["my_binded_devices"][0]["status"].clone()
    };
    assert_eq!(status().await, "DISCONNECTED");

    let mut host = Client::connect(addr, &room).await.unwrap();
    host.handshake().await;
    assert_eq!(status().await, "CONNECTED");

    drop(host);
    for _ in 0..50 {
        if status().await == "DISCONNECTED" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the host stayed connected after hanging up");
}

#[tokio::test]
async fn needs_a_valid_room_token() {
    let (app, addr) = start(common::config()).await;
    let user_token = common::log_in(&app).await;

    for token in ["bogus", &user_token] {
        let Err(Error::Http(response)) = Client::connect(addr, token).await else {
            panic!("expected the upgrade to be refused");
        };
        assert_eq!(response.status(), 401);
    }
}

#[tokio::test]
async fn signaling_is_not_subject_to_request_signing() {
    let config = Config {
        sign_key: Some(SignKey::new("test-sign-key")),
        ..common::config()
    };
    let (_, addr) = start(config).await;

    // Refused for the token, not for a missing signature.
    let Err(Error::Http(response)) = Client::connect(addr, "bogus").await else {
        panic!("expected the upgrade to be refused");
    };
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn hosts_that_stop_answering_pings_are_dropped() {
    let ping = Ping {
        interval: Duration::from_millis(100),
        timeout: Duration::from_millis(100),
    };
    let (app, addr) = start(Config {
        ping,
        ..common::config()
    })
    .await;
    let token = room_token(&app, "client-a").await;
    let mut host = Client::connect(addr, &token).await.unwrap();
    host.handshake().await;

    assert_eq!(host.recv().await, Some(Packet::Ping));
    host.send("3").await;
    assert_eq!(host.recv().await, Some(Packet::Ping));
    assert_eq!(host.recv().await, None, "no pong, so the server hangs up");
}

#[tokio::test]
async fn a_newer_connection_kicks_the_older_one() {
    let (app, addr) = start(common::config()).await;
    let token = room_token(&app, "client-a").await;
    let mut older = Client::connect(addr, &token).await.unwrap();
    older.handshake().await;

    let mut newer = Client::connect(addr, &token).await.unwrap();
    newer.handshake().await;

    let Some(Packet::Event(left)) = older.recv().await else {
        panic!("expected left");
    };
    assert_eq!(
        (left.name.as_str(), &left.args[..]),
        ("left", &[json!({})][..])
    );
    assert_eq!(older.recv().await, None);

    newer.send(r#"421["room_info"]"#).await;
    assert!(matches!(newer.recv().await, Some(Packet::Ack(_))));
}

/// Logs a host in, registers it, and brings it online; returns the user token and device id.
async fn online_host(app: &Router, addr: SocketAddr) -> (Client, String, String) {
    let device_id = common::register(app, "client-host").await;
    let token = common::log_in(app).await;
    let room = common::create_room(app, &token, &device_id).await;
    let mut host = Client::connect(addr, &room).await.unwrap();
    host.handshake().await;
    host.send(r#"421["room_info"]"#).await;
    assert!(matches!(host.recv().await, Some(Packet::Ack(_))));
    assert_eq!(host.event().await.name, "bmsg_push");
    (host, token, device_id)
}

async fn join(app: &Router, token: &str, device_id: &str) -> Result<Value, ApiError> {
    let path = format!("/api/v1/room/join/by_device/{device_id}");
    common::post_as(app, &path, token, None, r#"{"force_join":false}"#).await
}

#[tokio::test]
async fn joining_needs_an_own_online_device() {
    let (app, addr) = start(common::config()).await;
    let device_id = common::register(&app, "client-host").await;
    let token = common::log_in(&app).await;

    let unknown = Err(ApiError::Rejected {
        http_status: 400,
        code: 2003,
    });
    assert_eq!(join(&app, &token, "unknown").await.map(drop), unknown);
    assert_eq!(
        join(&app, &token, &device_id).await.map(drop),
        unknown,
        "never created a room"
    );

    let room = common::create_room(&app, &token, &device_id).await;
    let offline = Err(ApiError::Rejected {
        http_status: 400,
        code: 2001,
    });
    assert_eq!(join(&app, &token, &device_id).await.map(drop), offline);

    let mut host = Client::connect(addr, &room).await.unwrap();
    host.handshake().await;
    let joined = join(&app, &token, &device_id).await.unwrap();
    assert_eq!(joined["signaling_list"], json!(["wss://remora.test"]));
    assert_eq!(joined["publisher_platform"], 1);
    assert_eq!(
        (&joined["report_token"], &joined["report_url"]),
        (&json!(""), &json!(""))
    );
    assert!(
        joined["token"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
    );

    assert_eq!(
        join(&app, "bogus", &device_id).await.map(drop),
        Err(ApiError::TokenExpired)
    );
}

#[tokio::test]
async fn a_viewer_controls_the_host_through_the_cloud() {
    let (app, addr) = start(common::config()).await;
    let (mut host, token, device_id) = online_host(&app, addr).await;
    let joined = join(&app, &token, &device_id).await.unwrap();
    let mut viewer = Client::connect(addr, joined["token"].as_str().unwrap())
        .await
        .unwrap();
    viewer.handshake().await;

    // ConnectOptions { capture_type: CT_DESKTOP, client_type: Client_WINDOWS }.
    let connect_options = [0x08, 0x01, 0x40, 0x03];
    let header = json!({
        "app_control_id": "app",
        "app_data": { "_placeholder": true, "num": 0 },
        "streamer_data": "{}",
    });
    viewer
        .send(&format!("451-11{}", json!(["control", header])))
        .await;
    viewer.send_binary(&connect_options).await;

    let Some(Packet::Ack(ack)) = viewer.recv().await else {
        panic!("expected the control ack");
    };
    assert_eq!((ack.id, &ack.args[0]), (11, &json!("success")));
    let granted = &ack.args[1];
    let ice_id = granted["ice_id"].as_str().unwrap();
    let host_id = granted["client_id"].as_str().unwrap();
    assert_eq!(granted["app_control_id"], "app");
    assert!(granted["iceServers"].is_array());

    let be_controlled = host.event().await;
    assert_eq!(be_controlled.name, "be-controlled");
    let request = &be_controlled.args[0];
    assert_eq!(request["ice_id"], ice_id);
    assert_eq!(request["controller_platform"], 1);
    assert_eq!(
        be_controlled.attachment(&request["app_data"]),
        Some(&connect_options[..])
    );
    let viewer_id = request["client_id"].as_str().unwrap();

    let offer = json!({ "client_id": host_id, "data": { "type": "offer", "sdp": "v=0", "ice_id": ice_id, "app_control_id": "app" } });
    viewer.send(&format!("42{}", json!(["soac", offer]))).await;
    let offer = host.event().await;
    assert_eq!(offer.name, "soac");
    assert_eq!(offer.args[0]["client_id"], viewer_id);
    assert_eq!(offer.args[0]["data"]["sdp"], "v=0");

    let answer = json!({ "client_id": viewer_id, "data": { "type": "answer", "sdp": "v=0", "ice_id": ice_id, "client_id": viewer_id, "app_control_id": "app" } });
    host.send(&format!("42{}", json!(["soac", answer]))).await;
    let answer = viewer.event().await;
    assert_eq!(answer.args[0]["client_id"], host_id);
    assert_eq!(answer.args[0]["data"]["type"], "answer");

    drop(viewer);
    let released = host.event().await;
    assert_eq!(released.name, "released");
    assert_eq!(released.args[0]["ice_id"], ice_id);
}

#[tokio::test]
async fn control_fails_once_the_host_is_gone() {
    let (app, addr) = start(common::config()).await;
    let (host, token, device_id) = online_host(&app, addr).await;
    let joined = join(&app, &token, &device_id).await.unwrap();
    drop(host);

    let mut viewer = Client::connect(addr, joined["token"].as_str().unwrap())
        .await
        .unwrap();
    viewer.handshake().await;
    for _ in 0..50 {
        viewer
            .send(&format!(
                "451-7{}",
                json!(["control", { "app_data": { "_placeholder": true, "num": 0 } }])
            ))
            .await;
        viewer.send_binary(&[]).await;
        let Some(Packet::Ack(ack)) = viewer.recv().await else {
            panic!("expected the control ack");
        };
        if ack.args[0] == "failure" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("control kept succeeding without a host");
}

#[tokio::test]
async fn both_sides_get_the_same_turn_settings() {
    let ice = Ice::new(
        vec!["turn:turn.test:3478".parse().unwrap()],
        Some(TurnSecret::new("test-turn-secret")),
    )
    .and_then(Ice::force_relay)
    .unwrap();
    let (app, addr) = start(Config {
        ice,
        ..common::config()
    })
    .await;
    let (mut host, token, device_id) = online_host(&app, addr).await;
    let joined = join(&app, &token, &device_id).await.unwrap();
    let mut viewer = Client::connect(addr, joined["token"].as_str().unwrap())
        .await
        .unwrap();
    viewer.handshake().await;

    let header = json!({ "app_data": { "_placeholder": true, "num": 0 } });
    viewer
        .send(&format!("451-3{}", json!(["control", header])))
        .await;
    viewer.send_binary(&[]).await;
    let Some(Packet::Ack(ack)) = viewer.recv().await else {
        panic!("expected the control ack");
    };
    let servers = &ack.args[1]["iceServers"];
    assert_eq!(servers[0]["urls"], json!(["turn:turn.test:3478"]));
    assert!(
        servers[0]["username"]
            .as_str()
            .is_some_and(|name| name.ends_with(":1"))
    );
    assert!(
        servers[0]["credential"]
            .as_str()
            .is_some_and(|c| !c.is_empty())
    );

    assert_eq!(ack.args[1]["force_relay"], true);

    let be_controlled = host.event().await;
    assert_eq!(&be_controlled.args[0]["iceServers"], servers);
    assert_eq!(be_controlled.args[0]["force_relay"], true);
}
