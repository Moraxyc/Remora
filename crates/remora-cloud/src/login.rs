//! QR-code login. There is a single user and no phone app to scan with, so a QR login is
//! confirmed as soon as it is generated.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::api::{self, ApiError, Body};
use crate::token::{TokenKey, random};
use crate::user::{OWNER_ID, OWNER_NICKNAME, Sessions};

/// How long a generated QR code can be used to log in.
const QR_LIFETIME: Duration = Duration::from_secs(180);

/// QR logins that have been generated and not yet used, keyed by their status ticket.
#[derive(Debug, Default, Clone)]
pub struct Logins(Arc<Mutex<HashMap<String, Pending>>>);

#[derive(Debug)]
struct Pending {
    jump_url: String,
    created: Instant,
}

impl Pending {
    fn expired(&self) -> bool {
        self.created.elapsed() >= QR_LIFETIME
    }
}

/// A QR login as the client refers to it: the ticket plus the QR content it was issued with.
#[derive(Deserialize)]
pub struct QrRef {
    token: String,
    qrcode_jump_url: String,
}

enum Lookup {
    Valid,
    Expired,
    Unknown,
}

impl Logins {
    fn start(&self) -> (String, String) {
        let ticket = hex::encode(random::<32>());
        let jump_url = format!("remora-login:{}", hex::encode(random::<32>()));
        let mut pending = self.0.lock().expect("login lock poisoned");
        pending.retain(|_, login| !login.expired());
        pending.insert(
            ticket.clone(),
            Pending {
                jump_url: jump_url.clone(),
                created: Instant::now(),
            },
        );
        (ticket, jump_url)
    }

    /// Looks the login up, forgetting it if it expired or if `consume` is set.
    fn lookup(&self, qr: &QrRef, consume: bool) -> Lookup {
        let mut pending = self.0.lock().expect("login lock poisoned");
        let lookup = match pending.get(&qr.token) {
            Some(login) if login.jump_url != qr.qrcode_jump_url => return Lookup::Unknown,
            Some(login) if login.expired() => Lookup::Expired,
            Some(_) => Lookup::Valid,
            None => return Lookup::Unknown,
        };
        if consume || matches!(lookup, Lookup::Expired) {
            pending.remove(&qr.token);
        }
        lookup
    }
}

#[derive(Serialize)]
pub struct GenResponse {
    qrcode_jump_url: String,
    status_query_ticket: String,
}

/// `POST /api/v1/qrcode/gen/login`, with an empty body.
pub async fn generate(State(logins): State<Logins>) -> api::Ok<GenResponse> {
    let (status_query_ticket, qrcode_jump_url) = logins.start();
    tracing::info!("QR login generated");
    api::Ok(GenResponse {
        qrcode_jump_url,
        status_query_ticket,
    })
}

/// `data.status` of the QR status poll.
#[derive(Serialize)]
pub struct StatusResponse {
    status: u8,
}

const STATUS_EXPIRED: u8 = 3;
const STATUS_CONFIRMED: u8 = 4;

/// `POST /api/v1/qrcode/login/status`. Clients long-poll this; every login is already
/// confirmed, so it answers at once.
pub async fn status(
    State(logins): State<Logins>,
    Body(qr): Body<QrRef>,
) -> Result<api::Ok<StatusResponse>, ApiError> {
    let status = match logins.lookup(&qr, false) {
        Lookup::Valid => STATUS_CONFIRMED,
        Lookup::Expired => STATUS_EXPIRED,
        Lookup::Unknown => return Err(ApiError::QR_EXPIRED),
    };
    Ok(api::Ok(StatusResponse { status }))
}

#[derive(Serialize)]
pub struct LoginResponse {
    token: String,
    user_id: &'static str,
    nickname: &'static str,
}

/// `POST /api/v1/login/by_qrcode`: trades a confirmed QR login for a user token, once.
pub async fn by_qrcode(
    State(logins): State<Logins>,
    State(tokens): State<Arc<TokenKey>>,
    State(sessions): State<Sessions>,
    Body(qr): Body<QrRef>,
) -> Result<api::Ok<LoginResponse>, ApiError> {
    let Lookup::Valid = logins.lookup(&qr, true) else {
        return Err(ApiError::QR_EXPIRED);
    };
    let session = sessions.start(OWNER_ID)?;
    tracing::info!(user_id = OWNER_ID, "logged in by QR code");
    Ok(api::Ok(LoginResponse {
        token: tokens.issue_user(&session),
        user_id: OWNER_ID,
        nickname: OWNER_NICKNAME,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn age(logins: &Logins, ticket: &str) {
        let mut pending = logins.0.lock().unwrap();
        let login = pending.get_mut(ticket).unwrap();
        login.created = Instant::now().checked_sub(QR_LIFETIME).unwrap();
    }

    #[test]
    fn expired_logins_report_expiry_once_and_are_forgotten() {
        let logins = Logins::default();
        let (token, qrcode_jump_url) = logins.start();
        let qr = QrRef {
            token: token.clone(),
            qrcode_jump_url,
        };
        age(&logins, &token);

        assert!(matches!(logins.lookup(&qr, false), Lookup::Expired));
        assert!(matches!(logins.lookup(&qr, true), Lookup::Unknown));
    }

    #[test]
    fn starting_a_login_prunes_expired_ones() {
        let logins = Logins::default();
        let (stale, _) = logins.start();
        age(&logins, &stale);
        logins.start();

        let pending = logins.0.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending.contains_key(&stale));
    }
}
