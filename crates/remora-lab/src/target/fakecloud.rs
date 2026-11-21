//! Scriptable fake UU cloud server for testing endpoint implementations in isolation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::SinkExt;
use remora_proto::control::SignKey;
use remora_proto::error::ErrorCode;
use remora_proto::server::{EmptyData, error_envelope, ok_envelope, verify_request};
use remora_proto::signal::{
    Decoder, Handshake, Packet, SignalEvent, encode_ack, encode_binary_event, encode_connected,
    encode_event, placeholder,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::LabError;
use crate::scenario::model::FakeCloudSpec;

/// Summary of an HTTP request received by [`FakeCloudInstance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedHttpCall {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Clone)]
struct DeviceRecord {
    device_id: String,
    client_id: String,
    name: String,
    platform: u8,
    controllable: bool,
    version_name: String,
    owner: Option<String>,
}

#[derive(Clone)]
struct RoomTicket {
    device_id: String,
    role: FakeRole,
    force_join: bool,
}

#[derive(Clone, PartialEq, Eq)]
enum FakeRole {
    Host,
    Viewer,
}

#[derive(Default)]
struct CloudState {
    next_id: u64,
    sign_key: Option<SignKey>,
    script: Option<FakeCloudSpec>,
    devices_by_client: HashMap<String, DeviceRecord>,
    devices_by_id: HashMap<String, DeviceRecord>,
    qr_tickets: HashMap<String, String>,
    user_tokens: HashSet<String>,
    device_user_tokens: HashMap<String, Vec<String>>,
    room_tickets: HashMap<String, RoomTicket>,
    hosts_ws: HashMap<String, mpsc::Sender<Vec<WsOut>>>,
    viewers_ws: HashMap<String, mpsc::Sender<Vec<WsOut>>>,
    sessions: HashMap<String, (String, String)>, // ice_id -> (device_id, viewer_conn_id)
    http_calls: Vec<RecordedHttpCall>,
}

#[derive(Clone)]
enum WsOut {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

#[derive(Clone)]
struct AppState {
    base_url: String,
    ws_url: String,
    inner: Arc<Mutex<CloudState>>,
}

/// In-process fake UU cloud server driven by defaults and optional YAML overrides.
pub struct FakeCloudInstance {
    base_url: String,
    sign_key: Option<SignKey>,
    inner: Arc<Mutex<CloudState>>,
    server_task: JoinHandle<()>,
}

impl Drop for FakeCloudInstance {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

impl FakeCloudInstance {
    pub async fn spawn(
        sign_key: Option<&str>,
        script: Option<FakeCloudSpec>,
    ) -> Result<Self, LabError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let base_url = format!("http://{addr}");
        let ws_url = format!("ws://{addr}");
        let parsed_key = sign_key.map(SignKey::new);
        let inner = Arc::new(Mutex::new(CloudState {
            next_id: 1,
            sign_key: parsed_key.clone(),
            script,
            ..Default::default()
        }));

        let state = AppState {
            base_url: base_url.clone(),
            ws_url,
            inner: Arc::clone(&inner),
        };

        let app = Router::new()
            .route("/api/web/session", get(web_session))
            .route("/api/v1/device/windows/init", post(device_init_win))
            .route("/api/v1/device/macos/init", post(device_init_mac))
            .route("/api/v1/device/mac_controllable", post(mac_controllable))
            .route("/api/v1/qrcode/gen/login", post(qr_gen))
            .route("/api/v1/qrcode/login/status", post(qr_status))
            .route("/api/v1/login/by_qrcode", post(login_by_qrcode))
            .route("/api/v1/user/info", get(user_info))
            .route("/api/v1/user/logout", post(user_logout))
            .route("/api/v1/device/list", get(device_list))
            .route("/api/v1/device/groups/of/my", get(device_groups))
            .route(
                "/api/v1/device/{device_id}/preferences",
                get(device_preferences),
            )
            .route("/api/v1/device/share/info", get(device_share_info))
            .route("/api/v1/device/{device_id}/unbind", post(device_unbind))
            .route("/api/v1/member/entitlements", get(member_entitlements))
            .route("/api/v1/release/fetch/update", get(release_fetch_update))
            .route(
                "/api/v1/release/fetch/update/{device_id}",
                get(release_fetch_update_device),
            )
            .route("/api/v1/room/create", post(room_create))
            .route("/api/v1/room/join/by_device/{device_id}", post(room_join))
            .route("/socket.io/", get(ws_connect))
            .fallback(fallback_http)
            .with_state(state);

        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Ok(Self {
            base_url,
            sign_key: parsed_key,
            inner,
            server_task,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn sign_key(&self) -> Option<&SignKey> {
        self.sign_key.as_ref()
    }

    pub fn recorded_http_calls(&self) -> Vec<RecordedHttpCall> {
        self.inner
            .lock()
            .expect("fakecloud lock poisoned")
            .http_calls
            .clone()
    }

    pub async fn shutdown(self) {
        self.server_task.abort();
    }
}

fn json_envelope(status: StatusCode, body: Value) -> Response {
    (status, axum::Json(body)).into_response()
}

fn check_http_preconditions(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<Response> {
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    guard.http_calls.push(RecordedHttpCall {
        method: method.as_str().to_owned(),
        path: path_and_query.to_owned(),
        body: body.to_vec(),
    });

    if let Some(script) = &guard.script {
        let path_only = path_and_query.split('?').next().unwrap_or(path_and_query);
        for rule in &script.http_overrides {
            if rule.method.eq_ignore_ascii_case(method.as_str())
                && (rule.path == path_and_query || rule.path == path_only)
            {
                let status =
                    StatusCode::from_u16(rule.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let payload = if rule.code == 0 {
                    json!({
                        "code": 0,
                        "msg": rule.msg.as_deref().unwrap_or("ok"),
                        "data": rule.data.clone().unwrap_or_else(|| json!({}))
                    })
                } else {
                    json!({
                        "code": rule.code,
                        "msg": rule.msg.as_deref().unwrap_or("scripted error"),
                        "data": rule.data.clone()
                    })
                };
                return Some(json_envelope(status, payload));
            }
        }
    }

    if let Some(key) = &guard.sign_key {
        let header_pairs = headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str(), s)));
        if verify_request(key, method.as_str(), path_and_query, header_pairs, body).is_err() {
            return Some(json_envelope(
                StatusCode::FORBIDDEN,
                serde_json::to_value(error_envelope(
                    ErrorCode::InvalidSignature,
                    "invalid signature",
                ))
                .expect("envelope serializes"),
            ));
        }
    }
    None
}

async fn web_session() -> impl IntoResponse {
    json_envelope(StatusCode::OK, json!({ "signed_in": null }))
}

async fn fallback_http(
    State(state): State<AppState>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &method, pq, &headers, &body) {
        return resp;
    }
    json_envelope(
        StatusCode::NOT_FOUND,
        serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "not found"))
            .expect("envelope serializes"),
    )
}

async fn device_init_win(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    device_init_impl(state, headers, uri, body, 1).await
}

async fn device_init_mac(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    device_init_impl(state, headers, uri, body, 4).await
}

async fn device_init_impl(
    state: AppState,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
    platform: u8,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let Some(client_id) = val.get("client_id").and_then(Value::as_str) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(
                ErrorCode::InvalidRequest,
                "missing client_id",
            ))
            .unwrap(),
        );
    };
    if client_id.is_empty() {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "empty client_id"))
                .unwrap(),
        );
    }
    let name = val
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let controllable = val.get("controllable").and_then(Value::as_bool);
    let version_name = headers
        .get("x-param-vn")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let rec = if let Some(existing) = guard.devices_by_client.get_mut(client_id) {
        existing.name = name.clone();
        if let Some(c) = controllable {
            existing.controllable = c;
        }
        if !version_name.is_empty() {
            existing.version_name = version_name.clone();
        }
        let cloned = existing.clone();
        if let Some(by_id) = guard.devices_by_id.get_mut(&cloned.device_id) {
            *by_id = cloned.clone();
        }
        cloned
    } else {
        let seq = guard.next_id;
        guard.next_id += 1;
        let id = format!("fake-dev-{seq}");
        let rec = DeviceRecord {
            device_id: id.clone(),
            client_id: client_id.to_owned(),
            name,
            platform,
            controllable: controllable.unwrap_or(true),
            version_name,
            owner: None,
        };
        guard
            .devices_by_client
            .insert(client_id.to_owned(), rec.clone());
        guard.devices_by_id.insert(id, rec.clone());
        rec
    };
    let mut obj = json!({
        "device_id": rec.device_id,
        "platform": rec.platform,
        "status": "DISCONNECTED",
        "participants_info": [],
        "version_name": rec.version_name,
        "support_wol": false,
        "controllable": rec.controllable,
        "publisher_availability_status": "offline",
        "controlled_support": matches!(rec.platform, 1 | 4 | 5),
        "update_started_at": 0,
    });
    if !rec.name.is_empty() {
        obj["alias"] = json!(rec.name);
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(obj)).unwrap(),
    )
}

async fn qr_gen(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-ticket-{seq}");
    let jump_url = format!("{}/login/fake-{seq}", state.base_url);
    guard.qr_tickets.insert(ticket.clone(), jump_url.clone());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "qrcode_jump_url": jump_url,
            "status_query_ticket": ticket,
        })))
        .unwrap(),
    )
}

async fn qr_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let ticket = val.get("token").and_then(Value::as_str).unwrap_or("");
    let url = val
        .get("qrcode_jump_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if guard.qr_tickets.get(ticket).map(String::as_str) != Some(url) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::QrCodeExpired, "QR code expired"))
                .unwrap(),
        );
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({ "status": 4 }))).unwrap(),
    )
}

async fn login_by_qrcode(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let ticket = val.get("token").and_then(Value::as_str).unwrap_or("");
    let url = val
        .get("qrcode_jump_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if guard.qr_tickets.get(ticket).map(String::as_str) != Some(url) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::QrCodeExpired, "QR code expired"))
                .unwrap(),
        );
    }
    guard.qr_tickets.remove(ticket);
    let seq = guard.next_id;
    guard.next_id += 1;
    let token = format!("fake-user-jwt-{seq}");
    guard.user_tokens.insert(token.clone());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "token": token,
            "user_id": "1",
            "nickname": "Owner"
        })))
        .unwrap(),
    )
}

fn bearer_valid(guard: &CloudState, headers: &HeaderMap) -> Option<String> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))?;
    guard.user_tokens.contains(token).then(|| token.to_owned())
}

fn token_invalid_resp() -> Response {
    json_envelope(
        StatusCode::UNPROCESSABLE_ENTITY,
        serde_json::to_value(error_envelope(ErrorCode::TokenExpired, "login expired")).unwrap(),
    )
}

async fn user_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "user_id": "1",
            "nickname": "Owner"
        })))
        .unwrap(),
    )
}

async fn user_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(token) = bearer_valid(&guard, &headers) else {
        return token_invalid_resp();
    };
    guard.user_tokens.remove(&token);
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn device_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let caller_id = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let to_entry = |d: &DeviceRecord| {
        let online = guard.hosts_ws.contains_key(&d.device_id);
        let mut obj = json!({
            "device_id": d.device_id,
            "platform": d.platform,
            "status": if online { "CONNECTED" } else { "DISCONNECTED" },
            "participants_info": [],
            "version_name": d.version_name,
            "support_wol": false,
            "controllable": d.controllable,
            "publisher_availability_status": if matches!(d.platform, 1 | 4 | 5) {
                Value::String(if online { "ready" } else { "offline" }.to_owned())
            } else {
                Value::Null
            },
            "controlled_support": matches!(d.platform, 1 | 4 | 5),
            "update_started_at": 0,
        });
        if !d.name.is_empty() {
            obj["alias"] = json!(d.name);
        }
        obj
    };
    let current_device = caller_id
        .and_then(|id| guard.devices_by_id.get(id))
        .map(to_entry);
    let mut owned: Vec<_> = guard
        .devices_by_id
        .values()
        .filter(|d| d.owner.as_deref() == Some("1"))
        .map(to_entry)
        .collect();
    owned.sort_by(|a, b| a["device_id"].as_str().cmp(&b["device_id"].as_str()));
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "current_device": current_device,
            "my_binded_devices": owned,
            "others_shared_devices": []
        })))
        .unwrap(),
    )
}

async fn device_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let caller_id = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .filter(|id| guard.devices_by_id.contains_key(*id));
    let mut desktop = Vec::new();
    let mut mobile = Vec::new();
    let mut tv = Vec::new();
    let mut owned: Vec<_> = guard
        .devices_by_id
        .values()
        .filter(|d| d.owner.as_deref() == Some("1"))
        .collect();
    owned.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    for d in owned {
        let online = guard.hosts_ws.contains_key(&d.device_id);
        let mut obj = json!({
            "device_id": d.device_id,
            "platform": d.platform,
            "status": if online { "CONNECTED" } else { "DISCONNECTED" },
            "participants_info": [],
            "version_name": d.version_name,
            "support_wol": false,
            "controllable": d.controllable,
            "publisher_availability_status": if matches!(d.platform, 1 | 4 | 5) {
                Value::String(if online { "ready" } else { "offline" }.to_owned())
            } else {
                Value::Null
            },
            "controlled_support": matches!(d.platform, 1 | 4 | 5),
            "update_started_at": 0,
        });
        if !d.name.is_empty() {
            obj["alias"] = json!(d.name);
        }
        if matches!(d.platform, 2 | 3) {
            mobile.push(obj);
        } else if matches!(d.platform, 1 | 4 | 5) {
            desktop.push(obj);
        } else {
            tv.push(obj);
        }
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "current_device_id": caller_id,
            "mobile_devices": mobile,
            "desktop_devices": desktop,
            "tv_devices": tv,
        })))
        .unwrap(),
    )
}

async fn mac_controllable(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let Some(controllable) = val.get("controllable").and_then(Value::as_bool) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let Some(dev) = guard.devices_by_id.get_mut(&device_id) else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    dev.controllable = controllable;
    let cid = dev.client_id.clone();
    if let Some(by_client) = guard.devices_by_client.get_mut(&cid) {
        by_client.controllable = controllable;
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn device_preferences(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    if guard
        .devices_by_id
        .get(&device_id)
        .is_none_or(|d| d.owner.as_deref() != Some("1"))
    {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "device_preferences": {
                "device_id": device_id,
                "tools": {
                    "sequence": [
                        "file_transfer",
                        "view_mode",
                        "remote_connection_cli",
                        "port_mapping",
                        "wake_on_lan",
                        "device_reboot",
                        "device_shutdown",
                    ]
                }
            }
        })))
        .unwrap(),
    )
}

async fn member_entitlements(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "version": 0,
            "user_id": "1",
            "features": {}
        })))
        .unwrap(),
    )
}

fn connect_id_for(device_id: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in device_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:09}", 100_000_000 + (hash % 900_000_000))
}

async fn device_share_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let Some(dev) = guard.devices_by_id.get(device_id) else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let alias = if dev.name.is_empty() {
        dev.device_id.clone()
    } else {
        dev.name.clone()
    };
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "connect_id": connect_id_for(&dev.device_id),
            "connect_code": "",
            "alias": alias,
        })))
        .unwrap(),
    )
}

#[derive(Deserialize, Default)]
struct ReleaseUpdateQuery {
    #[serde(default)]
    version_name: Option<String>,
}

fn no_update_json(version_name: &str, version_code: u64) -> Value {
    json!({
        "url": "",
        "md5": "",
        "version_name": version_name,
        "version_code": version_code,
        "change_logs": [],
        "subtitle": "",
        "user_external_force_upgrade": false,
        "upgrade_uri": {},
        "update_type": "NoUpdate",
        "update_prompt_type": "",
    })
}

async fn release_fetch_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let version_name = headers
        .get("x-param-vn")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    let version_code = headers
        .get("x-param-vc")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(no_update_json(version_name, version_code))).unwrap(),
    )
}

async fn release_fetch_update_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Query(query): Query<ReleaseUpdateQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(dev) = guard
        .devices_by_id
        .get(&device_id)
        .filter(|d| d.owner.as_deref() == Some("1"))
    else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let version_name = query
        .version_name
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| dev.version_name.clone());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(no_update_json(&version_name, 0))).unwrap(),
    )
}

async fn device_unbind(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let (kick_tx, viewer_lefts) = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        if bearer_valid(&guard, &headers).is_none() {
            return token_invalid_resp();
        }
        let Some(dev) = guard.devices_by_id.get_mut(&device_id) else {
            return json_envelope(
                StatusCode::BAD_REQUEST,
                serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                    .unwrap(),
            );
        };
        if dev.owner.as_deref() != Some("1") {
            return json_envelope(
                StatusCode::BAD_REQUEST,
                serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                    .unwrap(),
            );
        }
        dev.owner = None;
        let client_id = dev.client_id.clone();
        if let Some(by_client) = guard.devices_by_client.get_mut(&client_id) {
            by_client.owner = None;
        }
        if let Some(bound_tokens) = guard.device_user_tokens.remove(&device_id) {
            for tok in bound_tokens {
                guard.user_tokens.remove(&tok);
            }
        }
        let mut removed_sessions = Vec::new();
        guard.sessions.retain(|ice_id, (dev_id, viewer_id)| {
            if dev_id == &device_id {
                removed_sessions.push((ice_id.clone(), viewer_id.clone()));
                false
            } else {
                true
            }
        });
        let viewer_lefts: Vec<_> = removed_sessions
            .into_iter()
            .filter_map(|(ice_id, viewer_id)| {
                guard
                    .viewers_ws
                    .get(&viewer_id)
                    .cloned()
                    .map(|tx| (ice_id, tx))
            })
            .collect();
        (guard.hosts_ws.remove(&device_id), viewer_lefts)
    };
    if let Some(tx) = kick_tx {
        let left = encode_event("left", &[json!({})], None);
        let _ = tx.send(vec![WsOut::Text(left), WsOut::Close]).await;
    }
    for (ice_id, v_tx) in viewer_lefts {
        let left = encode_event(
            "left",
            &[json!({ "ice_id": ice_id, "client_id": "fake-host" })],
            None,
        );
        let _ = v_tx.send(vec![WsOut::Text(left)]).await;
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn room_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(user_tok) = bearer_valid(&guard, &headers) else {
        return token_invalid_resp();
    };
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(
                ErrorCode::InvalidRequest,
                "missing device_id",
            ))
            .unwrap(),
        );
    };
    let Some(dev) = guard.devices_by_id.get_mut(&device_id) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "unknown device"))
                .unwrap(),
        );
    };
    dev.owner = Some("1".into());
    let client_id = dev.client_id.clone();
    if let Some(by_client) = guard.devices_by_client.get_mut(&client_id) {
        by_client.owner = Some("1".into());
    }
    guard
        .device_user_tokens
        .entry(device_id.clone())
        .or_default()
        .push(user_tok);
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-room-host-{seq}");
    guard.room_tickets.insert(
        ticket.clone(),
        RoomTicket {
            device_id,
            role: FakeRole::Host,
            force_join: false,
        },
    );
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "signaling_server": state.ws_url,
            "signaling_list": [state.ws_url],
            "sni_required_signaling_list": [],
            "token": ticket,
            "ws_connect_timeout_ms": 16_000,
            "max_reconnect_delta": 300,
            "streamer_retry_delta_ms": 1_000,
            "international_connect": false
        })))
        .unwrap(),
    )
}

async fn room_join(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req_val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid body"))
                .unwrap(),
        );
    };
    let force_join = req_val
        .get("force_join")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(dev) = guard.devices_by_id.get(&device_id) else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    if dev.owner.as_deref() != Some("1") {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    }
    let platform = dev.platform;
    if !guard.hosts_ws.contains_key(&device_id) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::DeviceOffline, "device offline"))
                .unwrap(),
        );
    }
    if !force_join && guard.sessions.values().any(|(d, _)| d == &device_id) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(
                ErrorCode::BeingControlled,
                "being controlled",
            ))
            .unwrap(),
        );
    }
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-room-viewer-{seq}");
    guard.room_tickets.insert(
        ticket.clone(),
        RoomTicket {
            device_id,
            role: FakeRole::Viewer,
            force_join,
        },
    );
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "signaling_server": state.ws_url,
            "signaling_list": [state.ws_url],
            "sni_required_signaling_list": [],
            "token": ticket,
            "ws_connect_timeout_ms": 16_000,
            "max_reconnect_delta": 300,
            "streamer_retry_delta_ms": 1_000,
            "international_connect": false,
            "report_token": "",
            "report_url": "",
            "publisher_platform": platform
        })))
        .unwrap(),
    )
}

#[derive(Deserialize)]
struct WsQuery {
    #[serde(rename = "EIO")]
    eio: String,
    transport: String,
}

async fn ws_connect(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if q.eio != "4" || q.transport != "websocket" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let token = headers
        .get("x-nrd-auth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ticket = {
        let guard = state.inner.lock().expect("fakecloud lock poisoned");
        guard.room_tickets.get(token).cloned()
    };
    let Some(ticket) = ticket else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.on_upgrade(move |socket| serve_ws(socket, ticket, state))
}

static WS_CONN_SEQ: AtomicU64 = AtomicU64::new(1);

async fn serve_ws(mut socket: WebSocket, ticket: RoomTicket, state: AppState) {
    let conn_id = format!("fake-conn-{}", WS_CONN_SEQ.fetch_add(1, Ordering::Relaxed));
    let (tx, mut rx) = mpsc::channel::<Vec<WsOut>>(64);
    {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        match ticket.role {
            FakeRole::Host => {
                if let Some(old) = guard.hosts_ws.insert(ticket.device_id.clone(), tx.clone()) {
                    let left = encode_event("left", &[json!({})], None);
                    let _ = old.try_send(vec![WsOut::Text(left), WsOut::Close]);
                }
            }
            FakeRole::Viewer => {
                guard.viewers_ws.insert(conn_id.clone(), tx.clone());
            }
        }
    }

    let open = Handshake {
        sid: conn_id.clone(),
        ping_interval: 25_000,
        ping_timeout: 20_000,
    }
    .encode();
    if socket.send(Message::Text(open.into())).await.is_err() {
        cleanup_ws(&state, &ticket, &conn_id).await;
        return;
    }

    let mut decoder = Decoder::new();
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                let packet = match inbound {
                    Some(Ok(Message::Text(t))) => decoder.text(&t),
                    Some(Ok(Message::Binary(b))) => decoder.binary(&b),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                };
                match packet {
                    Ok(Some(Packet::Connected)) => {
                        let _ = socket.send(Message::Text(encode_connected(&conn_id).into())).await;
                    }
                    Ok(Some(Packet::Event(ev))) => {
                        let ack_id = ev.ack_id;
                        let sig = SignalEvent::from_event(&ev);
                        handle_ws_event(&state, &ticket, &conn_id, ack_id, sig, &tx).await;
                    }
                    _ => {}
                }
            }
            outbound = rx.recv() => {
                let Some(batch) = outbound else { break; };
                for item in batch {
                    match item {
                        WsOut::Text(t) => {
                            if socket.send(Message::Text(t.into())).await.is_err() {
                                break;
                            }
                        }
                        WsOut::Binary(b) => {
                            if socket.send(Message::Binary(b.into())).await.is_err() {
                                break;
                            }
                        }
                        WsOut::Close => {
                            let _ = socket.close().await;
                            cleanup_ws(&state, &ticket, &conn_id).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    cleanup_ws(&state, &ticket, &conn_id).await;
}

async fn cleanup_ws(state: &AppState, ticket: &RoomTicket, conn_id: &str) {
    let mut to_notify = Vec::new();
    {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        match ticket.role {
            FakeRole::Host => {
                guard.hosts_ws.remove(&ticket.device_id);
            }
            FakeRole::Viewer => {
                guard.viewers_ws.remove(conn_id);
                let mut released = Vec::new();
                guard.sessions.retain(|ice_id, (dev, viewer)| {
                    if viewer == conn_id && dev == &ticket.device_id {
                        released.push(ice_id.clone());
                        false
                    } else {
                        true
                    }
                });
                if let Some(host_tx) = guard.hosts_ws.get(&ticket.device_id).cloned() {
                    for ice_id in released {
                        let msg = encode_event(
                            "released",
                            &[json!({ "ice_id": ice_id, "client_id": conn_id })],
                            None,
                        );
                        to_notify.push((host_tx.clone(), vec![WsOut::Text(msg)]));
                    }
                }
            }
        }
    }
    for (tx, batch) in to_notify {
        let _ = tx.send(batch).await;
    }
}

async fn handle_ws_event(
    state: &AppState,
    ticket: &RoomTicket,
    conn_id: &str,
    ack_id: Option<u64>,
    event: SignalEvent,
    self_tx: &mpsc::Sender<Vec<WsOut>>,
) {
    match (&ticket.role, event) {
        (FakeRole::Host, SignalEvent::RoomInfo(_)) => {
            let mut batch = Vec::new();
            if let Some(id) = ack_id {
                batch.push(WsOut::Text(encode_ack(id, &[json!({})])));
            }
            batch.push(WsOut::Text(encode_event(
                "bmsg_push",
                &[json!({ "data": { "status": "online", "publisher_availability_status": "ready" } })],
                None,
            )));
            let _ = self_tx.send(batch).await;
        }
        (_, SignalEvent::RefreshReconnectKey(_)) => {
            if let Some(id) = ack_id {
                let ack = {
                    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
                    let seq = guard.next_id;
                    guard.next_id += 1;
                    encode_ack(
                        id,
                        &[
                            json!("success"),
                            json!({ "reconnect_key": format!("fake-rk-{seq}") }),
                        ],
                    )
                };
                let _ = self_tx.send(vec![WsOut::Text(ack)]).await;
            }
        }
        (FakeRole::Viewer, SignalEvent::Control(ctrl)) => {
            struct Eviction {
                ice_id: String,
                viewer_id: String,
                viewer_tx: Option<mpsc::Sender<Vec<WsOut>>>,
            }
            enum ControlStep {
                Ack(Vec<Value>),
                Forward {
                    host_tx: mpsc::Sender<Vec<WsOut>>,
                    evictions: Vec<Eviction>,
                    batch_host: Vec<WsOut>,
                    ack_args: Vec<Value>,
                },
            }
            let action = {
                let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
                if let Some(host_tx) = guard.hosts_ws.get(&ticket.device_id).cloned() {
                    if !ticket.force_join
                        && guard.sessions.values().any(|(d, _)| d == &ticket.device_id)
                    {
                        ControlStep::Ack(vec![
                            json!("failure"),
                            json!({ "reason": "controlled from another device" }),
                        ])
                    } else {
                        let mut evicted = Vec::new();
                        if ticket.force_join {
                            guard.sessions.retain(|ice_id, (dev_id, viewer_id)| {
                                if dev_id == &ticket.device_id {
                                    evicted.push((ice_id.clone(), viewer_id.clone()));
                                    false
                                } else {
                                    true
                                }
                            });
                        }
                        let evictions: Vec<Eviction> = evicted
                            .into_iter()
                            .map(|(ice_id, viewer_id)| {
                                let viewer_tx = guard.viewers_ws.get(&viewer_id).cloned();
                                Eviction {
                                    ice_id,
                                    viewer_id,
                                    viewer_tx,
                                }
                            })
                            .collect();

                        let seq = guard.next_id;
                        guard.next_id += 1;
                        let ice_id = format!("fake-ice-{seq}");
                        guard.sessions.insert(
                            ice_id.clone(),
                            (ticket.device_id.clone(), conn_id.to_owned()),
                        );
                        let bc = json!({
                            "ice_id": ice_id,
                            "client_id": conn_id,
                            "app_control_id": ctrl.app_control_id,
                            "iceServers": [],
                            "force_relay": false,
                            "app_data": placeholder(0),
                        });
                        let batch_host = vec![
                            WsOut::Text(encode_binary_event("be-controlled", &[bc], 1, None)),
                            WsOut::Binary(ctrl.connect_options),
                        ];
                        let ack_args = vec![
                            json!("success"),
                            json!({
                                "ice_id": ice_id,
                                "client_id": "fake-host",
                                "app_control_id": ctrl.app_control_id,
                                "iceServers": [],
                                "force_relay": false,
                            }),
                        ];
                        ControlStep::Forward {
                            host_tx,
                            evictions,
                            batch_host,
                            ack_args,
                        }
                    }
                } else {
                    ControlStep::Ack(vec![json!("failure"), json!({ "reason": "host offline" })])
                }
            };
            match action {
                ControlStep::Ack(args) => {
                    if let Some(id) = ack_id {
                        let _ = self_tx.send(vec![WsOut::Text(encode_ack(id, &args))]).await;
                    }
                }
                ControlStep::Forward {
                    host_tx,
                    evictions,
                    batch_host,
                    ack_args,
                } => {
                    for ev in evictions {
                        let released = encode_event(
                            "released",
                            &[json!({ "ice_id": ev.ice_id, "client_id": ev.viewer_id })],
                            None,
                        );
                        let _ = host_tx.send(vec![WsOut::Text(released)]).await;
                        if let Some(v_tx) = ev.viewer_tx {
                            let left = encode_event(
                                "left",
                                &[json!({ "ice_id": ev.ice_id, "client_id": "fake-host" })],
                                None,
                            );
                            let _ = v_tx.send(vec![WsOut::Text(left)]).await;
                        }
                    }
                    let _ = host_tx.send(batch_host).await;
                    if let Some(id) = ack_id {
                        let _ = self_tx
                            .send(vec![WsOut::Text(encode_ack(id, &ack_args))])
                            .await;
                    }
                }
            }
        }
        (_, SignalEvent::Soac(mut soac)) => {
            let target = {
                let guard = state.inner.lock().expect("fakecloud lock poisoned");
                match ticket.role {
                    FakeRole::Host => soac
                        .client_id
                        .as_deref()
                        .and_then(|id| guard.viewers_ws.get(id))
                        .cloned(),
                    FakeRole::Viewer => guard.hosts_ws.get(&ticket.device_id).cloned(),
                }
            };
            if let Some(tx) = target {
                soac.client_id = Some(conn_id.to_owned());
                let body = serde_json::to_value(soac).expect("soac serializes");
                let _ = tx
                    .send(vec![WsOut::Text(encode_event("soac", &[body], None))])
                    .await;
            }
        }
        (_, SignalEvent::ForwardSetting(mut fs)) => {
            let target = {
                let guard = state.inner.lock().expect("fakecloud lock poisoned");
                match ticket.role {
                    FakeRole::Host => fs
                        .client_id
                        .as_deref()
                        .and_then(|id| guard.viewers_ws.get(id))
                        .cloned(),
                    FakeRole::Viewer => guard.hosts_ws.get(&ticket.device_id).cloned(),
                }
            };
            if let Some(tx) = target {
                fs.client_id = Some(conn_id.to_owned());
                let body = serde_json::to_value(fs).expect("forward_setting serializes");
                let _ = tx
                    .send(vec![WsOut::Text(encode_event(
                        "forward_setting",
                        &[body],
                        None,
                    ))])
                    .await;
            }
        }
        _ => {}
    }
}
