mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use remora_cloud::{Config, OwnerPassword};
use remora_proto::control::ApiError;
use remora_proto::signal::{Decoder, Event, Packet};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

const REPORT: &str = "/api/v1/device/wake_on_lan/info";
const PASSWORD: &str = "correct horse battery staple";

/// Logins confirmed automatically, as the owner; the owner signs in to the web app with
/// [`PASSWORD`]. Served on a free port; the returned router shares the served state.
async fn start() -> (Router, SocketAddr) {
    let app = remora_cloud::router(Config {
        owner_password: Some(OwnerPassword::hash(PASSWORD)),
        ..common::config()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    (app, addr)
}

/// What uu-remote-linux reports, with synthetic values: `ip` and `inner_ip` are the same.
fn lan(mac: &str, ip: &str, subnet_mask: &str) -> Value {
    json!({ "mac": mac, "inner_ip": ip, "ip": ip, "subnet_mask": subnet_mask })
}

/// A host on the owner's account: registered, logged in, and bound by creating a room.
struct Host {
    device_id: String,
    token: String,
    room: String,
}

impl Host {
    async fn bind(app: &Router, client_id: &str) -> Self {
        let device_id = common::register(app, client_id).await;
        let token = common::log_in(app).await;
        let room = common::create_room(app, &token, &device_id).await;
        Self {
            device_id,
            token,
            room,
        }
    }

    async fn report(&self, app: &Router, body: &Value) -> Result<Value, ApiError> {
        let body = body.to_string();
        common::post_as(app, REPORT, &self.token, Some(&self.device_id), &body).await
    }

    /// Connects to signaling, which makes the device online.
    async fn connect(&self, addr: SocketAddr) -> Client {
        Client::connect(addr, &self.room).await
    }
}

struct Client {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    decoder: Decoder,
}

impl Client {
    async fn connect(addr: SocketAddr, token: &str) -> Self {
        let url = format!("ws://{addr}/socket.io/?transport=websocket&EIO=4");
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("X-NRD-AUTH", token.parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let mut client = Self {
            socket,
            decoder: Decoder::new(),
        };
        assert!(matches!(client.recv().await, Some(Packet::Open(_))));
        client.socket.send(Message::text("40")).await.unwrap();
        assert_eq!(client.recv().await, Some(Packet::Connected));
        client
    }

    async fn recv_within(&mut self, wait: Duration) -> Option<Packet> {
        loop {
            let frame = tokio::time::timeout(wait, self.socket.next()).await.ok()?;
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

    async fn recv(&mut self) -> Option<Packet> {
        self.recv_within(Duration::from_secs(5)).await
    }

    async fn event(&mut self) -> Event {
        match self.recv().await {
            Some(Packet::Event(event)) => event,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    /// Asserts that nothing arrives for a moment.
    async fn quiet(&mut self) {
        let packet = self.recv_within(Duration::from_millis(200)).await;
        assert_eq!(packet, None);
    }
}

/// A browser signed in as the owner.
struct Browser {
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn sign_in(app: &Router) -> Self {
        let request = Request::post("/api/web/session")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "password": PASSWORD }).to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        let cookie = cookie.split(';').next().unwrap().to_owned();
        let mut browser = Self {
            cookie,
            csrf: String::new(),
        };
        let (_, session) = browser.call(app, Method::GET, "/api/web/session").await;
        browser.csrf = session["signed_in"]["csrf"].as_str().unwrap().to_owned();
        browser
    }

    async fn call(&self, app: &Router, method: Method, path: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn wake(&self, app: &Router, device_id: &str) -> (StatusCode, Value) {
        let path = format!("/api/web/devices/{device_id}/wake");
        self.call(app, Method::POST, &path).await
    }

    /// `wakeable` of each of the owner's devices, by id.
    async fn wakeable(&self, app: &Router) -> Vec<(String, bool)> {
        let (_, devices) = self.call(app, Method::GET, "/api/web/devices").await;
        let devices = devices.as_array().unwrap();
        devices
            .iter()
            .map(|device| {
                let id = device["id"].as_str().unwrap().to_owned();
                (id, device["wakeable"].as_bool().unwrap())
            })
            .collect()
    }
}

#[tokio::test]
async fn a_host_reports_its_lan() {
    let (app, _) = start().await;
    let host = Host::bind(&app, "client-host").await;
    let reported = host
        .report(
            &app,
            &lan("AA:BB:CC:DD:EE:01", "192.168.1.20", "255.255.255.0"),
        )
        .await
        .unwrap();
    assert_eq!(reported, json!({ "turn_on_wol": true }));

    let browser = Browser::sign_in(&app).await;
    assert_eq!(browser.wakeable(&app).await, [(host.device_id, true)]);
}

#[tokio::test]
async fn invalid_reports_are_refused() {
    let (app, _) = start().await;
    let host = Host::bind(&app, "client-host").await;
    let invalid = Err(ApiError::Rejected {
        http_status: 422,
        code: 1001,
    });
    let without_inner_ip =
        json!({ "mac": "aa:bb:cc:dd:ee:01", "ip": "192.168.1.20", "subnet_mask": "255.255.255.0" });
    for body in [
        without_inner_ip,
        lan("aa:bb:cc:dd:ee", "192.168.1.20", "255.255.255.0"),
        lan("aa:bb:cc:dd:ee:01", "fe80::1", "255.255.255.0"),
        lan("aa:bb:cc:dd:ee:01", "192.168.1.20", "255.0.255.0"),
    ] {
        assert_eq!(host.report(&app, &body).await, invalid, "{body}");
    }

    let browser = Browser::sign_in(&app).await;
    assert_eq!(browser.wakeable(&app).await, [(host.device_id, false)]);
}

#[tokio::test]
async fn only_bound_devices_report() {
    let (app, _) = start().await;
    let unbound = common::register(&app, "client-unbound").await;
    let token = common::log_in(&app).await;
    let body = lan("aa:bb:cc:dd:ee:01", "192.168.1.20", "255.255.255.0").to_string();
    let result: Result<Value, _> =
        common::post_as(&app, REPORT, &token, Some(&unbound), &body).await;
    assert_eq!(
        result,
        Err(ApiError::Rejected {
            http_status: 400,
            code: 2003
        })
    );
    let result: Result<Value, _> = common::post_as(&app, REPORT, &token, None, &body).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn a_host_on_the_same_lan_wakes_the_device() {
    let (app, addr) = start().await;
    let target = Host::bind(&app, "client-target").await;
    let other_lan = Host::bind(&app, "client-other-lan").await;
    let offline = Host::bind(&app, "client-offline").await;
    let helper = Host::bind(&app, "client-helper").await;
    let mask = "255.255.255.0";
    // Reported while online; the report outlives the connection.
    let target_socket = target.connect(addr).await;
    target
        .report(&app, &lan("AA-BB-CC-DD-EE-01", "192.168.1.20", mask))
        .await
        .unwrap();
    drop(target_socket);
    other_lan
        .report(&app, &lan("aa:bb:cc:dd:ee:02", "192.168.2.20", mask))
        .await
        .unwrap();
    offline
        .report(&app, &lan("aa:bb:cc:dd:ee:03", "192.168.1.30", mask))
        .await
        .unwrap();
    let mut other_lan_socket = other_lan.connect(addr).await;

    let browser = Browser::sign_in(&app).await;
    // Wait until the target's connection is gone.
    for _ in 0..50 {
        if browser.wake(&app, &target.device_id).await.1 != json!({ "error": "device_online" }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        browser.wake(&app, &target.device_id).await,
        (StatusCode::CONFLICT, json!({ "error": "no_wake_helper" }))
    );
    other_lan_socket.quiet().await;

    helper
        .report(&app, &lan("aa:bb:cc:dd:ee:04", "192.168.1.40", mask))
        .await
        .unwrap();
    let mut helper_socket = helper.connect(addr).await;
    let (status, _) = browser.wake(&app, &target.device_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let push = helper_socket.event().await;
    assert_eq!(push.name, "bmsg_push");
    assert_eq!(
        push.args,
        [json!({
            "type": "wake_on_lan",
            "data": {
                "target_device": {
                    "mac": "aa:bb:cc:dd:ee:01",
                    "ip": "192.168.1.20",
                    "subnet_mask": "255.255.255.0",
                },
            },
        })]
    );
    other_lan_socket.quiet().await;
}

#[tokio::test]
async fn waking_needs_an_offline_device_with_a_report() {
    let (app, addr) = start().await;
    let online = Host::bind(&app, "client-online").await;
    let unreported = Host::bind(&app, "client-unreported").await;
    let _socket = online.connect(addr).await;
    online
        .report(
            &app,
            &lan("aa:bb:cc:dd:ee:01", "192.168.1.20", "255.255.255.0"),
        )
        .await
        .unwrap();
    let browser = Browser::sign_in(&app).await;

    let conflict = |error: &str| (StatusCode::CONFLICT, json!({ "error": error }));
    assert_eq!(
        browser.wake(&app, &online.device_id).await,
        conflict("device_online")
    );
    assert_eq!(
        browser.wake(&app, &unreported.device_id).await,
        conflict("no_lan_report")
    );
    let unknown = browser.wake(&app, "unknown").await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);

    let without_csrf = Browser {
        csrf: String::new(),
        ..browser
    };
    let (status, _) = without_csrf.wake(&app, &unreported.device_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
