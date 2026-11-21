mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::{Form, Json, Router, routing};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use remora_cloud::{Config, OidcProvider, OidcRegistration};
use remora_proto::control::{
    ApiError, AssistDeviceRecordPage, AssistRoomConfig, DeviceListResponse, GuestCreateResponse,
    LoginResponse, QrLoginGenResponse, ShareControlMode, ShareControlModeResponse, parse_response,
    share_code_digest,
};
use remora_proto::pb::ConnectOptions;
use remora_proto::signal::{
    Decoder, Event, Packet, encode_binary_event, encode_event, placeholder,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

const CLIENT_ID: &str = "remora-share-test";
const CLIENT_SECRET: &str = "test-client-secret";

#[derive(Clone, Default)]
struct Idp {
    issuer: String,
    codes: Arc<Mutex<HashMap<String, Grant>>>,
}

struct Grant {
    nonce: String,
    challenge: String,
    subject: String,
    name: String,
}

impl Idp {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let idp = Self {
            issuer: format!("http://{addr}"),
            ..Self::default()
        };
        let app = Router::new()
            .route("/.well-known/openid-configuration", routing::get(discovery))
            .route(
                "/jwks",
                routing::get(|| async { Json(json!({ "keys": [] })) }),
            )
            .route("/token", routing::post(token))
            .with_state(idp.clone());
        tokio::spawn(axum::serve(listener, app).into_future());
        idp
    }

    fn approve(&self, location: &str, subject: &str, name: &str) -> String {
        let (_, query) = location.split_once('?').unwrap();
        let params: HashMap<&str, &str> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .collect();
        let code = format!("code-{subject}-{}", self.codes.lock().unwrap().len());
        self.codes.lock().unwrap().insert(
            code.clone(),
            Grant {
                nonce: params["nonce"].to_owned(),
                challenge: params["code_challenge"].to_owned(),
                subject: subject.to_owned(),
                name: name.to_owned(),
            },
        );
        format!("code={code}&state={}", params["state"])
    }
}

async fn discovery(State(idp): State<Idp>) -> Json<Value> {
    Json(json!({
        "issuer": idp.issuer,
        "authorization_endpoint": format!("{}/authorize", idp.issuer),
        "token_endpoint": format!("{}/token", idp.issuer),
        "jwks_uri": format!("{}/jwks", idp.issuer),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["HS256"],
    }))
}

#[derive(Deserialize)]
struct TokenRequest {
    code: String,
    code_verifier: String,
}

async fn token(
    State(idp): State<Idp>,
    headers: HeaderMap,
    Form(request): Form<TokenRequest>,
) -> Result<Json<Value>, StatusCode> {
    let basic = STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    if headers[header::AUTHORIZATION] != format!("Basic {basic}").as_str() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let grant = idp
        .codes
        .lock()
        .unwrap()
        .remove(&request.code)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(request.code_verifier.as_bytes()));
    if challenge != grant.challenge {
        return Err(StatusCode::BAD_REQUEST);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({
        "iss": idp.issuer,
        "sub": grant.subject,
        "aud": CLIENT_ID,
        "iat": now,
        "exp": now + 300,
        "nonce": grant.nonce,
        "name": grant.name,
    });
    let header_b64 = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload_b64 = URL_SAFE_NO_PAD.encode(claims.to_string());
    let unsigned = format!("{header_b64}.{payload_b64}");
    let mut mac = Hmac::<Sha256>::new_from_slice(CLIENT_SECRET.as_bytes()).unwrap();
    mac.update(unsigned.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(Json(json!({
        "access_token": "access",
        "token_type": "Bearer",
        "id_token": format!("{unsigned}.{sig}"),
    })))
}

async fn start() -> (Router, Idp, SocketAddr) {
    let idp = Idp::start().await;
    let registration = OidcRegistration {
        issuer: idp.issuer.clone(),
        client_id: CLIENT_ID.into(),
        client_secret: Some(CLIENT_SECRET.into()),
        name: "Test IdP".into(),
    };
    let app = remora_cloud::router(Config {
        auto_confirm: false,
        oidc: Some(OidcProvider::new(registration, common::PUBLIC_URL).unwrap()),
        ..common::config()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    (app, idp, addr)
}

async fn log_in_oidc_user(app: &Router, idp: &Idp, subject: &str, name: &str) -> String {
    let start_resp = app
        .clone()
        .oneshot(
            Request::get("/api/web/oidc/start?next=/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let oidc_cookie = start_resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|s| s.split(';').next())
        .find(|s| s.starts_with("remora_oidc="))
        .unwrap()
        .to_owned();
    let location = start_resp.headers()[header::LOCATION].to_str().unwrap();
    let callback = idp.approve(location, subject, name);
    let cb_resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/web/oidc/callback?{callback}"))
                .header(header::COOKIE, oidc_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let session_cookie = cb_resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|s| s.split(';').next())
        .find(|s| s.starts_with("remora_session=") && *s != "remora_session=")
        .unwrap()
        .to_owned();

    let session_resp = app
        .clone()
        .oneshot(
            Request::get("/api/web/session")
                .header(header::COOKIE, &session_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let session_bytes = session_resp.into_body().collect().await.unwrap().to_bytes();
    let session_json: Value = serde_json::from_slice(&session_bytes).unwrap();
    let csrf = session_json["signed_in"]["csrf"].as_str().unwrap();

    let qr: QrLoginGenResponse = common::post(app, "/api/v1/qrcode/gen/login", Some("{}"))
        .await
        .unwrap();
    let qr_id = qr.qrcode_jump_url.rsplit('/').next().unwrap();
    let confirm_resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/web/logins/{qr_id}"))
                .header(header::COOKIE, &session_cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"decision":"confirm"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(confirm_resp.status(), StatusCode::NO_CONTENT);

    let login: LoginResponse = common::post(
        app,
        "/api/v1/login/by_qrcode",
        Some(
            &json!({
                "token": qr.status_query_ticket,
                "qrcode_jump_url": qr.qrcode_jump_url,
            })
            .to_string(),
        ),
    )
    .await
    .unwrap();
    login.token
}

async fn post_with_headers<T: DeserializeOwned>(
    app: &Router,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<T, ApiError> {
    let mut builder = Request::post(path);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(body.unwrap_or_default().to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    parse_response(status, &bytes)
}

async fn request_raw(
    app: &Router,
    method: Method,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> (u16, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(body.unwrap_or_default().to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

struct WsClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    decoder: Decoder,
}

impl WsClient {
    async fn connect(addr: SocketAddr, token: &str) -> Self {
        let url = format!("ws://{addr}/socket.io/?transport=websocket&EIO=4");
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("x-nrd-auth", token.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let _open = socket.next().await.unwrap().unwrap();
        socket.send(Message::Text("40".into())).await.unwrap();
        let _connect = socket.next().await.unwrap().unwrap();
        Self {
            socket,
            decoder: Decoder::new(),
        }
    }

    async fn next_event(&mut self) -> Event {
        loop {
            let msg = self
                .socket
                .next()
                .await
                .expect("ws stream ended")
                .expect("ws error");
            let packet = match msg {
                Message::Text(txt) => {
                    if txt == "2" {
                        self.socket.send(Message::Text("3".into())).await.unwrap();
                        continue;
                    }
                    self.decoder.text(&txt).unwrap()
                }
                Message::Binary(bin) => self.decoder.binary(&bin).unwrap(),
                Message::Close(_) => panic!("unexpected ws close"),
                _ => continue,
            };
            if let Some(Packet::Event(ev)) = packet {
                return ev;
            }
        }
    }

    async fn try_next_event(&mut self) -> Option<Event> {
        loop {
            let msg = self.socket.next().await?.ok()?;
            let packet = match msg {
                Message::Text(txt) => {
                    if txt == "2" {
                        let _ = self.socket.send(Message::Text("3".into())).await;
                        continue;
                    }
                    self.decoder.text(&txt).ok()?
                }
                Message::Binary(bin) => self.decoder.binary(&bin).ok()?,
                Message::Close(_) => return None,
                _ => continue,
            };
            if let Some(Packet::Event(ev)) = packet {
                return Some(ev);
            }
        }
    }

    async fn send_control(&mut self, options: &ConnectOptions) {
        use prost::Message as _;
        let header = json!({
            "app_control_id": "app-1",
            "app_data": placeholder(0),
        });
        let text = encode_binary_event("control", &[header], 1, Some(1));
        let bin = options.encode_to_vec();
        self.socket.send(Message::Text(text.into())).await.unwrap();
        self.socket.send(Message::Binary(bin.into())).await.unwrap();
    }

    async fn send_room_info(&mut self) {
        let text = encode_event("room_info", &[json!({})], Some(1));
        self.socket.send(Message::Text(text.into())).await.unwrap();
        let ready = self.next_event().await;
        assert_eq!(ready.name, "bmsg_push");
    }
}

#[tokio::test]
async fn guest_ordering_and_assist_code_flow_work_end_to_end() {
    let (app, idp, addr) = start().await;

    // 1. guest/create requires a registered device (422 / 1001).
    let err = post_with_headers::<GuestCreateResponse>(
        &app,
        "/api/v1/guest/create",
        &[("x-param-device-id", "missing")],
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        ApiError::Rejected {
            http_status: 422,
            code: 1001
        }
    );

    // 2. Register guest host device, create guest identity, enforce guest/room/create order.
    let host_device = common::register_named(&app, "guest-client-1", "GuestHost").await;
    let guest: GuestCreateResponse = post_with_headers(
        &app,
        "/api/v1/guest/create",
        &[("x-param-device-id", &host_device)],
        None,
    )
    .await
    .unwrap();
    let guest_auth = format!("Bearer {}", guest.token);
    let guest_headers = [
        ("authorization", guest_auth.as_str()),
        ("x-param-guest-id", guest.guest_id.as_str()),
    ];

    // Before guest/room/create -> 400 / 2004.
    let err = post_with_headers::<Value>(&app, "/api/v1/guest/share/info", &guest_headers, None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ApiError::Rejected {
            http_status: 400,
            code: 2004
        }
    );

    // Create guest room, connect signaling, then publish share info.
    let room: Value = post_with_headers(&app, "/api/v1/guest/room/create", &guest_headers, None)
        .await
        .unwrap();
    let room_token = room["token"].as_str().unwrap().to_owned();
    let mut host_ws = WsClient::connect(addr, &room_token).await;
    host_ws.send_room_info().await;

    let info: Value = post_with_headers(&app, "/api/v1/guest/share/info", &guest_headers, None)
        .await
        .unwrap();
    let connect_id = info["connect_id"].as_str().unwrap().to_owned();
    assert_eq!(connect_id.len(), 9);
    assert_eq!(info["alias"], "GuestHost");

    // 3. Viewer (User 2 via OIDC) checks control_mode -> host answers get_control_mode push.
    let viewer_token = log_in_oidc_user(&app, &idp, "viewer-sub", "Viewer").await;
    let viewer_device = common::register_named(&app, "viewer-client-2", "ViewerWin").await;

    let app_clone = app.clone();
    let vt = viewer_token.clone();
    let cid = connect_id.clone();
    let cm_task = tokio::spawn(async move {
        common::post_as::<ShareControlModeResponse>(
            &app_clone,
            "/api/v2/room/share/control_mode",
            &vt,
            None,
            &json!({ "connect_id": cid }).to_string(),
        )
        .await
    });

    let push = host_ws.next_event().await;
    assert_eq!(push.name, "bmsg_push");
    assert_eq!(push.args[0]["type"], "get_control_mode");
    let control_id = push.args[0]["data"]["control_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _: Value = post_with_headers(
        &app,
        "/api/v1/guest/room/share/upload_control_mode",
        &guest_headers,
        Some(
            &json!({
                "control_id": control_id,
                "allow_control": true,
                "control_mode": "by_password",
            })
            .to_string(),
        ),
    )
    .await
    .unwrap();
    let cm_resp = cm_task.await.unwrap().unwrap();
    assert!(cm_resp.allow_control);
    assert_eq!(cm_resp.control_mode, ShareControlMode::ByPassword);
    assert!(cm_resp.need_password);

    // 4. Viewer tries wrong code -> 400 / 1131; right code -> AssistRoomConfig.
    let assist_code = "PASS9988";
    for (attempt_code, expect_ok) in [("WRONG111", false), (assist_code, true)] {
        let app_clone = app.clone();
        let vt = viewer_token.clone();
        let vd = viewer_device.clone();
        let cid = connect_id.clone();
        let code = attempt_code.to_owned();
        let join_task = tokio::spawn(async move {
            common::post_as::<AssistRoomConfig>(
                &app_clone,
                "/api/v2/room/join/share/by_code",
                &vt,
                Some(&vd),
                &json!({ "connect_id": cid, "connect_code": code }).to_string(),
            )
            .await
        });

        let challenge = host_ws.next_event().await;
        assert_eq!(challenge.name, "bmsg_push");
        assert_eq!(challenge.args[0]["type"], "remote_control");
        let cid_chal = challenge.args[0]["data"]["control_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let salt = challenge.args[0]["data"]["salt"]
            .as_str()
            .unwrap()
            .to_owned();
        let _: Value = post_with_headers(
            &app,
            "/api/v1/guest/room/share/upload/sign",
            &guest_headers,
            Some(
                &json!({
                    "control_id": cid_chal,
                    "can_remote_control": true,
                    "sign": share_code_digest(&salt, assist_code),
                    "backup_sign": "",
                })
                .to_string(),
            ),
        )
        .await
        .unwrap();

        let res = join_task.await.unwrap();
        if expect_ok {
            let room_cfg = res.unwrap();
            assert_eq!(room_cfg.share_id, connect_id);
            assert_eq!(room_cfg.publisher_device_id, host_device);
            assert_eq!(room_cfg.publisher_platform, 1);

            // Connect shared viewer over signaling & open desktop session.
            let mut viewer_ws = WsClient::connect(addr, &room_cfg.token).await;
            viewer_ws
                .send_control(&ConnectOptions {
                    client_type: 3,
                    capture_type: 1,
                    ..Default::default()
                })
                .await;
            let bc = host_ws.next_event().await;
            assert_eq!(bc.name, "be-controlled");

            // Cancelling guest share immediately terminates the shared viewer.
            let _: Value =
                post_with_headers(&app, "/api/v1/guest/share/cancel", &guest_headers, None)
                    .await
                    .unwrap();
            let released = host_ws.next_event().await;
            assert_eq!(released.name, "released");
            let left = viewer_ws.try_next_event().await;
            assert_eq!(left.map(|e| e.name), Some("left".to_owned()));
        } else {
            assert_eq!(
                res.unwrap_err(),
                ApiError::Rejected {
                    http_status: 400,
                    code: 1131
                }
            );
        }
    }
}

#[tokio::test]
async fn account_share_confirmation_refresh_and_saved_devices_work() {
    let (app, idp, addr) = start().await;

    // User 1 binds host device and opens room.
    let host_device = common::register_named(&app, "acct-host-1", "OfficePC").await;
    let owner_token = log_in_oidc_user(&app, &idp, "user1-sub", "UserOne").await;
    let owner_room = common::create_room(&app, &owner_token, &host_device).await;
    let mut host_ws = WsClient::connect(addr, &owner_room).await;
    host_ws.send_room_info().await;

    let info: Value = common::get_as(
        &app,
        "/api/v1/device/share/info",
        &owner_token,
        &host_device,
    )
    .await
    .unwrap();
    let old_id = info["connect_id"].as_str().unwrap().to_owned();

    // Refresh ID replaces connect_id and keeps it unique.
    let refreshed: Value = common::post_as(
        &app,
        "/api/v1/device/share/info/refresh_id",
        &owner_token,
        Some(&host_device),
        "",
    )
    .await
    .unwrap();
    let new_id = refreshed["connect_id"].as_str().unwrap().to_owned();
    assert_ne!(old_id, new_id);

    // Same-account assist is rejected with 400 / 1130.
    let err = common::post_as::<AssistRoomConfig>(
        &app,
        "/api/v1/room/join/share/by_code",
        &owner_token,
        None,
        &json!({ "connect_id": new_id, "connect_code": "1234" }).to_string(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        ApiError::Rejected {
            http_status: 400,
            code: 1130
        }
    );

    // User 2 requests confirmation flow (by_confirmation).
    let user2_token = log_in_oidc_user(&app, &idp, "user2-sub", "UserTwo").await;
    let (status, body) = request_raw(
        &app,
        Method::POST,
        "/api/v2/room/join/share/by_confirmation",
        &user2_token,
        Some(&json!({ "connect_id": new_id }).to_string()),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], 1136);
    let control_id = body["data"]["control_id"].as_str().unwrap().to_owned();

    let push = host_ws.next_event().await;
    assert_eq!(push.name, "bmsg_push");
    assert_eq!(push.args[0]["type"], "remote_control_by_confirmation");
    assert_eq!(push.args[0]["data"]["control_id"], control_id);

    // Host approves confirmation -> user 2 polls again and gets AssistRoomConfig.
    let _: Value = common::post_as(
        &app,
        "/api/v2/room/share/confirmation",
        &owner_token,
        Some(&host_device),
        &json!({ "control_id": control_id, "allow": true }).to_string(),
    )
    .await
    .unwrap();

    let room_cfg: AssistRoomConfig = common::post_as(
        &app,
        "/api/v2/room/join/share/by_confirmation",
        &user2_token,
        None,
        &json!({ "connect_id": new_id, "control_id": control_id }).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(room_cfg.publisher_device_id, host_device);

    // User 2 sees the device in others_shared_devices and saved/recent, can favorite and unbind.
    let list: DeviceListResponse = common::get(&app, "/api/v1/device/list", Some(&user2_token))
        .await
        .unwrap();
    assert_eq!(list.others_shared_devices.len(), 1);
    assert_eq!(list.others_shared_devices[0].device_id, host_device);

    let recent: AssistDeviceRecordPage = common::get(
        &app,
        "/api/v1/device/share/saved/recent",
        Some(&user2_token),
    )
    .await
    .unwrap();
    assert_eq!(recent.saved.len(), 1);
    assert_eq!(recent.saved[0].connect_id, new_id);

    let _: Value = common::post_as(
        &app,
        "/api/v1/device/share/saved/favorites",
        &user2_token,
        None,
        &json!({
            "favorites": [{ "connect_id": new_id, "remark": "Team Mac", "favorited_at": 1700000000 }],
            "skip_invalid": false
        })
        .to_string(),
    )
    .await
    .unwrap();
    let favs: AssistDeviceRecordPage = common::get(
        &app,
        "/api/v1/device/share/saved/favorites",
        Some(&user2_token),
    )
    .await
    .unwrap();
    assert_eq!(favs.saved.len(), 1);
    assert!(favs.saved[0].is_favorite);
    assert_eq!(favs.saved[0].remark, "Team Mac");

    // Cancel remote assist pushes cancellation to host when pending.
    let (status, body) = request_raw(
        &app,
        Method::POST,
        "/api/v2/room/join/share/by_confirmation",
        &user2_token,
        Some(&json!({ "connect_id": new_id }).to_string()),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], 1136);
    let _push_confirm = host_ws.next_event().await;
    let _: Value = common::post_as(
        &app,
        "/api/v2/room/share/cancel_remote_assist",
        &user2_token,
        None,
        &json!({ "connect_id": new_id }).to_string(),
    )
    .await
    .unwrap();
    let cancel_push = host_ws.next_event().await;
    assert_eq!(cancel_push.name, "bmsg_push");
    assert_eq!(
        cancel_push.args[0]["type"],
        "remote_control_by_confirmation_cancel"
    );

    // Owner's own viewer stays connected when owner calls device/share/cancel, while shared
    // viewer is kicked.
    let mut shared_ws = WsClient::connect(addr, &room_cfg.token).await;
    shared_ws
        .send_control(&ConnectOptions {
            client_type: 3,
            capture_type: 3,
            ..Default::default()
        })
        .await;
    assert_eq!(host_ws.next_event().await.name, "be-controlled");

    let owner_join: Value = common::post_as(
        &app,
        &format!("/api/v1/room/join/by_device/{host_device}"),
        &owner_token,
        None,
        "{}",
    )
    .await
    .unwrap();
    let mut owner_ws = WsClient::connect(addr, owner_join["token"].as_str().unwrap()).await;
    owner_ws
        .send_control(&ConnectOptions {
            client_type: 3,
            capture_type: 1,
            ..Default::default()
        })
        .await;
    assert_eq!(host_ws.next_event().await.name, "be-controlled");

    let _: Value = common::post_as(
        &app,
        "/api/v1/device/share/cancel",
        &owner_token,
        Some(&host_device),
        "",
    )
    .await
    .unwrap();
    let released = host_ws.next_event().await;
    assert_eq!(released.name, "released");
    assert_eq!(
        shared_ws.try_next_event().await.map(|e| e.name),
        Some("left".to_owned())
    );

    // Owner's viewer session still works (e.g. can send soac).
    owner_ws
        .socket
        .send(Message::Text(
            encode_event("soac", &[json!({ "data": { "type": "candidate" } })], None).into(),
        ))
        .await
        .unwrap();

    // Delete favorite & unbind shared device.
    let (del_status, _) = request_raw(
        &app,
        Method::DELETE,
        &format!("/api/v1/device/share/saved/favorites/{host_device}"),
        &user2_token,
        None,
    )
    .await;
    assert_eq!(del_status, 200);
    let _: Value = common::post_as(
        &app,
        &format!("/api/v1/device/share/{host_device}/unbind"),
        &user2_token,
        None,
        "",
    )
    .await
    .unwrap();
    let list_after: DeviceListResponse =
        common::get(&app, "/api/v1/device/list", Some(&user2_token))
            .await
            .unwrap();
    assert!(list_after.others_shared_devices.is_empty());
}
