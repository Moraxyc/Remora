//! Sans-IO device registration and QR login state machine.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use rand_core::RngCore;

use super::{Machine, Timestamp};
use crate::control::{
    ApiError, DeviceInitRequest, DeviceInitResponse, LoginByQrcodeRequest, LoginResponse,
    PARAM_PREFIX, QrLoginGenResponse, QrLoginStatusRequest, QrLoginStatusResponse, SIGN_HEADER,
    SignKey, parse_response, sign,
};

/// QR code validity window (180 seconds).
const QR_LIFETIME: Duration = Duration::from_secs(180);
/// Interval between QR status polls when not long-polling continuously.
const QR_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AuthState {
    #[default]
    Idle,
    DeviceInitPending,
    QrGenerating,
    QrPending,
    QrRedeeming,
    Authenticated,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOp {
    DeviceInit,
    QrGenerate,
    QrStatusPoll,
    QrRedeem,
    Logout,
}

/// Signed HTTP request produced by [`AuthMachine`] for an I/O driver to execute.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedHttpRequest {
    pub method: &'static str,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl fmt::Debug for SignedHttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedHttpRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("header_count", &self.headers.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthTransmit {
    HttpRequest(SignedHttpRequest),
}

#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthEvent {
    DeviceInitialized {
        device_id: String,
    },
    QrReady {
        ticket: String,
        jump_url: String,
    },
    QrStatusChanged {
        status: u8,
    },
    Authenticated {
        token: String,
        user_id: String,
        nickname: String,
    },
    LoggedOut,
    QrExpired,
    NeedsRelogin,
    ApiFailed(ApiError),
}

impl fmt::Debug for AuthEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceInitialized { device_id } => f
                .debug_struct("DeviceInitialized")
                .field("device_id", device_id)
                .finish(),
            Self::QrReady { .. } => f
                .debug_struct("QrReady")
                .field("ticket", &"<redacted>")
                .field("jump_url", &"<redacted>")
                .finish(),
            Self::QrStatusChanged { status } => f
                .debug_struct("QrStatusChanged")
                .field("status", status)
                .finish(),
            Self::Authenticated {
                user_id, nickname, ..
            } => f
                .debug_struct("Authenticated")
                .field("token", &"<redacted>")
                .field("user_id", user_id)
                .field("nickname", nickname)
                .finish(),
            Self::LoggedOut => f.write_str("LoggedOut"),
            Self::QrExpired => f.write_str("QrExpired"),
            Self::NeedsRelogin => f.write_str("NeedsRelogin"),
            Self::ApiFailed(err) => f.debug_tuple("ApiFailed").field(err).finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthError {
    UnexpectedResponse { state: AuthState },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedResponse { state } => {
                write!(f, "unexpected HTTP response in auth state {state:?}")
            }
        }
    }
}

impl std::error::Error for AuthError {}

/// Sans-IO authentication state machine with injected [`SignKey`], Unix epoch anchor, and
/// [`RngCore`].
pub struct AuthMachine<R: RngCore> {
    sign_key: SignKey,
    base_unix_secs: u64,
    plat: String,
    version: String,
    rng: R,
    state: AuthState,
    pending_op: Option<PendingOp>,
    qr_ticket: Option<String>,
    qr_jump_url: Option<String>,
    last_qr_status: u8,
    qr_expires_at: Option<Timestamp>,
    next_poll_at: Option<Timestamp>,
    transmits: VecDeque<AuthTransmit>,
    events: VecDeque<AuthEvent>,
}

impl<R: RngCore> fmt::Debug for AuthMachine<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthMachine")
            .field("state", &self.state)
            .field("pending_op", &self.pending_op)
            .field("has_qr", &self.qr_ticket.is_some())
            .finish()
    }
}

impl<R: RngCore> AuthMachine<R> {
    pub fn new(
        sign_key: SignKey,
        base_unix_secs: u64,
        plat: impl Into<String>,
        version: impl Into<String>,
        rng: R,
    ) -> Self {
        Self {
            sign_key,
            base_unix_secs,
            plat: plat.into(),
            version: version.into(),
            rng,
            state: AuthState::Idle,
            pending_op: None,
            qr_ticket: None,
            qr_jump_url: None,
            last_qr_status: 1,
            qr_expires_at: None,
            next_poll_at: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn state(&self) -> AuthState {
        self.state
    }

    /// Starts device initialization (`POST /api/v1/device/{platform}/init`).
    pub fn start_device_init(
        &mut self,
        now: Timestamp,
        endpoint: &str,
        client_id: &str,
        name: &str,
    ) {
        let body = serde_json::to_vec(&DeviceInitRequest {
            client_id: client_id.to_owned(),
            name: name.to_owned(),
        })
        .expect("init body serializes");
        let req = self.build_signed_request(now, "POST", endpoint, None, None, body);
        self.state = AuthState::DeviceInitPending;
        self.pending_op = Some(PendingOp::DeviceInit);
        self.transmits.push_back(AuthTransmit::HttpRequest(req));
    }

    /// Starts QR login (`POST /api/v1/qrcode/gen/login`).
    pub fn request_qr_login(&mut self, now: Timestamp, device_id: Option<&str>) {
        let req = self.build_signed_request(
            now,
            "POST",
            "/api/v1/qrcode/gen/login",
            device_id,
            None,
            b"{}".to_vec(),
        );
        self.state = AuthState::QrGenerating;
        self.pending_op = Some(PendingOp::QrGenerate);
        self.qr_ticket = None;
        self.qr_jump_url = None;
        self.qr_expires_at = None;
        self.next_poll_at = None;
        self.transmits.push_back(AuthTransmit::HttpRequest(req));
    }

    /// Logs out the current user session (`POST /api/v1/user/logout`).
    pub fn request_logout(&mut self, now: Timestamp, user_token: &str, device_id: Option<&str>) {
        let req = self.build_signed_request(
            now,
            "POST",
            "/api/v1/user/logout",
            device_id,
            Some(user_token),
            b"{}".to_vec(),
        );
        self.pending_op = Some(PendingOp::Logout);
        self.transmits.push_back(AuthTransmit::HttpRequest(req));
    }

    fn enqueue_qr_poll(&mut self, now: Timestamp) {
        let (Some(ticket), Some(jump_url)) = (&self.qr_ticket, &self.qr_jump_url) else {
            return;
        };
        let body = serde_json::to_vec(&QrLoginStatusRequest {
            token: ticket.clone(),
            qrcode_jump_url: jump_url.clone(),
            login_status: self.last_qr_status,
        })
        .expect("qr poll serializes");
        let req =
            self.build_signed_request(now, "POST", "/api/v1/qrcode/login/status", None, None, body);
        self.pending_op = Some(PendingOp::QrStatusPoll);
        self.next_poll_at = None;
        self.transmits.push_back(AuthTransmit::HttpRequest(req));
    }

    fn enqueue_qr_redeem(&mut self, now: Timestamp) {
        let (Some(ticket), Some(jump_url)) = (&self.qr_ticket, &self.qr_jump_url) else {
            return;
        };
        let body = serde_json::to_vec(&LoginByQrcodeRequest {
            token: ticket.clone(),
            qrcode_jump_url: jump_url.clone(),
        })
        .expect("qr redeem serializes");
        let req =
            self.build_signed_request(now, "POST", "/api/v1/login/by_qrcode", None, None, body);
        self.state = AuthState::QrRedeeming;
        self.pending_op = Some(PendingOp::QrRedeem);
        self.next_poll_at = None;
        self.transmits.push_back(AuthTransmit::HttpRequest(req));
    }

    fn build_signed_request(
        &mut self,
        now: Timestamp,
        method: &'static str,
        path: &str,
        device_id: Option<&str>,
        bearer_token: Option<&str>,
        body: Vec<u8>,
    ) -> SignedHttpRequest {
        let ts = self
            .base_unix_secs
            .saturating_add(now.as_secs())
            .to_string();
        let mut nonce_bytes = [0u8; 8];
        self.rng.fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);

        let mut headers = vec![
            (format!("{PARAM_PREFIX}plat"), self.plat.clone()),
            (format!("{PARAM_PREFIX}vn"), self.version.clone()),
            (format!("{PARAM_PREFIX}ts"), ts),
            (format!("{PARAM_PREFIX}nonce"), nonce),
        ];
        if let Some(dev) = device_id {
            headers.push((format!("{PARAM_PREFIX}device-id"), dev.to_owned()));
        }
        if !body.is_empty() {
            headers.push(("content-type".into(), "application/json".into()));
        }
        if let Some(token) = bearer_token {
            headers.push(("authorization".into(), format!("Bearer {token}")));
        }
        let sig = sign(
            &self.sign_key,
            method,
            path,
            headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &body,
        );
        headers.push((SIGN_HEADER.into(), sig));

        SignedHttpRequest {
            method,
            path: path.to_owned(),
            headers,
            body,
        }
    }

    fn clear_qr(&mut self) {
        self.qr_ticket = None;
        self.qr_jump_url = None;
        self.qr_expires_at = None;
        self.next_poll_at = None;
    }
}

/// Splits raw HTTP response bytes into `(status, body)`. If `bytes` is plain JSON without an
/// HTTP status line, treats status as `200` so callers and tests can pass response bodies directly.
fn split_http_response(bytes: &[u8]) -> (u16, &[u8]) {
    if bytes.starts_with(b"HTTP/")
        && let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n")
    {
        let head = std::str::from_utf8(&bytes[..pos]).unwrap_or("");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(500);
        return (status, &bytes[pos + 4..]);
    }
    (200, bytes)
}

impl<R: RngCore> Machine for AuthMachine<R> {
    type Transmit = AuthTransmit;
    type Event = AuthEvent;
    type Error = AuthError;

    fn handle_input(&mut self, now: Timestamp, input: &[u8]) -> Result<(), Self::Error> {
        let Some(op) = self.pending_op.take() else {
            return Err(AuthError::UnexpectedResponse { state: self.state });
        };
        let (status, body) = split_http_response(input);

        match op {
            PendingOp::DeviceInit => match parse_response::<DeviceInitResponse>(status, body) {
                Ok(data) => {
                    self.state = AuthState::Idle;
                    self.events.push_back(AuthEvent::DeviceInitialized {
                        device_id: data.device_id,
                    });
                }
                Err(err) => {
                    self.state = AuthState::Idle;
                    self.events.push_back(AuthEvent::ApiFailed(err));
                }
            },
            PendingOp::QrGenerate => match parse_response::<QrLoginGenResponse>(status, body) {
                Ok(data) => {
                    self.state = AuthState::QrPending;
                    self.last_qr_status = 1;
                    self.qr_ticket = Some(data.status_query_ticket.clone());
                    self.qr_jump_url = Some(data.qrcode_jump_url.clone());
                    self.qr_expires_at = Some(now + QR_LIFETIME);
                    self.next_poll_at = Some(now + QR_POLL_INTERVAL);
                    self.events.push_back(AuthEvent::QrReady {
                        ticket: data.status_query_ticket,
                        jump_url: data.qrcode_jump_url,
                    });
                }
                Err(err) => {
                    self.state = AuthState::Idle;
                    self.events.push_back(AuthEvent::ApiFailed(err));
                }
            },
            PendingOp::QrStatusPoll => {
                match parse_response::<QrLoginStatusResponse>(status, body) {
                    Ok(data) => {
                        if data.status != self.last_qr_status {
                            self.last_qr_status = data.status;
                            self.events.push_back(AuthEvent::QrStatusChanged {
                                status: data.status,
                            });
                        }
                        match data.status {
                            3 => {
                                self.clear_qr();
                                self.state = AuthState::Expired;
                                self.events.push_back(AuthEvent::QrExpired);
                            }
                            4 => {
                                self.enqueue_qr_redeem(now);
                            }
                            _ => {
                                self.next_poll_at = Some(now + QR_POLL_INTERVAL);
                            }
                        }
                    }
                    Err(ApiError::QrCodeExpired) => {
                        self.clear_qr();
                        self.state = AuthState::Expired;
                        self.events.push_back(AuthEvent::QrExpired);
                    }
                    Err(err) => {
                        self.events.push_back(AuthEvent::ApiFailed(err));
                    }
                }
            }
            PendingOp::QrRedeem => match parse_response::<LoginResponse>(status, body) {
                Ok(data) => {
                    self.clear_qr();
                    self.state = AuthState::Authenticated;
                    self.events.push_back(AuthEvent::Authenticated {
                        token: data.token,
                        user_id: data.user_id,
                        nickname: data.nickname,
                    });
                }
                Err(ApiError::QrCodeExpired) => {
                    self.clear_qr();
                    self.state = AuthState::Expired;
                    self.events.push_back(AuthEvent::QrExpired);
                }
                Err(ApiError::TokenExpired) => {
                    self.clear_qr();
                    self.state = AuthState::Expired;
                    self.events.push_back(AuthEvent::NeedsRelogin);
                }
                Err(err) => {
                    self.events.push_back(AuthEvent::ApiFailed(err));
                }
            },
            PendingOp::Logout => match parse_response::<serde_json::Value>(status, body) {
                Ok(_) => {
                    self.clear_qr();
                    self.state = AuthState::Idle;
                    self.events.push_back(AuthEvent::LoggedOut);
                }
                Err(ApiError::TokenExpired) => {
                    self.state = AuthState::Expired;
                    self.events.push_back(AuthEvent::NeedsRelogin);
                }
                Err(err) => {
                    self.events.push_back(AuthEvent::ApiFailed(err));
                }
            },
        }
        Ok(())
    }

    fn handle_timeout(&mut self, now: Timestamp) {
        if self.state != AuthState::QrPending {
            return;
        }
        if self.qr_expires_at.is_some_and(|exp| now >= exp) {
            self.clear_qr();
            self.pending_op = None;
            self.state = AuthState::Expired;
            self.events.push_back(AuthEvent::QrExpired);
            return;
        }
        if self.next_poll_at.is_some_and(|poll| now >= poll) && self.pending_op.is_none() {
            self.enqueue_qr_poll(now);
        }
    }

    fn poll_transmit(&mut self) -> Option<Self::Transmit> {
        self.transmits.pop_front()
    }

    fn poll_event(&mut self) -> Option<Self::Event> {
        self.events.pop_front()
    }

    fn poll_timeout(&self) -> Option<Timestamp> {
        if self.state != AuthState::QrPending {
            return None;
        }
        match (self.next_poll_at, self.qr_expires_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::verify_request;

    struct DeterministicRng(u64);

    impl RngCore for DeterministicRng {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for chunk in dest.chunks_mut(8) {
                let bytes = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
        }
    }

    #[test]
    fn scripted_device_init_and_qr_login_flow() {
        let key = SignKey::new("test-key");
        let mut auth = AuthMachine::new(
            key.clone(),
            1_700_000_000,
            "4",
            "4.48.2",
            DeterministicRng(42),
        );
        let t0 = Timestamp::ZERO;

        // 1. Device init -> signed request -> response -> DeviceInitialized
        auth.start_device_init(t0, "/api/v1/device/macos/init", "cid-1", "Mac");
        assert_eq!(auth.state(), AuthState::DeviceInitPending);
        let Some(AuthTransmit::HttpRequest(req)) = auth.poll_transmit() else {
            panic!("expected device init request");
        };
        assert_eq!(req.path, "/api/v1/device/macos/init");
        verify_request(
            &key,
            req.method,
            &req.path,
            req.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &req.body,
        )
        .expect("request signature must verify");

        auth.handle_input(
            t0 + Duration::from_millis(20),
            br#"{"code":0,"msg":"ok","data":{"device_id":"dev-1"}}"#,
        )
        .unwrap();
        assert_eq!(auth.state(), AuthState::Idle);
        assert_eq!(
            auth.poll_event(),
            Some(AuthEvent::DeviceInitialized {
                device_id: "dev-1".into()
            })
        );

        // 2. Request QR login -> QrReady -> timeout triggers poll -> confirmed -> redeem -> Authenticated
        let t1 = Timestamp::from_secs(1);
        auth.request_qr_login(t1, Some("dev-1"));
        assert!(matches!(
            auth.poll_transmit(),
            Some(AuthTransmit::HttpRequest(_))
        ));

        auth.handle_input(
            t1 + Duration::from_millis(50),
            br#"{"code":0,"msg":"ok","data":{"qrcode_jump_url":"https://remora.test/login/abc","status_query_ticket":"tkt-1"}}"#,
        )
        .unwrap();
        assert_eq!(auth.state(), AuthState::QrPending);
        assert_eq!(
            auth.poll_event(),
            Some(AuthEvent::QrReady {
                ticket: "tkt-1".into(),
                jump_url: "https://remora.test/login/abc".into(),
            })
        );

        let poll_at = auth.poll_timeout().unwrap();
        auth.handle_timeout(poll_at);
        let Some(AuthTransmit::HttpRequest(poll_req)) = auth.poll_transmit() else {
            panic!("expected qr poll request");
        };
        assert_eq!(poll_req.path, "/api/v1/qrcode/login/status");

        auth.handle_input(
            poll_at + Duration::from_millis(100),
            br#"{"code":0,"msg":"ok","data":{"status":4}}"#,
        )
        .unwrap();
        assert_eq!(auth.state(), AuthState::QrRedeeming);
        assert_eq!(
            auth.poll_event(),
            Some(AuthEvent::QrStatusChanged { status: 4 })
        );
        let Some(AuthTransmit::HttpRequest(redeem_req)) = auth.poll_transmit() else {
            panic!("expected redeem request");
        };
        assert_eq!(redeem_req.path, "/api/v1/login/by_qrcode");

        auth.handle_input(
            poll_at + Duration::from_millis(150),
            concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: application/json\r\n\r\n",
                r#"{"code":0,"msg":"ok","data":{"token":"jwt-secret","user_id":"1","nickname":"Owner"}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(auth.state(), AuthState::Authenticated);
        assert_eq!(
            auth.poll_event(),
            Some(AuthEvent::Authenticated {
                token: "jwt-secret".into(),
                user_id: "1".into(),
                nickname: "Owner".into(),
            })
        );
        assert_eq!(auth.poll_timeout(), None);
    }

    #[test]
    fn scripted_qr_expiration_timeout() {
        let key = SignKey::new("k");
        let mut auth = AuthMachine::new(key, 1_700_000_000, "4", "4.48.2", DeterministicRng(7));
        auth.request_qr_login(Timestamp::ZERO, None);
        let _ = auth.poll_transmit();
        auth.handle_input(
            Timestamp::from_millis(10),
            br#"{"code":0,"msg":"ok","data":{"qrcode_jump_url":"https://r/login/x","status_query_ticket":"t"}}"#,
        )
        .unwrap();
        let _ = auth.poll_event();

        // Advance straight past 180s expiry.
        auth.handle_timeout(Timestamp::from_secs(181));
        assert_eq!(auth.state(), AuthState::Expired);
        assert_eq!(auth.poll_event(), Some(AuthEvent::QrExpired));
        assert_eq!(auth.poll_transmit(), None);
        assert_eq!(auth.poll_timeout(), None);
    }
}
