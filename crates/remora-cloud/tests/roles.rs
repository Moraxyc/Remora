mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use remora_cloud::bus::{Bus, BusMessage, TOPIC_PRESENCE};
use remora_cloud::{CloudRole, Config, Database, Ice, LocalBus, Roles, TurnSecret};
use remora_proto::signal::{Decoder, Event, Packet};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

async fn serve_router(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());
    addr
}

struct WsClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    decoder: Decoder,
}

impl WsClient {
    async fn connect_with_headers(
        addr: SocketAddr,
        token: &str,
        extra_headers: &[(&str, &str)],
    ) -> Self {
        let url = format!("ws://{addr}/socket.io/?transport=websocket&EIO=4");
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("X-NRD-AUTH", token.parse().unwrap());
        for (name, value) in extra_headers {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let mut client = Self {
            socket,
            decoder: Decoder::new(),
        };
        assert!(matches!(client.recv().await, Some(Packet::Open(_))));
        client.send("40").await;
        assert_eq!(client.recv().await, Some(Packet::Connected));
        client
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

    async fn event(&mut self) -> Event {
        loop {
            match self.recv().await {
                Some(Packet::Event(event)) => return event,
                Some(Packet::Ping) => self.send("3").await,
                other => panic!("expected an event, got {other:?}"),
            }
        }
    }
}

fn turn_ice() -> Ice {
    Ice::new(
        vec!["turn:turn.test:3478".parse().unwrap()],
        Some(TurnSecret::new("roles-turn-secret")),
    )
    .and_then(Ice::force_relay)
    .unwrap()
}

async fn status_of(app: &Router, path: &str) -> StatusCode {
    let req = Request::get(path).body(Body::empty()).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn non_overlapping_api_relay_and_gateway_room_communicate_over_local_bus() {
    let bus = Arc::new(LocalBus::new());
    let mut presence_sub = bus.subscribe(TOPIC_PRESENCE);
    let database = Database::in_memory();

    // Instance A: API + Relay (no Gateway, no Room)
    let api_relay = remora_cloud::router_with_bus(
        Config {
            roles: Roles::from_iter([CloudRole::Api, CloudRole::Relay]),
            database: database.clone(),
            ice: turn_ice(),
            ..common::config()
        },
        bus.clone(),
    );
    // Instance B: Gateway + Room (no Api, no Relay)
    let gateway_room = remora_cloud::router_with_bus(
        Config {
            roles: Roles::from_iter([CloudRole::Gateway, CloudRole::Room]),
            database,
            ice: Ice::default(),
            ..common::config()
        },
        bus.clone(),
    );

    // Role route isolation checks.
    assert_eq!(
        status_of(&api_relay, "/socket.io/?EIO=4&transport=websocket").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(&gateway_room, "/api/v1/device/list").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(status_of(&gateway_room, "/").await, StatusCode::NOT_FOUND);

    let gw_addr = serve_router(gateway_room).await;

    // Host registers, logs in, and creates a room via Instance A (with profile headers).
    let init_req = Request::post("/api/v1/device/windows/init")
        .header("content-type", "application/json")
        .header("X-Param-PLAT", "4")
        .header("X-Param-VN", "4.48.2")
        .header("X-Param-VC", "634")
        .body(Body::from(
            json!({ "name": "mac-host", "client_id": "client-split-host" }).to_string(),
        ))
        .unwrap();
    let init_res = api_relay.clone().oneshot(init_req).await.unwrap();
    assert_eq!(init_res.status(), StatusCode::OK);

    let device_id = common::register(&api_relay, "client-split-host").await;
    let token = common::log_in(&api_relay).await;
    let room_token = common::create_room(&api_relay, &token, &device_id).await;

    // Host connects to Instance B (Gateway + Room) with profile headers.
    let mut host = WsClient::connect_with_headers(
        gw_addr,
        &room_token,
        &[
            ("X-Param-PLAT", "4"),
            ("X-Param-VN", "4.48.2"),
            ("X-Param-VC", "634"),
        ],
    )
    .await;
    host.send(r#"421["room_info"]"#).await;
    assert!(matches!(host.recv().await, Some(Packet::Ack(_))));
    assert_eq!(host.event().await.name, "bmsg_push");

    // Verify PresenceChanged(online=true) was published on the shared bus.
    let Some(BusMessage::PresenceChanged(p)) = presence_sub.recv().await else {
        panic!("expected PresenceChanged");
    };
    assert_eq!(p.device_id, device_id);
    assert!(p.online);

    // Instance A (Api) sees the host online in Room (Instance B) over the bus.
    let list: Value = common::get_as(&api_relay, "/api/v1/device/list", &token, &device_id)
        .await
        .unwrap();
    assert_eq!(list["my_binded_devices"][0]["status"], "CONNECTED");

    // Viewer joins on Instance A and connects signaling on Instance B.
    let joined: Value = common::post_as(
        &api_relay,
        &format!("/api/v1/room/join/by_device/{device_id}"),
        &token,
        None,
        r#"{"force_join":false}"#,
    )
    .await
    .unwrap();
    let mut viewer =
        WsClient::connect_with_headers(gw_addr, joined["token"].as_str().unwrap(), &[]).await;

    // Viewer sends control; Room (Instance B) requests TURN grant from Relay (Instance A).
    let header = json!({
        "app_control_id": "split-app",
        "app_data": { "_placeholder": true, "num": 0 },
    });
    viewer
        .send(&format!("451-9{}", json!(["control", header])))
        .await;
    viewer.send_binary(&[0x08, 0x01, 0x40, 0x04]).await;

    let Some(Packet::Ack(ack)) = viewer.recv().await else {
        panic!("expected control ack");
    };
    assert_eq!((ack.id, &ack.args[0]), (9, &json!("success")));
    let granted = &ack.args[1];
    assert_eq!(granted["force_relay"], true);
    assert_eq!(
        granted["iceServers"][0]["urls"],
        json!(["turn:turn.test:3478"])
    );
    let ice_id = granted["ice_id"].as_str().unwrap().to_owned();
    let host_id = granted["client_id"].as_str().unwrap().to_owned();

    let be_controlled = host.event().await;
    assert_eq!(be_controlled.name, "be-controlled");
    assert_eq!(be_controlled.args[0]["ice_id"], ice_id);
    assert_eq!(be_controlled.args[0]["controller_platform"], 4);
    assert_eq!(be_controlled.args[0]["force_relay"], true);

    // Bidirectional soac relay across Gateway + Room.
    let offer = json!({ "client_id": host_id, "data": { "type": "offer", "ice_id": ice_id } });
    viewer.send(&format!("42{}", json!(["soac", offer]))).await;
    let relayed_offer = host.event().await;
    assert_eq!(relayed_offer.name, "soac");
    assert_eq!(relayed_offer.args[0]["data"]["type"], "offer");

    // Unbind on Instance A kicks the host on Instance B via bus.
    let unbound: Value = common::post_as(
        &api_relay,
        &format!("/api/v1/device/{device_id}/unbind"),
        &token,
        None,
        "",
    )
    .await
    .unwrap();
    assert_eq!(unbound, json!({}));

    let host_left = host.event().await;
    assert_eq!(host_left.name, "left");
    assert_eq!(host.recv().await, None);
    let viewer_left = viewer.event().await;
    assert_eq!(viewer_left.name, "left");
    assert_eq!(viewer_left.args[0]["ice_id"], ice_id);
}

#[tokio::test]
async fn non_overlapping_api_room_and_gateway_relay_communicate_over_local_bus() {
    let bus = Arc::new(LocalBus::new());
    let database = Database::in_memory();

    // Instance A: Api + Room
    let api_room = remora_cloud::router_with_bus(
        Config {
            roles: Roles::from_iter([CloudRole::Api, CloudRole::Room]),
            database: database.clone(),
            ice: Ice::default(),
            ..common::config()
        },
        bus.clone(),
    );
    // Instance B: Gateway + Relay
    let gateway_relay = remora_cloud::router_with_bus(
        Config {
            roles: Roles::from_iter([CloudRole::Gateway, CloudRole::Relay]),
            database,
            ice: turn_ice(),
            ..common::config()
        },
        bus,
    );

    let gw_addr = serve_router(gateway_relay).await;
    let device_id = common::register(&api_room, "client-split-2").await;
    let token = common::log_in(&api_room).await;
    let room_token = common::create_room(&api_room, &token, &device_id).await;

    let mut host = WsClient::connect_with_headers(gw_addr, &room_token, &[]).await;
    host.send(r#"421["room_info"]"#).await;
    assert!(matches!(host.recv().await, Some(Packet::Ack(_))));
    assert_eq!(host.event().await.name, "bmsg_push");

    let joined: Value = common::post_as(
        &api_room,
        &format!("/api/v1/room/join/by_device/{device_id}"),
        &token,
        None,
        r#"{"force_join":false}"#,
    )
    .await
    .unwrap();
    let mut viewer =
        WsClient::connect_with_headers(gw_addr, joined["token"].as_str().unwrap(), &[]).await;

    let header = json!({ "app_data": { "_placeholder": true, "num": 0 } });
    viewer
        .send(&format!("451-5{}", json!(["control", header])))
        .await;
    viewer.send_binary(&[0x08, 0x01]).await;

    let Some(Packet::Ack(ack)) = viewer.recv().await else {
        panic!("expected control ack");
    };
    assert_eq!(ack.args[0], "success");
    assert_eq!(ack.args[1]["force_relay"], true);
    assert_eq!(
        ack.args[1]["iceServers"][0]["urls"],
        json!(["turn:turn.test:3478"])
    );
    assert_eq!(host.event().await.name, "be-controlled");
}
