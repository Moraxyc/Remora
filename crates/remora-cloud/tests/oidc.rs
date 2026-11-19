mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing;
use axum::{Form, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use remora_cloud::{Config, OidcProvider, OidcRegistration};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const CLIENT_ID: &str = "remora";
const CLIENT_SECRET: &str = "test-client-secret";

/// A provider that approves whatever the test tells it to.
#[derive(Clone, Default)]
struct Idp {
    issuer: String,
    /// Issued codes, with what their ID token will say.
    codes: Arc<Mutex<HashMap<String, Grant>>>,
}

struct Grant {
    nonce: String,
    challenge: String,
    subject: String,
    name: String,
    /// Signs the ID token with the wrong key.
    forged: bool,
}

impl Idp {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
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

    /// The user approves the authorization request at `location`; returns the callback's
    /// query string.
    fn approve(&self, location: &str, subject: &str, name: &str, forged: bool) -> String {
        let query = query_of(location);
        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["redirect_uri"],
            format!("{}/api/web/oidc/callback", common::PUBLIC_URL)
        );
        assert!(query["scope"].split(' ').any(|scope| scope == "openid"));
        let code = format!("code-{subject}-{}", self.codes.lock().unwrap().len());
        self.codes.lock().unwrap().insert(
            code.clone(),
            Grant {
                nonce: query["nonce"].clone(),
                challenge: query["code_challenge"].clone(),
                subject: subject.into(),
                name: name.into(),
                forged,
            },
        );
        format!("code={code}&state={}", query["state"])
    }
}

fn query_of(url: &str) -> HashMap<String, String> {
    let (_, query) = url.split_once('?').unwrap();
    pairs(query)
}

fn pairs(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

fn decode(text: &str) -> String {
    let text = text.replace('+', " ");
    let mut bytes = Vec::new();
    let mut chars = text.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hex = [chars.next().unwrap(), chars.next().unwrap()];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).unwrap(), 16).unwrap());
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).unwrap()
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
    grant_type: String,
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
    assert_eq!(request.grant_type, "authorization_code");
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
    let key = if grant.forged {
        "not-the-secret"
    } else {
        CLIENT_SECRET
    };
    Ok(Json(json!({
        "access_token": "access",
        "token_type": "Bearer",
        "id_token": hs256(&claims, key),
    })))
}

fn hs256(claims: &Value, key: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let unsigned = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(unsigned.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{unsigned}.{signature}")
}

fn app(idp: &Idp) -> Router {
    let registration = OidcRegistration {
        issuer: idp.issuer.clone(),
        client_id: CLIENT_ID.into(),
        client_secret: Some(CLIENT_SECRET.into()),
        name: "Test IdP".into(),
    };
    remora_cloud::router(Config {
        auto_confirm: false,
        oidc: Some(OidcProvider::new(registration, common::PUBLIC_URL).unwrap()),
        ..common::config()
    })
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

async fn fetch(app: &Router, path: &str, cookie: Option<&str>) -> Answer {
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Answer {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

impl Answer {
    fn location(&self) -> &str {
        assert_eq!(self.status, StatusCode::SEE_OTHER);
        self.headers[header::LOCATION].to_str().unwrap()
    }

    /// The cookies set, as `name=value`, with their attributes.
    fn cookies(&self) -> Vec<(String, String)> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|cookie| {
                let cookie = cookie.to_str().unwrap();
                let (pair, attributes) = cookie.split_once(';').unwrap();
                (pair.to_owned(), attributes.to_owned())
            })
            .collect()
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.cookies()
            .into_iter()
            .map(|(pair, _)| pair)
            .find(|pair| pair.starts_with(&format!("{name}=")))
    }
}

/// Signs in at the provider as `subject`; returns the final answer of Remora's callback.
async fn sign_in(app: &Router, idp: &Idp, next: &str, subject: &str, name: &str) -> Answer {
    let start = fetch(app, &format!("/api/web/oidc/start?next={next}"), None).await;
    let flow = start.cookie("remora_oidc").unwrap();
    let callback = idp.approve(start.location(), subject, name, false);
    fetch(
        app,
        &format!("/api/web/oidc/callback?{callback}"),
        Some(&flow),
    )
    .await
}

#[tokio::test]
async fn users_sign_in_with_the_provider() {
    let idp = Idp::start().await;
    let app = app(&idp);
    let session = fetch(&app, "/api/web/session", None).await;
    assert_eq!(session.body["oidc"], "Test IdP");

    let start = fetch(&app, "/api/web/oidc/start?next=/login/abc", None).await;
    assert!(
        start
            .location()
            .starts_with(&format!("{}/authorize?", idp.issuer))
    );
    let (flow, attributes) = start.cookies().remove(0);
    assert!(flow.starts_with("remora_oidc="));
    for attribute in ["Path=/api/web/oidc", "HttpOnly", "SameSite=Lax", "Secure"] {
        assert!(attributes.contains(attribute), "{attributes}");
    }

    let callback = idp.approve(start.location(), "alice", "Alice", false);
    let back = fetch(
        &app,
        &format!("/api/web/oidc/callback?{callback}"),
        Some(&flow),
    )
    .await;
    assert_eq!(back.location(), "/login/abc");
    assert_eq!(
        back.cookie("remora_oidc").as_deref(),
        Some("remora_oidc="),
        "cleared"
    );
    let signed_in = back.cookie("remora_session").unwrap();

    let session = fetch(&app, "/api/web/session", Some(&signed_in)).await;
    assert_eq!(
        session.body["signed_in"]["user"],
        json!({ "id": "2", "name": "Alice" })
    );

    let again = sign_in(&app, &idp, "/", "alice", "Alice Liddell").await;
    let session = fetch(
        &app,
        "/api/web/session",
        again.cookie("remora_session").as_deref(),
    )
    .await;
    assert_eq!(
        session.body["signed_in"]["user"],
        json!({ "id": "2", "name": "Alice Liddell" }),
        "the same user, renamed"
    );
    let bob = sign_in(&app, &idp, "/", "bob", "Bob").await;
    let session = fetch(
        &app,
        "/api/web/session",
        bob.cookie("remora_session").as_deref(),
    )
    .await;
    assert_eq!(session.body["signed_in"]["user"]["id"], "3");
}

#[tokio::test]
async fn only_the_browser_that_left_comes_back_signed_in() {
    let idp = Idp::start().await;
    let app = app(&idp);
    let start = fetch(&app, "/api/web/oidc/start?next=/devices", None).await;
    let callback = idp.approve(start.location(), "alice", "Alice", false);

    for cookie in [None, Some("remora_oidc=another-state")] {
        let back = fetch(&app, &format!("/api/web/oidc/callback?{callback}"), cookie).await;
        assert_eq!(back.location(), "/?sign_in=failed");
        assert!(back.cookie("remora_session").is_none());
    }
}

#[tokio::test]
async fn forged_id_tokens_are_refused() {
    let idp = Idp::start().await;
    let app = app(&idp);
    let start = fetch(&app, "/api/web/oidc/start?next=/devices", None).await;
    let flow = start.cookie("remora_oidc").unwrap();
    let callback = idp.approve(start.location(), "alice", "Alice", true);
    let back = fetch(
        &app,
        &format!("/api/web/oidc/callback?{callback}"),
        Some(&flow),
    )
    .await;
    assert_eq!(back.location(), "/?sign_in=failed");
    assert!(back.cookie("remora_session").is_none());
}

#[tokio::test]
async fn a_refused_sign_in_returns_to_where_it_started() {
    let idp = Idp::start().await;
    let app = app(&idp);
    let start = fetch(&app, "/api/web/oidc/start?next=/devices", None).await;
    let flow = start.cookie("remora_oidc").unwrap();
    let state = &query_of(start.location())["state"];
    let back = fetch(
        &app,
        &format!("/api/web/oidc/callback?error=access_denied&state={state}"),
        Some(&flow),
    )
    .await;
    assert_eq!(back.location(), "/devices?sign_in=failed");
}

#[tokio::test]
async fn sign_ins_return_only_into_the_app() {
    let idp = Idp::start().await;
    let app = app(&idp);
    for next in [
        "https://elsewhere.example",
        "//elsewhere.example",
        "/api/web/session",
    ] {
        let back = sign_in(&app, &idp, next, "alice", "Alice").await;
        assert_eq!(back.location(), "/", "{next}");
    }
}

#[tokio::test]
async fn an_unreachable_provider_fails_the_sign_in() {
    let registration = OidcRegistration {
        issuer: "http://127.0.0.1:9".into(),
        client_id: CLIENT_ID.into(),
        client_secret: None,
        name: "Gone".into(),
    };
    let app = remora_cloud::router(Config {
        oidc: Some(OidcProvider::new(registration, common::PUBLIC_URL).unwrap()),
        ..common::config()
    });
    let start = fetch(&app, "/api/web/oidc/start?next=/devices", None).await;
    assert_eq!(start.location(), "/devices?sign_in=failed");
}

async fn web_call(
    app: &Router,
    method: Method,
    path: &str,
    cookie: &str,
    csrf: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, cookie);
    if let Some(csrf) = csrf {
        request = request.header("x-csrf-token", csrf);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Answer {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

#[tokio::test]
async fn users_manage_only_their_own_devices_and_sessions() {
    let idp = Idp::start().await;
    let app = app(&idp);

    let alice_cookie = sign_in(&app, &idp, "/", "alice", "Alice")
        .await
        .cookie("remora_session")
        .unwrap();
    let bob_cookie = sign_in(&app, &idp, "/", "bob", "Bob")
        .await
        .cookie("remora_session")
        .unwrap();
    let alice_csrf = fetch(&app, "/api/web/session", Some(&alice_cookie))
        .await
        .body["signed_in"]["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let bob_csrf = fetch(&app, "/api/web/session", Some(&bob_cookie))
        .await
        .body["signed_in"]["csrf"]
        .as_str()
        .unwrap()
        .to_owned();

    // Alice confirms a device and binds it by creating a room.
    let device_id = common::register_named(&app, "alice-host", "Alice Desktop").await;
    let qr_req = Request::post("/api/v1/qrcode/gen/login")
        .header("x-param-device-id", &device_id)
        .body(Body::empty())
        .unwrap();
    let qr_bytes = app
        .clone()
        .oneshot(qr_req)
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let qr_data: Value = remora_proto::cloud::parse_response(200, &qr_bytes).unwrap();
    let qr_id = qr_data["qrcode_jump_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap();
    let confirm = web_call(
        &app,
        Method::POST,
        &format!("/api/web/logins/{qr_id}"),
        &alice_cookie,
        Some(&alice_csrf),
        Some(json!({ "decision": "confirm" })),
    )
    .await;
    assert_eq!(confirm.status, StatusCode::NO_CONTENT);
    let login: Value = common::post(
        &app,
        "/api/v1/login/by_qrcode",
        Some(
            &json!({
                "token": qr_data["status_query_ticket"],
                "qrcode_jump_url": qr_data["qrcode_jump_url"],
            })
            .to_string(),
        ),
    )
    .await
    .unwrap();
    let alice_token = login["token"].as_str().unwrap();
    common::create_room(&app, alice_token, &device_id).await;

    // Alice sees her device, client session and browser; Bob sees none of Alice's.
    let alice_devices = web_call(
        &app,
        Method::GET,
        "/api/web/devices",
        &alice_cookie,
        None,
        None,
    )
    .await;
    assert_eq!(alice_devices.body.as_array().unwrap().len(), 1);
    let bob_devices = web_call(
        &app,
        Method::GET,
        "/api/web/devices",
        &bob_cookie,
        None,
        None,
    )
    .await;
    assert_eq!(bob_devices.body, json!([]));

    let alice_sessions = web_call(
        &app,
        Method::GET,
        "/api/web/client-sessions",
        &alice_cookie,
        None,
        None,
    )
    .await;
    let alice_session_id = alice_sessions.body[0]["id"].as_str().unwrap();
    let bob_sessions = web_call(
        &app,
        Method::GET,
        "/api/web/client-sessions",
        &bob_cookie,
        None,
        None,
    )
    .await;
    assert_eq!(bob_sessions.body, json!([]));

    let alice_browsers = web_call(
        &app,
        Method::GET,
        "/api/web/browsers",
        &alice_cookie,
        None,
        None,
    )
    .await;
    let alice_browser_id = alice_browsers.body[0]["id"].as_str().unwrap();
    let bob_browsers = web_call(
        &app,
        Method::GET,
        "/api/web/browsers",
        &bob_cookie,
        None,
        None,
    )
    .await;
    assert_eq!(bob_browsers.body.as_array().unwrap().len(), 1);
    assert_ne!(bob_browsers.body[0]["id"], alice_browser_id);

    // Bob cannot unbind Alice's device or revoke her client or browser sessions.
    for path in [
        format!("/api/web/devices/{device_id}"),
        format!("/api/web/client-sessions/{alice_session_id}"),
        format!("/api/web/browsers/{alice_browser_id}"),
    ] {
        let attempt = web_call(
            &app,
            Method::DELETE,
            &path,
            &bob_cookie,
            Some(&bob_csrf),
            None,
        )
        .await;
        assert_eq!(attempt.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert!(
        common::get::<Value>(&app, "/api/v1/user/info", Some(alice_token))
            .await
            .is_ok()
    );
}
