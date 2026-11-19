mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use remora_cloud::{Config, Ping};
use remora_proto::cloud::SignKey;
use remora_proto::socketio::{Decoder, Packet};
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
    let room: Value = common::post_as(app, "/api/v1/room/create", &token, Some(&device_id), "{}")
        .await
        .unwrap();
    room["token"].as_str().unwrap().to_owned()
}

struct Host {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    decoder: Decoder,
}

impl Host {
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

    /// The next packet, or `None` once the server closed the connection.
    async fn recv(&mut self) -> Option<Packet> {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.socket.next())
                .await
                .expect("server answered in time");
            let packet = match frame {
                Some(Ok(Message::Text(text))) => self.decoder.text(&text).unwrap(),
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
    let mut host = Host::connect(addr, &token).await.unwrap();

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
async fn needs_a_valid_room_token() {
    let (app, addr) = start(common::config()).await;
    let user_token = common::log_in(&app).await;

    for token in ["bogus", &user_token] {
        let Err(Error::Http(response)) = Host::connect(addr, token).await else {
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
    let Err(Error::Http(response)) = Host::connect(addr, "bogus").await else {
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
    let mut host = Host::connect(addr, &token).await.unwrap();
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
    let mut older = Host::connect(addr, &token).await.unwrap();
    older.handshake().await;

    let mut newer = Host::connect(addr, &token).await.unwrap();
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
