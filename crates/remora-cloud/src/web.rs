//! The owner's web pages: a QR login's page, where the owner confirms or denies it after
//! signing in with the password. A signed-in browser keeps a browser token as its cookie.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{HeaderName, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::PublicUrl;
use crate::login::{Logins, Progress, Requester};
use crate::password::OwnerPassword;
use crate::token::{BROWSER_TOKEN_LIFETIME, Browser, TokenKey};

const COOKIE: &str = "remora_browser";

/// What the pages work with.
#[derive(Clone)]
pub struct Web {
    pub logins: Logins,
    pub tokens: Arc<TokenKey>,
    /// Without a password, nobody can sign in.
    pub password: Option<Arc<OwnerPassword>>,
    pub public_url: PublicUrl,
}

impl Web {
    /// The browser the request comes from, if the owner signed in with it.
    fn signed_in(&self, headers: &HeaderMap) -> Option<Browser> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|cookies| cookies.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .find(|(name, _)| *name == COOKIE)
            .and_then(|(_, token)| self.tokens.verify_browser(token))
    }

    fn page(&self, status: StatusCode, title: &str, body: &str) -> Response {
        let mut html = String::from(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
        );
        let _ = write!(
            html,
            "<title>{} · Remora</title><style>{STYLE}</style></head><body><main>",
            escape(title)
        );
        if !self.public_url.is_secure() {
            html.push_str(
                "<p class=\"warning\">This server is reached over plain HTTP, so this page and \
                 your password travel unencrypted.</p>",
            );
        }
        let _ = write!(
            html,
            "<h1>{}</h1>{body}</main></body></html>",
            escape(title)
        );
        (
            status,
            PAGE_HEADERS.map(|(name, value)| (name, HeaderValue::from_static(value))),
            html,
        )
            .into_response()
    }

    fn expired(&self) -> Response {
        self.page(
            StatusCode::NOT_FOUND,
            "Login expired",
            "<p>This QR code has expired or has been used. Show a new one on the device.</p>",
        )
    }

    fn sign_in_form(&self, status: StatusCode, next: &str, error: Option<&str>) -> Response {
        if self.password.is_none() {
            return self.page(
                StatusCode::NOT_FOUND,
                "Signing in is off",
                "<p>This server has no owner password, so nobody can sign in.</p>",
            );
        }
        let mut body = String::new();
        if let Some(error) = error {
            let _ = write!(body, "<p class=\"error\">{}</p>", escape(error));
        }
        let _ = write!(
            body,
            "<p>Sign in to confirm logins.</p>\
             <form method=\"post\" action=\"/signin\">\
             <input type=\"hidden\" name=\"next\" value=\"{}\">\
             <label>Password <input type=\"password\" name=\"password\" \
             autocomplete=\"current-password\" required autofocus></label>\
             <button>Sign in</button></form>",
            escape(next)
        );
        self.page(status, "Sign in", &body)
    }

    fn decision_form(&self, id: &str, requester: &Requester, browser: &Browser) -> Response {
        let device = match &requester.device {
            Some((name, platform)) if !name.is_empty() => {
                format!("{} ({})", escape(name), platform_name(*platform))
            }
            Some((_, platform)) => format!("Unnamed device ({})", platform_name(*platform)),
            None => "Unregistered device".to_owned(),
        };
        let address = requester
            .address
            .map_or_else(|| "unknown".to_owned(), |address| address.to_string());
        let body = format!(
            "<p>A device asks to log in to Remora as you.</p><dl>\
             <dt>Device</dt><dd>{device}</dd>\
             <dt>Address</dt><dd>{address}</dd>\
             <dt>Asked at</dt><dd>{}</dd></dl>\
             <p>Log it in only if you asked for this yourself, just now.</p>\
             <form method=\"post\" action=\"/login/{}\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
             <button name=\"decision\" value=\"confirm\">Log in</button> \
             <button name=\"decision\" value=\"deny\" class=\"secondary\">Deny</button></form>",
            httpdate::fmt_http_date(requester.at),
            escape(id),
            escape(&browser.csrf),
        );
        self.page(StatusCode::OK, "Log in?", &body)
    }
}

/// Every page is private to the owner, carries a secret in its address, and has buttons that
/// must not be clicked through someone else's frame.
const PAGE_HEADERS: [(HeaderName, &str); 6] = [
    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
    (header::CACHE_CONTROL, "no-store"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::X_FRAME_OPTIONS, "DENY"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
         frame-ancestors 'none'; base-uri 'none'",
    ),
];

const STYLE: &str = "body{font-family:system-ui,sans-serif;margin:0;padding:1rem;\
    background:#f6f6f6;color:#222}main{max-width:28rem;margin:2rem auto;background:#fff;\
    padding:1.5rem;border-radius:.5rem}dt{font-weight:600}dd{margin:0 0 .75rem}\
    input[type=password]{display:block;width:100%;box-sizing:border-box;margin:.5rem 0 1rem;\
    padding:.5rem;font-size:1rem}button{font-size:1rem;padding:.5rem 1.25rem;border:0;\
    border-radius:.25rem;background:#1a5fb4;color:#fff}button.secondary{background:#ddd;\
    color:#222}.warning,.error{padding:.75rem;border-radius:.25rem;background:#fdecc8}\
    .error{background:#f8d7da}";

/// UU's platform numbers, as `device/windows/init` and `be-controlled` use them.
fn platform_name(platform: u8) -> &'static str {
    match platform {
        1 => "Windows",
        2 => "Android",
        3 => "iOS",
        4 => "macOS",
        _ => "unknown platform",
    }
}

/// `text` safe to put in HTML text and in quoted attribute values.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Compares secrets without stopping at the first difference.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |diff, (x, y)| diff | (x ^ y))
            == 0
}

fn login_path(id: &str) -> String {
    format!("/login/{id}")
}

/// `GET /login/{id}`, the address in a QR code. Opening it counts as scanning the code.
pub async fn show_login(
    State(web): State<Web>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some((progress, requester)) = web.logins.scan(&id) else {
        return web.expired();
    };
    if progress == Progress::Confirmed {
        return web.page(
            StatusCode::OK,
            "Logged in",
            "<p>This login is confirmed. You can close this page.</p>",
        );
    }
    match web.signed_in(&headers) {
        Some(browser) => web.decision_form(&id, &requester, &browser),
        None => web.sign_in_form(StatusCode::OK, &login_path(&id), None),
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Confirm,
    Deny,
}

#[derive(Deserialize)]
pub struct DecisionForm {
    csrf: String,
    decision: Decision,
}

/// `POST /login/{id}`: the owner's decision on a login.
pub async fn decide_login(
    State(web): State<Web>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<DecisionForm>,
) -> Response {
    let Some(browser) = web.signed_in(&headers) else {
        return Redirect::to(&login_path(&id)).into_response();
    };
    if !same_secret(&form.csrf, &browser.csrf) {
        return web.page(
            StatusCode::FORBIDDEN,
            "Try again",
            "<p>This form is out of date. Open the QR code's address again.</p>",
        );
    }
    let confirm = form.decision == Decision::Confirm;
    if !web.logins.decide(&id, confirm) {
        return web.expired();
    }
    if confirm {
        tracing::info!("QR login confirmed by the owner");
        web.page(
            StatusCode::OK,
            "Logged in",
            "<p>The device is logging in. You can close this page.</p>",
        )
    } else {
        tracing::info!("QR login denied by the owner");
        web.page(
            StatusCode::OK,
            "Denied",
            "<p>The device was not logged in. You can close this page.</p>",
        )
    }
}

#[derive(Deserialize)]
pub struct SignInForm {
    password: String,
    /// The login page to return to.
    next: String,
}

/// `POST /signin`: signs the browser in with the owner's password, and returns to the login
/// page it came from. Failed attempts are not limited yet.
pub async fn sign_in(State(web): State<Web>, Form(form): Form<SignInForm>) -> Response {
    let is_login_page = form
        .next
        .strip_prefix("/login/")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if !is_login_page {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(password) = web.password.clone() else {
        return web.sign_in_form(StatusCode::NOT_FOUND, &form.next, None);
    };
    let attempt = form.password;
    let correct = tokio::task::spawn_blocking(move || password.verify(&attempt))
        .await
        .unwrap_or(false);
    if !correct {
        tracing::warn!("owner sign-in failed");
        return web.sign_in_form(
            StatusCode::UNAUTHORIZED,
            &form.next,
            Some("Wrong password."),
        );
    }

    tracing::info!("owner signed in");
    let mut cookie = format!(
        "{COOKIE}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict",
        web.tokens.issue_browser(),
        BROWSER_TOKEN_LIFETIME.as_secs()
    );
    if web.public_url.is_secure() {
        cookie.push_str("; Secure");
    }
    let cookie = HeaderValue::from_str(&cookie).expect("tokens are base64url");
    ([(header::SET_COOKIE, cookie)], Redirect::to(&form.next)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_quotes() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn compares_secrets() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "ab"));
    }
}
