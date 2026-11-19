mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use remora_cloud::{Config, OwnerPassword};
use remora_proto::cloud::ApiError;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";

/// Logins confirmed by the owner, who signs in with [`PASSWORD`].
fn app() -> Router {
    remora_cloud::router(Config {
        auto_confirm: false,
        owner_password: Some(OwnerPassword::hash(PASSWORD)),
        ..common::config()
    })
}

struct Page {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> Page {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Page {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn open(app: &Router, path: &str, cookie: Option<&str>) -> Page {
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn submit(app: &Router, path: &str, cookie: Option<&str>, form: &str) -> Page {
    let mut request =
        Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::from(form.to_owned())).unwrap()).await
}

/// A login started from the registered device `device_id`.
#[derive(Clone)]
struct Qr {
    ticket: String,
    url: String,
    /// The page's path on this server.
    path: String,
}

async fn generate(app: &Router, device_id: &str) -> Qr {
    let request = Request::post("/api/v1/qrcode/gen/login")
        .header("x-param-device-id", device_id)
        .body(Body::empty())
        .unwrap();
    let page = send(app, request).await;
    let data: Value = remora_proto::cloud::parse_response(200, page.body.as_bytes()).unwrap();
    let url = data["qrcode_jump_url"].as_str().unwrap().to_owned();
    Qr {
        ticket: data["status_query_ticket"].as_str().unwrap().to_owned(),
        path: url.strip_prefix(common::PUBLIC_URL).unwrap().to_owned(),
        url,
    }
}

impl Qr {
    async fn status(&self, app: &Router, last: u8) -> Result<u8, ApiError> {
        let body =
            json!({ "token": self.ticket, "qrcode_jump_url": self.url, "login_status": last });
        let status: Value =
            common::post(app, "/api/v1/qrcode/login/status", Some(&body.to_string())).await?;
        Ok(status["status"].as_u64().unwrap().try_into().unwrap())
    }

    async fn log_in(&self, app: &Router) -> Result<Value, ApiError> {
        let body = json!({ "token": self.ticket, "qrcode_jump_url": self.url });
        common::post(app, "/api/v1/login/by_qrcode", Some(&body.to_string())).await
    }
}

/// Signs in for `qr`'s page; returns the cookie to send back.
async fn sign_in(app: &Router, qr: &Qr) -> String {
    let form = format!("password={}&next={}", PASSWORD.replace(' ', "+"), qr.path);
    let page = submit(app, "/signin", None, &form).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.headers[header::LOCATION], qr.path.as_str());
    let cookie = page.headers[header::SET_COOKIE].to_str().unwrap();
    cookie.split(';').next().unwrap().to_owned()
}

/// The CSRF token in a decision form.
fn csrf(page: &Page) -> String {
    let (_, rest) = page.body.split_once("name=\"csrf\" value=\"").unwrap();
    rest.split('"').next().unwrap().to_owned()
}

#[tokio::test]
async fn the_owner_confirms_a_login_on_its_page() {
    let app = app();
    let device_id = common::register_named(&app, "client-a", "<b>Desk</b>").await;
    let qr = generate(&app, &device_id).await;
    assert!(qr.path.starts_with("/login/"));
    assert_eq!(
        qr.log_in(&app).await.map(drop),
        Err(ApiError::QrCodeExpired)
    );

    let page = open(&app, &qr.path, None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("type=\"password\""));
    assert_eq!(qr.status(&app, 1).await, Ok(2), "opening the page scans it");

    let wrong = submit(
        &app,
        "/signin",
        None,
        &format!("password=hunter2&next={}", qr.path),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.headers.get(header::SET_COOKIE).is_none());

    let cookie = sign_in(&app, &qr).await;
    let page = open(&app, &qr.path, Some(&cookie)).await;
    assert!(page.body.contains("&lt;b&gt;Desk&lt;/b&gt; (Windows)"));
    assert!(!page.body.contains("<b>Desk"));

    let poll = tokio::spawn({
        let (app, qr) = (app.clone(), qr.clone());
        async move { qr.status(&app, 2).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!poll.is_finished(), "the poll waits for a change");
    let form = format!("csrf={}&decision=confirm", csrf(&page));
    assert!(page.body.contains(&format!("action=\"{}\"", qr.path)));
    let confirmed = submit(&app, &qr.path, Some(&cookie), &form).await;
    assert_eq!(confirmed.status, StatusCode::OK);
    let status = tokio::time::timeout(Duration::from_secs(5), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, Ok(4));

    let login = qr.log_in(&app).await.unwrap();
    assert!(
        login["token"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
    );
    assert_eq!(
        open(&app, &qr.path, Some(&cookie)).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_denied_login_is_over() {
    let app = app();
    let device_id = common::register(&app, "client-a").await;
    let qr = generate(&app, &device_id).await;
    let cookie = sign_in(&app, &qr).await;
    let page = open(&app, &qr.path, Some(&cookie)).await;

    let form = format!("csrf={}&decision=deny", csrf(&page));
    let denied = submit(&app, &qr.path, Some(&cookie), &form).await;
    assert_eq!(denied.status, StatusCode::OK);
    assert_eq!(qr.status(&app, 2).await, Err(ApiError::QrCodeExpired));
    assert_eq!(
        qr.log_in(&app).await.map(drop),
        Err(ApiError::QrCodeExpired)
    );
}

#[tokio::test]
async fn decisions_need_a_signed_in_browser_and_its_csrf_token() {
    let app = app();
    let device_id = common::register(&app, "client-a").await;
    let qr = generate(&app, &device_id).await;

    let signed_out = submit(&app, &qr.path, None, "csrf=x&decision=confirm").await;
    assert_eq!(signed_out.status, StatusCode::SEE_OTHER);
    assert_eq!(signed_out.headers[header::LOCATION], qr.path.as_str());

    let cookie = sign_in(&app, &qr).await;
    let forged = submit(&app, &qr.path, Some(&cookie), "csrf=x&decision=confirm").await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let bogus = submit(
        &app,
        &qr.path,
        Some("remora_browser=x"),
        "csrf=x&decision=confirm",
    )
    .await;
    assert_eq!(bogus.status, StatusCode::SEE_OTHER);

    assert_eq!(qr.status(&app, 4).await, Ok(1), "still undecided");
}

#[tokio::test]
async fn the_cookie_is_kept_from_scripts_and_other_sites() {
    let app = app();
    let device_id = common::register(&app, "client-a").await;
    let qr = generate(&app, &device_id).await;
    let form = format!("password={}&next={}", PASSWORD.replace(' ', "+"), qr.path);
    let page = submit(&app, "/signin", None, &form).await;
    let cookie = page.headers[header::SET_COOKIE].to_str().unwrap();
    for attribute in [
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Path=/",
        "Max-Age=2592000",
    ] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
}

#[tokio::test]
async fn pages_are_not_cached_framed_or_referred() {
    let app = app();
    let device_id = common::register(&app, "client-a").await;
    let qr = generate(&app, &device_id).await;
    for page in [
        open(&app, &qr.path, None).await,
        open(&app, "/login/unknown", None).await,
    ] {
        assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(page.headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(page.headers[header::REFERRER_POLICY], "no-referrer");
        let policy = page.headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(policy.contains("frame-ancestors 'none'"), "{policy}");
    }
}

#[tokio::test]
async fn signing_in_returns_only_to_login_pages() {
    let app = app();
    for next in [
        "https://elsewhere.example",
        "//elsewhere.example",
        "/login/",
        "/signin",
    ] {
        let form = format!("password={}&next={next}", PASSWORD.replace(' ', "+"));
        let page = submit(&app, "/signin", None, &form).await;
        assert_eq!(page.status, StatusCode::BAD_REQUEST, "{next}");
    }
}

#[tokio::test]
async fn auto_confirmed_logins_skip_the_page() {
    let app = remora_cloud::router(common::config());
    let device_id = common::register(&app, "client-a").await;
    let qr = generate(&app, &device_id).await;
    assert_eq!(qr.status(&app, 1).await, Ok(4));
    let page = open(&app, &qr.path, None).await;
    assert!(page.body.contains("confirmed"));
    assert!(qr.log_in(&app).await.is_ok());
}
