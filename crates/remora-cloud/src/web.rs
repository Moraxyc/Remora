//! The web app's API: plain JSON under `/api/web/`, for browsers signed in with a session
//! cookie (see [`crate::browser`]). Bodies are JSON, which other sites cannot send without the
//! browser asking this server first.

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::PublicUrl;
use crate::browser::{self, Browsers, SignedIn, read_cookie};
use crate::login::{Logins, Progress};
use crate::oidc::OidcProvider;
use crate::password::OwnerPassword;
use crate::user::{OWNER_ID, Profile, Users};

/// Ties a sign-in at the OpenID Connect provider to the browser that started it. Lax, because
/// the provider sends the browser back from another site.
const OIDC_COOKIE: &str = "remora_oidc";

/// Where the sign-in cookie of an OpenID Connect provider is sent.
const OIDC_COOKIE_PATH: &str = "/api/web/oidc";

/// How long a sign-in at the provider may take.
const OIDC_COOKIE_LIFETIME: u64 = 10 * 60;

/// What the API works with.
#[derive(Clone)]
pub struct Web {
    pub logins: Logins,
    pub browsers: Browsers,
    pub users: Users,
    /// Without a password, nobody can sign in with one.
    pub password: Option<Arc<OwnerPassword>>,
    pub oidc: Option<Arc<OidcProvider>>,
    pub public_url: PublicUrl,
}

/// Why a request failed, as `{"error": "<code>"}` with a matching HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebError {
    BadRequest,
    SignedOut,
    /// The request lacks the browser's CSRF token.
    Csrf,
    NotFound,
    WrongPassword,
    /// This server has no password to sign in with.
    PasswordOff,
    Internal,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::BadRequest => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::SignedOut => (StatusCode::UNAUTHORIZED, "signed_out"),
            Self::Csrf => (StatusCode::FORBIDDEN, "csrf"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::WrongPassword => (StatusCode::UNAUTHORIZED, "wrong_password"),
            Self::PasswordOff => (StatusCode::NOT_FOUND, "password_off"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        (status, Json(json!({ "error": code }))).into_response()
    }
}

impl From<rusqlite::Error> for WebError {
    fn from(error: rusqlite::Error) -> Self {
        tracing::error!(%error, "database query failed");
        Self::Internal
    }
}

impl From<JsonRejection> for WebError {
    fn from(_: JsonRejection) -> Self {
        Self::BadRequest
    }
}

/// JSON request body whose rejection is a [`WebError`].
#[derive(FromRequest)]
#[from_request(via(Json), rejection(WebError))]
pub struct Body<T>(pub T);

/// Answers of this API are about one browser and never cached.
pub async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Serialize)]
pub struct SessionView {
    /// The browser's user and CSRF token, once signed in.
    signed_in: Option<SignedInView>,
    /// Whether signing in with the owner password is possible.
    password: bool,
    /// The name of the OpenID Connect provider to sign in with, if there is one.
    oidc: Option<String>,
    /// Whether this server is reached over https.
    secure: bool,
}

#[derive(Serialize)]
pub struct SignedInView {
    user: Profile,
    csrf: String,
}

/// `GET /api/web/session`: who the browser is signed in as, and how it could sign in.
pub async fn session(
    State(web): State<Web>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, WebError> {
    let signed_in = SignedIn::from_headers(&web.browsers, &headers)?;
    Ok(Json(SessionView {
        signed_in: signed_in.map(|signed_in| SignedInView {
            user: signed_in.user,
            csrf: signed_in.csrf,
        }),
        password: web.password.is_some(),
        oidc: web.oidc.as_ref().map(|oidc| oidc.name.clone()),
        secure: web.public_url.is_secure(),
    }))
}

#[derive(Deserialize)]
pub struct PasswordSignIn {
    password: String,
}

/// `POST /api/web/session`: signs the browser in as the owner with the password. Failed
/// attempts are not limited yet.
pub async fn sign_in_with_password(
    State(web): State<Web>,
    Body(request): Body<PasswordSignIn>,
) -> Result<Response, WebError> {
    let password = web.password.clone().ok_or(WebError::PasswordOff)?;
    let correct = tokio::task::spawn_blocking(move || password.verify(&request.password))
        .await
        .unwrap_or(false);
    if !correct {
        tracing::warn!("password sign-in failed");
        return Err(WebError::WrongPassword);
    }
    let started = web.browsers.start(OWNER_ID)?;
    tracing::info!(user_id = OWNER_ID, "signed in with the password");
    let cookie = web.session_cookie(&started.cookie, browser::LIFETIME.as_secs());
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

/// `DELETE /api/web/session`: signs the browser out.
pub async fn sign_out(signed_in: SignedIn, State(web): State<Web>) -> Result<Response, WebError> {
    web.browsers.end(&signed_in.browser_id)?;
    tracing::info!(user_id = %signed_in.user.id, "signed out");
    let cookie = web.session_cookie("", 0);
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

impl Web {
    fn session_cookie(&self, value: &str, max_age: u64) -> HeaderValue {
        self.cookie(browser::COOKIE, value, "/", max_age, "Strict")
    }

    fn oidc_cookie(&self, value: &str, max_age: u64) -> HeaderValue {
        self.cookie(OIDC_COOKIE, value, OIDC_COOKIE_PATH, max_age, "Lax")
    }

    fn cookie(
        &self,
        name: &str,
        value: &str,
        path: &str,
        max_age: u64,
        same_site: &str,
    ) -> HeaderValue {
        let mut cookie = format!(
            "{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite={same_site}"
        );
        if self.public_url.is_secure() {
            cookie.push_str("; Secure");
        }
        HeaderValue::from_str(&cookie).expect("cookie values are URL-safe")
    }
}

/// Whether `path` is a path of the app on this server, safe to send a browser back to.
fn is_app_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.starts_with("/api/")
        && !path.contains(['\\', '\r', '\n'])
}

/// `next`, telling the app that signing in failed.
fn sign_in_failed(next: &str) -> String {
    let separator = if next.contains('?') { '&' } else { '?' };
    format!("{next}{separator}sign_in=failed")
}

#[derive(Deserialize)]
pub struct OidcStart {
    /// The app's path to return to.
    next: Option<String>,
}

/// `GET /api/web/oidc/start?next=<path>`: sends the browser to the provider to sign in.
pub async fn start_oidc(
    State(web): State<Web>,
    Query(query): Query<OidcStart>,
) -> Result<Response, WebError> {
    let provider = web.oidc.clone().ok_or(WebError::NotFound)?;
    let next = query
        .next
        .filter(|next| is_app_path(next))
        .unwrap_or_else(|| "/".to_owned());
    match provider.start(next.clone()).await {
        Ok((url, state)) => {
            let cookie = web.oidc_cookie(&state, OIDC_COOKIE_LIFETIME);
            Ok(([(header::SET_COOKIE, cookie)], Redirect::to(url.as_str())).into_response())
        }
        Err(failure) => {
            tracing::warn!(reason = failure.as_str(), "OIDC sign-in failed");
            Ok(Redirect::to(&sign_in_failed(&next)).into_response())
        }
    }
}

#[derive(Deserialize)]
pub struct OidcCallback {
    state: Option<String>,
    code: Option<String>,
}

/// `GET /api/web/oidc/callback`, where the provider sends the browser back: signs it in as the
/// user of the identity it signed in with, and returns to the app.
pub async fn finish_oidc(
    State(web): State<Web>,
    headers: HeaderMap,
    Query(callback): Query<OidcCallback>,
) -> Result<Response, WebError> {
    let provider = web.oidc.clone().ok_or(WebError::NotFound)?;
    let clear = web.oidc_cookie("", 0);
    let failed = |reason: &str, next: Option<String>| {
        tracing::warn!(reason, "OIDC sign-in failed");
        let next = sign_in_failed(next.as_deref().unwrap_or("/"));
        (
            AppendHeaders([(header::SET_COOKIE, clear.clone())]),
            Redirect::to(&next),
        )
            .into_response()
    };
    // The browser that comes back has to be the one that left.
    let Some(state) = callback
        .state
        .filter(|state| read_cookie(&headers, OIDC_COOKIE) == Some(state.as_str()))
    else {
        return Ok(failed("another browser's sign-in", None));
    };
    let Some(code) = callback.code else {
        return Ok(failed("refused by the provider", provider.abandon(&state)));
    };
    let (identity, next) = match provider.finish(&state, code).await {
        Ok(signed_in) => signed_in,
        Err(failure) => return Ok(failed(failure.as_str(), None)),
    };
    let user = web.users.of_identity(&identity)?;
    let started = web.browsers.start(&user.id)?;
    tracing::info!(user_id = %user.id, "signed in with OIDC");
    let session = web.session_cookie(&started.cookie, browser::LIFETIME.as_secs());
    Ok((
        AppendHeaders([(header::SET_COOKIE, session), (header::SET_COOKIE, clear)]),
        Redirect::to(&next),
    )
        .into_response())
}

#[derive(Serialize)]
pub struct LoginView {
    /// The registered device that asked, if it said which.
    device: Option<DeviceView>,
    /// The address the request came from, as this server saw it.
    address: Option<String>,
    /// When it was asked for, in seconds since the Unix epoch.
    asked_at: u64,
    confirmed: bool,
}

#[derive(Serialize)]
pub struct DeviceView {
    name: String,
    /// UU's platform number: 1 Windows, 2 Android, 3 iOS, 4 macOS.
    platform: u8,
}

/// `GET /api/web/logins/{id}`, for the page the QR code opens. Opening it counts as scanning
/// the code, so it needs no sign-in: the address is the secret.
pub async fn login(
    State(web): State<Web>,
    Path(id): Path<String>,
) -> Result<Json<LoginView>, WebError> {
    let (progress, requester) = web.logins.scan(&id).ok_or(WebError::NotFound)?;
    Ok(Json(LoginView {
        device: requester
            .device
            .map(|(name, platform)| DeviceView { name, platform }),
        address: requester.address.map(|address| address.to_string()),
        asked_at: requester
            .at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        confirmed: progress == Progress::Confirmed,
    }))
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Confirm,
    Deny,
}

#[derive(Deserialize)]
pub struct DecisionRequest {
    decision: Decision,
}

/// `POST /api/web/logins/{id}`: the signed-in user logs the device in as themselves, or
/// denies it.
pub async fn decide_login(
    signed_in: SignedIn,
    State(web): State<Web>,
    Path(id): Path<String>,
    Body(request): Body<DecisionRequest>,
) -> Result<StatusCode, WebError> {
    let user_id = &signed_in.user.id;
    let decided = match request.decision {
        Decision::Confirm => web.logins.confirm(&id, user_id),
        Decision::Deny => web.logins.deny(&id),
    };
    if !decided {
        return Err(WebError::NotFound);
    }
    match request.decision {
        Decision::Confirm => tracing::info!(%user_id, "QR login confirmed"),
        Decision::Deny => tracing::info!(%user_id, "QR login denied"),
    }
    Ok(StatusCode::NO_CONTENT)
}
