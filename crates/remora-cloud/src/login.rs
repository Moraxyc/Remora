//! QR-code login. The QR code holds the address of a page of the web app: a user opens it,
//! which counts as scanning it, and confirms the login as themselves or denies it there (see
//! [`crate::web`]). With `auto_confirm`, every login is confirmed for the owner as soon as it is
//! generated instead.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::PublicUrl;
use crate::api::{self, ApiError, Body};
use crate::device::{CallerDevice, Devices};
use crate::token::{TokenKey, random};
use crate::user::{OWNER_ID, Sessions, Users};

/// How long a generated QR code can be used to log in.
const QR_LIFETIME: Duration = Duration::from_secs(180);

/// How long a status poll waits for the status to change. Clients give up after 60 s, and
/// reverse proxies often sooner.
const POLL_WAIT: Duration = Duration::from_secs(25);

/// QR logins that have been generated and not yet used, keyed by the id in their page address.
#[derive(Debug, Clone)]
pub struct Logins {
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    public_url: PublicUrl,
    auto_confirm: bool,
}

#[derive(Debug)]
struct Pending {
    /// What the client polls the status with.
    ticket: String,
    jump_url: String,
    created: Instant,
    requester: Requester,
    state: watch::Sender<Progress>,
    /// Who confirmed it, and so who it logs in.
    user_id: Option<String>,
}

impl Pending {
    fn expired(&self) -> bool {
        self.created.elapsed() >= QR_LIFETIME
    }
}

/// Who asked for a login, as the owner is shown before confirming it.
#[derive(Debug, Clone)]
pub struct Requester {
    /// The registered device the request came from, by name and UU platform number.
    pub device: Option<(String, u8)>,
    /// The address the request came from, as this server saw it.
    pub address: Option<IpAddr>,
    pub at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Waiting,
    /// The owner opened the login's page.
    Scanned,
    Confirmed,
}

impl Progress {
    /// `data.status` of the status poll.
    fn status(self) -> u8 {
        match self {
            Self::Waiting => 1,
            Self::Scanned => 2,
            Self::Confirmed => 4,
        }
    }
}

const STATUS_EXPIRED: u8 = 3;

/// A QR login as the client refers to it: the ticket plus the QR content it was issued with.
#[derive(Deserialize)]
pub struct QrRef {
    token: String,
    qrcode_jump_url: String,
}

/// A login as its client finds it.
enum Check {
    Live {
        state: Progress,
        changes: watch::Receiver<Progress>,
        expires_in: Duration,
    },
    Expired,
    Unknown,
}

impl Logins {
    pub fn new(public_url: PublicUrl, auto_confirm: bool) -> Self {
        Self {
            pending: Arc::default(),
            public_url,
            auto_confirm,
        }
    }

    fn start(&self, requester: Requester) -> (String, String) {
        let id = hex::encode(random::<32>());
        let ticket = hex::encode(random::<32>());
        let jump_url = format!("{}/login/{id}", self.public_url);
        let (state, user_id) = if self.auto_confirm {
            (Progress::Confirmed, Some(OWNER_ID.to_owned()))
        } else {
            (Progress::Waiting, None)
        };
        let mut pending = self.pending.lock().expect("login lock poisoned");
        pending.retain(|_, login| !login.expired());
        pending.insert(
            id,
            Pending {
                ticket: ticket.clone(),
                jump_url: jump_url.clone(),
                created: Instant::now(),
                requester,
                state: watch::Sender::new(state),
                user_id,
            },
        );
        (ticket, jump_url)
    }

    /// Looks a login up for its client, forgetting it if it expired.
    fn check(&self, qr: &QrRef) -> Check {
        let mut pending = self.pending.lock().expect("login lock poisoned");
        let Some((id, login)) = find(&pending, qr) else {
            return Check::Unknown;
        };
        if login.expired() {
            let id = id.to_owned();
            pending.remove(&id);
            return Check::Expired;
        }
        let changes = login.state.subscribe();
        let state = *changes.borrow();
        Check::Live {
            state,
            changes,
            expires_in: QR_LIFETIME.saturating_sub(login.created.elapsed()),
        }
    }

    /// Takes a confirmed login out, so it is used only once, and says whom it logs in.
    fn redeem(&self, qr: &QrRef) -> Option<String> {
        let mut pending = self.pending.lock().expect("login lock poisoned");
        let (id, login) = find(&pending, qr)?;
        if login.expired() || *login.state.borrow() != Progress::Confirmed {
            return None;
        }
        let id = id.to_owned();
        pending.remove(&id)?.user_id
    }

    /// Someone opened the page of login `id`: marks it scanned, and says what it is.
    pub fn scan(&self, id: &str) -> Option<(Progress, Requester)> {
        let pending = self.pending.lock().expect("login lock poisoned");
        let login = pending.get(id).filter(|login| !login.expired())?;
        login.state.send_if_modified(|state| {
            let waiting = *state == Progress::Waiting;
            if waiting {
                *state = Progress::Scanned;
            }
            waiting
        });
        Some((*login.state.borrow(), login.requester.clone()))
    }

    /// User `user_id` confirmed login `id`, which then logs in as them. Returns whether the
    /// login was still waiting for a decision.
    pub fn confirm(&self, id: &str, user_id: &str) -> bool {
        let mut pending = self.pending.lock().expect("login lock poisoned");
        let Some(login) = undecided(&mut pending, id) else {
            return false;
        };
        login.user_id = Some(user_id.to_owned());
        login.state.send_replace(Progress::Confirmed);
        true
    }

    /// Someone denied login `id`, which is forgotten. Returns whether the login was still
    /// waiting for a decision.
    pub fn deny(&self, id: &str) -> bool {
        let mut pending = self.pending.lock().expect("login lock poisoned");
        if undecided(&mut pending, id).is_none() {
            return false;
        }
        // Dropping the state wakes the client's poll, which then finds the login gone.
        pending.remove(id);
        true
    }
}

fn undecided<'a>(pending: &'a mut HashMap<String, Pending>, id: &str) -> Option<&'a mut Pending> {
    pending
        .get_mut(id)
        .filter(|login| !login.expired() && *login.state.borrow() != Progress::Confirmed)
}

/// The pending login `qr` names, with its id.
fn find<'a>(pending: &'a HashMap<String, Pending>, qr: &QrRef) -> Option<(&'a str, &'a Pending)> {
    let (_, id) = qr.qrcode_jump_url.rsplit_once('/')?;
    let (id, login) = pending.get_key_value(id)?;
    (login.ticket == qr.token && login.jump_url == qr.qrcode_jump_url)
        .then_some((id.as_str(), login))
}

/// The address a request came from, when the server knows it.
pub struct PeerAddress(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddress {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let address = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip().to_canonical());
        Ok(Self(address))
    }
}

#[derive(Serialize)]
pub struct GenResponse {
    qrcode_jump_url: String,
    status_query_ticket: String,
}

/// `POST /api/v1/qrcode/gen/login`, with an empty body.
pub async fn generate(
    State(logins): State<Logins>,
    State(devices): State<Devices>,
    CallerDevice(device_id): CallerDevice,
    PeerAddress(address): PeerAddress,
) -> Result<api::Ok<GenResponse>, ApiError> {
    let device = match device_id {
        Some(id) => devices
            .get(&id)?
            .map(|device| (device.name, device.platform)),
        None => None,
    };
    let requester = Requester {
        device,
        address,
        at: SystemTime::now(),
    };
    let (status_query_ticket, qrcode_jump_url) = logins.start(requester);
    tracing::info!("QR login generated");
    Ok(api::Ok(GenResponse {
        qrcode_jump_url,
        status_query_ticket,
    }))
}

#[derive(Deserialize)]
pub struct StatusRequest {
    #[serde(flatten)]
    qr: QrRef,
    /// The status the client last saw.
    #[serde(default)]
    login_status: u8,
}

/// `data.status` of the QR status poll.
#[derive(Serialize)]
pub struct StatusResponse {
    status: u8,
}

/// `POST /api/v1/qrcode/login/status`. Clients long-poll this with the status they last saw:
/// it answers once the status differs, or after a while with the same one.
pub async fn status(
    State(logins): State<Logins>,
    Body(request): Body<StatusRequest>,
) -> Result<api::Ok<StatusResponse>, ApiError> {
    let check = match logins.check(&request.qr) {
        Check::Live {
            state,
            mut changes,
            expires_in,
        } if state.status() == request.login_status => {
            // Ends early when the login changes or is forgotten.
            let _ = tokio::time::timeout(POLL_WAIT.min(expires_in), changes.changed()).await;
            logins.check(&request.qr)
        }
        check => check,
    };
    let status = match check {
        Check::Live { state, .. } => state.status(),
        Check::Expired => STATUS_EXPIRED,
        Check::Unknown => return Err(ApiError::QR_EXPIRED),
    };
    Ok(api::Ok(StatusResponse { status }))
}

#[derive(Serialize)]
pub struct LoginResponse {
    token: String,
    user_id: String,
    nickname: String,
}

/// `POST /api/v1/login/by_qrcode`: trades a confirmed QR login for a user token, once.
pub async fn by_qrcode(
    State(logins): State<Logins>,
    State(tokens): State<Arc<TokenKey>>,
    State(sessions): State<Sessions>,
    State(users): State<Users>,
    Body(qr): Body<QrRef>,
) -> Result<api::Ok<LoginResponse>, ApiError> {
    let user_id = logins.redeem(&qr).ok_or(ApiError::QR_EXPIRED)?;
    // A user who is gone by now has nobody to log in as.
    let profile = users.get(&user_id)?.ok_or(ApiError::QR_EXPIRED)?;
    let session = sessions.start(&profile.id)?;
    tracing::info!(user_id = %profile.id, "logged in by QR code");
    Ok(api::Ok(LoginResponse {
        token: tokens.issue_user(&session),
        user_id: profile.id,
        nickname: profile.name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logins(auto_confirm: bool) -> Logins {
        Logins::new("https://remora.test".parse().unwrap(), auto_confirm)
    }

    fn nobody() -> Requester {
        Requester {
            device: None,
            address: None,
            at: SystemTime::now(),
        }
    }

    /// Starts a login; returns its page id and how its client refers to it.
    fn start(logins: &Logins) -> (String, QrRef) {
        let (token, qrcode_jump_url) = logins.start(nobody());
        let id = qrcode_jump_url
            .strip_prefix("https://remora.test/login/")
            .unwrap()
            .to_owned();
        (
            id,
            QrRef {
                token,
                qrcode_jump_url,
            },
        )
    }

    fn age(logins: &Logins, id: &str) {
        let mut pending = logins.pending.lock().unwrap();
        let login = pending.get_mut(id).unwrap();
        login.created = Instant::now().checked_sub(QR_LIFETIME).unwrap();
    }

    fn state(logins: &Logins, qr: &QrRef) -> Option<Progress> {
        match logins.check(qr) {
            Check::Live { state, .. } => Some(state),
            Check::Expired | Check::Unknown => None,
        }
    }

    #[test]
    fn the_owner_scans_and_confirms() {
        let logins = logins(false);
        let (id, qr) = start(&logins);
        assert_eq!(state(&logins, &qr), Some(Progress::Waiting));
        assert_eq!(logins.redeem(&qr), None, "not confirmed yet");

        let (scanned, _) = logins.scan(&id).unwrap();
        assert_eq!(scanned, Progress::Scanned);
        assert_eq!(state(&logins, &qr), Some(Progress::Scanned));

        assert!(logins.confirm(&id, "7"));
        assert!(!logins.deny(&id), "already decided");
        assert!(!logins.confirm(&id, "8"), "already decided");
        assert_eq!(logins.scan(&id).unwrap().0, Progress::Confirmed);
        assert_eq!(logins.redeem(&qr).as_deref(), Some("7"), "as who confirmed");
        assert_eq!(logins.redeem(&qr), None, "used once");
        assert!(logins.scan(&id).is_none());
    }

    #[test]
    fn denied_logins_are_forgotten() {
        let logins = logins(false);
        let (id, qr) = start(&logins);
        let Check::Live { changes, .. } = logins.check(&qr) else {
            panic!("expected a live login");
        };
        assert!(logins.deny(&id));
        assert!(changes.has_changed().is_err(), "the poll wakes up");
        assert!(matches!(logins.check(&qr), Check::Unknown));
        assert_eq!(logins.redeem(&qr), None);
    }

    #[test]
    fn auto_confirmed_logins_log_the_owner_in() {
        let logins = logins(true);
        let (_, qr) = start(&logins);
        assert_eq!(state(&logins, &qr), Some(Progress::Confirmed));
        assert_eq!(logins.redeem(&qr).as_deref(), Some(OWNER_ID));
    }

    #[test]
    fn clients_must_name_both_ticket_and_url() {
        let logins = logins(true);
        let (_, qr) = start(&logins);
        let (_, other) = start(&logins);
        let crossed = QrRef {
            token: other.token,
            qrcode_jump_url: qr.qrcode_jump_url.clone(),
        };
        assert!(matches!(logins.check(&crossed), Check::Unknown));
        assert_eq!(logins.redeem(&crossed), None);
        assert!(logins.redeem(&qr).is_some());
    }

    #[test]
    fn expired_logins_report_expiry_once_and_are_forgotten() {
        let logins = logins(false);
        let (id, qr) = start(&logins);
        age(&logins, &id);

        assert!(logins.scan(&id).is_none());
        assert!(!logins.confirm(&id, OWNER_ID));
        assert!(matches!(logins.check(&qr), Check::Expired));
        assert!(matches!(logins.check(&qr), Check::Unknown));
    }

    #[test]
    fn starting_a_login_prunes_expired_ones() {
        let logins = logins(false);
        let (stale, _) = start(&logins);
        age(&logins, &stale);
        start(&logins);

        let pending = logins.pending.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending.contains_key(&stale));
    }
}
