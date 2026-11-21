//! Signed cloud control-plane HTTP client returning `remora_proto::control` types directly.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use remora_proto::control::signing::{PARAM_PREFIX, SIGN_HEADER, SignKey, sign};
use remora_proto::control::{
    ApiError, DeviceInitRequest, DeviceInitResponse, DeviceListResponse, LoginByQrcodeRequest,
    LoginResponse, QrLoginGenResponse, QrLoginStatusRequest, QrLoginStatusResponse,
    RoomCreateResponse, RoomJoinRequest, RoomJoinResponse, UserInfoResponse, parse_response,
};
use remora_proto::machine::auth::SignedHttpRequest;
use remora_proto::profile::Profile;
use remora_proto::server::EmptyData;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

#[cfg(feature = "record")]
use std::sync::Arc;

#[cfg(feature = "record")]
use crate::record::TraceRecorder;

/// Error returned by [`ControlClient`] calls.
#[derive(Debug)]
#[non_exhaustive]
pub enum HttpError {
    /// Cloud returned a protocol error envelope or malformed JSON.
    Api(ApiError),
    /// Network, TLS, or HTTP transport error (without secret URL details).
    Transport(String),
    /// Endpoint path could not be joined onto the base URL.
    InvalidUrl(url::ParseError),
    /// Request header name or value was invalid.
    InvalidHeader,
    /// Request body serialization failed.
    Serialize(serde_json::Error),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(err) => err.fmt(f),
            Self::Transport(msg) => write!(f, "control transport error: {msg}"),
            Self::InvalidUrl(err) => write!(f, "invalid control URL: {err}"),
            Self::InvalidHeader => f.write_str("invalid HTTP header"),
            Self::Serialize(err) => write!(f, "request serialization error: {err}"),
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api(err) => Some(err),
            Self::InvalidUrl(err) => Some(err),
            Self::Serialize(err) => Some(err),
            Self::Transport(_) | Self::InvalidHeader => None,
        }
    }
}

impl From<ApiError> for HttpError {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<reqwest::Error> for HttpError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.without_url().to_string())
    }
}

/// Signed control-plane HTTP client using `reqwest` with OS trust-store TLS verification.
#[derive(Clone)]
pub struct ControlClient {
    client: Client,
    base_url: Url,
    profile: &'static Profile,
    sign_key: SignKey,
    #[cfg(feature = "record")]
    recorder: Option<Arc<TraceRecorder>>,
}

impl fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlClient")
            .field("base_url", &self.base_url.as_str())
            .field("profile", &self.profile.id)
            .field("sign_key", &self.sign_key)
            .finish()
    }
}

impl ControlClient {
    /// Builds a control client with a default `reqwest::Client` verifying TLS via OS roots.
    pub fn new(
        base_url: Url,
        profile: &'static Profile,
        sign_key: SignKey,
    ) -> Result<Self, HttpError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self::with_http_client(base_url, profile, sign_key, client))
    }

    /// Builds a control client around a caller-supplied `reqwest::Client`.
    pub fn with_http_client(
        base_url: Url,
        profile: &'static Profile,
        sign_key: SignKey,
        client: Client,
    ) -> Self {
        Self {
            client,
            base_url,
            profile,
            sign_key,
            #[cfg(feature = "record")]
            recorder: None,
        }
    }

    /// Attaches a JSONL trace recorder.
    #[cfg(feature = "record")]
    pub fn with_recorder(mut self, recorder: Arc<TraceRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn profile(&self) -> &'static Profile {
        self.profile
    }

    pub fn sign_key(&self) -> &SignKey {
        &self.sign_key
    }

    /// Registers or renames this installation (`POST {profile.init_endpoint}`).
    pub async fn device_init(
        &self,
        req: &DeviceInitRequest,
    ) -> Result<DeviceInitResponse, HttpError> {
        self.post_json(&self.profile.init_endpoint, None, None, Some(req))
            .await
    }

    /// Generates a QR login challenge (`POST /api/v1/qrcode/gen/login`).
    pub async fn qr_login_generate(
        &self,
        device_id: Option<&str>,
    ) -> Result<QrLoginGenResponse, HttpError> {
        self.post_json::<(), _>("/api/v1/qrcode/gen/login", None, device_id, None)
            .await
    }

    /// Polls QR login progress (`POST /api/v1/qrcode/login/status`).
    pub async fn qr_login_status(
        &self,
        req: &QrLoginStatusRequest,
    ) -> Result<QrLoginStatusResponse, HttpError> {
        self.post_json("/api/v1/qrcode/login/status", None, None, Some(req))
            .await
    }

    /// Redeems a confirmed QR login for a user token (`POST /api/v1/login/by_qrcode`).
    pub async fn login_by_qrcode(
        &self,
        req: &LoginByQrcodeRequest,
    ) -> Result<LoginResponse, HttpError> {
        self.post_json("/api/v1/login/by_qrcode", None, None, Some(req))
            .await
    }

    /// Fetches the authenticated user's profile (`GET /api/v1/user/info`).
    pub async fn user_info(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<UserInfoResponse, HttpError> {
        self.get("/api/v1/user/info", Some(user_token), device_id)
            .await
    }

    /// Ends the current user session (`POST /api/v1/user/logout`).
    pub async fn user_logout(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<EmptyData, HttpError> {
        self.post_json::<(), _>("/api/v1/user/logout", Some(user_token), device_id, None)
            .await
    }

    /// Lists owned and current devices (`GET /api/v1/device/list`).
    pub async fn device_list(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<DeviceListResponse, HttpError> {
        self.get("/api/v1/device/list", Some(user_token), device_id)
            .await
    }

    /// Unbinds `target_device_id` from the authenticated user (`POST /api/v1/device/{id}/unbind`).
    pub async fn device_unbind(
        &self,
        user_token: &str,
        target_device_id: &str,
        caller_device_id: Option<&str>,
    ) -> Result<EmptyData, HttpError> {
        let path = format!("/api/v1/device/{target_device_id}/unbind");
        self.post_json::<(), _>(&path, Some(user_token), caller_device_id, None)
            .await
    }

    /// Creates a host signaling room (`POST /api/v1/room/create`).
    pub async fn room_create(
        &self,
        user_token: &str,
        device_id: &str,
    ) -> Result<RoomCreateResponse, HttpError> {
        let empty = serde_json::json!({});
        self.post_json(
            "/api/v1/room/create",
            Some(user_token),
            Some(device_id),
            Some(&empty),
        )
        .await
    }

    /// Requests a viewer room ticket for `target_device_id` (`POST /api/v1/room/join/by_device/{id}`).
    pub async fn room_join_by_device(
        &self,
        user_token: &str,
        target_device_id: &str,
        caller_device_id: Option<&str>,
        req: &RoomJoinRequest,
    ) -> Result<RoomJoinResponse, HttpError> {
        let path = format!("/api/v1/room/join/by_device/{target_device_id}");
        self.post_json(&path, Some(user_token), caller_device_id, Some(req))
            .await
    }

    /// Executes a pre-signed HTTP request emitted by `remora_proto::machine::AuthMachine`.
    pub async fn execute_signed(
        &self,
        req: SignedHttpRequest,
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let url = self.join_url(&req.path)?;
        let method =
            Method::from_bytes(req.method.as_bytes()).map_err(|_| HttpError::InvalidHeader)?;
        let mut headers = HeaderMap::new();
        for (name, value) in &req.headers {
            let h_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| HttpError::InvalidHeader)?;
            let h_val = HeaderValue::from_str(value).map_err(|_| HttpError::InvalidHeader)?;
            headers.insert(h_name, h_val);
        }
        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_request(req.method, &req.path, &req.body);
        }

        let response = self
            .client
            .request(method, url)
            .headers(headers)
            .body(req.body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?.to_vec();

        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_response(req.method, &req.path, status, &bytes);
        }

        Ok((status, bytes))
    }

    async fn get<Resp: DeserializeOwned>(
        &self,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<Resp, HttpError> {
        self.send_signed(Method::GET, path, user_token, device_id, Vec::new())
            .await
    }

    async fn post_json<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
        body: Option<&Req>,
    ) -> Result<Resp, HttpError> {
        let bytes = match body {
            Some(payload) => serde_json::to_vec(payload).map_err(HttpError::Serialize)?,
            None => Vec::new(),
        };
        self.send_signed(Method::POST, path, user_token, device_id, bytes)
            .await
    }

    async fn send_signed<Resp: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Resp, HttpError> {
        let url = self.join_url(path)?;
        let signed_path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let headers =
            self.build_signed_headers(method.as_str(), &signed_path, user_token, device_id, &body)?;

        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_request(method.as_str(), &signed_path, &body);
        }

        let response = self
            .client
            .request(method.clone(), url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?.to_vec();

        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_response(method.as_str(), &signed_path, status, &bytes);
        }

        parse_response(status, &bytes).map_err(HttpError::Api)
    }

    fn join_url(&self, path: &str) -> Result<Url, HttpError> {
        self.base_url.join(path).map_err(HttpError::InvalidUrl)
    }

    fn build_signed_headers(
        &self,
        method: &str,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
        body: &[u8],
    ) -> Result<HeaderMap, HttpError> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let mut nonce_bytes = [0u8; 8];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);
        let build = self.profile.build.to_string();

        let mut param_pairs: Vec<(String, String)> = vec![
            (format!("{PARAM_PREFIX}plat"), self.profile.plat.to_owned()),
            (format!("{PARAM_PREFIX}vn"), self.profile.version.to_owned()),
            (format!("{PARAM_PREFIX}vc"), build),
            (
                format!("{PARAM_PREFIX}chn"),
                self.profile.channel.to_owned(),
            ),
            (
                format!("{PARAM_PREFIX}pkgn"),
                self.profile.bundle_id.to_owned(),
            ),
            (format!("{PARAM_PREFIX}ts"), ts),
            (format!("{PARAM_PREFIX}nonce"), nonce),
        ];
        if let Some(dev) = device_id
            && !dev.is_empty()
        {
            param_pairs.push((format!("{PARAM_PREFIX}device-id"), dev.to_owned()));
        }

        let signature = sign(
            &self.sign_key,
            method,
            path,
            param_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            body,
        );
        param_pairs.push((SIGN_HEADER.to_owned(), signature));

        let mut headers = HeaderMap::new();
        for (name, value) in param_pairs {
            let h_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| HttpError::InvalidHeader)?;
            let h_val = HeaderValue::from_str(&value).map_err(|_| HttpError::InvalidHeader)?;
            headers.insert(h_name, h_val);
        }
        if !body.is_empty() {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if let Some(token) = user_token {
            let auth = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| HttpError::InvalidHeader)?;
            headers.insert(AUTHORIZATION, auth);
        }
        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use remora_proto::control::DeviceStatus;
    use remora_proto::profile;
    use remora_proto::server::verify_request;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    type Handler =
        Arc<dyn Fn(&str, &str, &HashMap<String, String>, &[u8]) -> (u16, String) + Send + Sync>;

    async fn spawn_fake_http(handler: Handler) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let head_end;
                    loop {
                        let n = stream.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            head_end = pos;
                            break;
                        }
                    }
                    let head_str = std::str::from_utf8(&buf[..head_end]).unwrap();
                    let mut lines = head_str.lines();
                    let req_line = lines.next().unwrap();
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap().to_owned();
                    let path = parts.next().unwrap().to_owned();

                    let mut headers = HashMap::new();
                    let mut content_len = 0usize;
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            let k_lower = k.trim().to_ascii_lowercase();
                            let v_trim = v.trim().to_owned();
                            if k_lower == "content-length" {
                                content_len = v_trim.parse().unwrap_or(0);
                            }
                            headers.insert(k_lower, v_trim);
                        }
                    }
                    let body_start = head_end + 4;
                    while buf.len() < body_start + content_len {
                        let n = stream.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = &buf[body_start..body_start + content_len];
                    let (status, resp_body) = handler(&method, &path, &headers, body);
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        Url::parse(&format!("http://{addr}")).unwrap()
    }

    #[tokio::test]
    async fn signs_requests_and_returns_proto_types() {
        let sign_key = SignKey::new("test-sign-key");
        let seen_paths = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen_paths);
        let key_clone = sign_key.clone();

        let url = spawn_fake_http(Arc::new(move |method, path, headers, body| {
            let hdr_iter = headers.iter().map(|(k, v)| (k.as_str(), v.as_str()));
            assert_eq!(
                verify_request(&key_clone, method, path, hdr_iter, body),
                Ok(())
            );
            assert_eq!(headers.get("x-param-plat").map(String::as_str), Some("1"));
            assert_eq!(
                headers.get("x-param-vn").map(String::as_str),
                Some("4.48.2.5754")
            );
            seen_clone.lock().unwrap().push(format!("{method} {path}"));

            match (method, path) {
                ("POST", "/api/v1/device/windows/init") => (
                    200,
                    json!({"code": 0, "msg": "ok", "data": {"device_id": "dev-1"}}).to_string(),
                ),
                ("POST", "/api/v1/qrcode/gen/login") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "qrcode_jump_url": "https://remora.test/login/abc",
                            "status_query_ticket": "ticket-1"
                        }
                    })
                    .to_string(),
                ),
                ("POST", "/api/v1/qrcode/login/status") => (
                    200,
                    json!({"code": 0, "msg": "ok", "data": {"status": 4}}).to_string(),
                ),
                ("POST", "/api/v1/login/by_qrcode") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {"token": "jwt-1", "user_id": "1", "nickname": "Owner"}
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/device/list") => {
                    assert_eq!(
                        headers.get("authorization").map(String::as_str),
                        Some("Bearer jwt-1")
                    );
                    assert_eq!(
                        headers.get("x-param-device-id").map(String::as_str),
                        Some("dev-1")
                    );
                    (
                        200,
                        json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "current_device": null,
                                "my_binded_devices": [{
                                    "device_id": "dev-1",
                                    "alias": "Mac",
                                    "platform": 1,
                                    "status": "CONNECTED"
                                }],
                                "others_shared_devices": []
                            }
                        })
                        .to_string(),
                    )
                }
                ("POST", "/api/v1/room/create") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "signaling_server": "ws://127.0.0.1:9999",
                            "signaling_list": ["ws://127.0.0.1:9999"],
                            "token": "room-tok",
                            "ws_connect_timeout_ms": 10000
                        }
                    })
                    .to_string(),
                ),
                ("POST", "/api/v1/room/join/by_device/dev-1") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "signaling_list": ["ws://127.0.0.1:9999"],
                            "token": "viewer-room-tok",
                            "ws_connect_timeout_ms": 10000,
                            "report_token": "",
                            "report_url": "",
                            "publisher_platform": 1
                        }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/user/info") => (
                    422,
                    json!({"code": 1120, "msg": "login expired", "data": {}}).to_string(),
                ),
                _ => (404, json!({"code": 1001, "msg": "not found"}).to_string()),
            }
        }))
        .await;

        let profile = profile::by_id("windows-4.48.2").unwrap();
        let client = ControlClient::new(url, profile, sign_key).unwrap();

        let init = client
            .device_init(&DeviceInitRequest {
                client_id: "cid-1".into(),
                name: "Host".into(),
            })
            .await
            .unwrap();
        assert_eq!(init.device_id, "dev-1");

        let qr = client.qr_login_generate(Some("dev-1")).await.unwrap();
        let status = client
            .qr_login_status(&QrLoginStatusRequest {
                token: qr.status_query_ticket.clone(),
                qrcode_jump_url: qr.qrcode_jump_url.clone(),
                login_status: 1,
            })
            .await
            .unwrap();
        assert_eq!(status.status, 4);

        let login = client
            .login_by_qrcode(&LoginByQrcodeRequest {
                token: qr.status_query_ticket,
                qrcode_jump_url: qr.qrcode_jump_url,
            })
            .await
            .unwrap();
        assert_eq!(login.user_id, "1");

        let devices = client
            .device_list(&login.token, Some("dev-1"))
            .await
            .unwrap();
        assert_eq!(devices.my_binded_devices.len(), 1);
        assert_eq!(devices.my_binded_devices[0].status, DeviceStatus::Connected);

        let created = client.room_create(&login.token, "dev-1").await.unwrap();
        assert_eq!(created.ws_connect_timeout_ms, 10000);

        let joined = client
            .room_join_by_device(
                &login.token,
                "dev-1",
                None,
                &RoomJoinRequest { force_join: false },
            )
            .await
            .unwrap();
        assert_eq!(joined.publisher_platform, 1);

        let err = client.user_info(&login.token, None).await.unwrap_err();
        assert!(matches!(err, HttpError::Api(ApiError::TokenExpired)));
        assert!(!seen_paths.lock().unwrap().is_empty());
    }
}
