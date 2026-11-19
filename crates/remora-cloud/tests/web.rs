mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use remora_cloud::{Config, OwnerPassword};
use remora_proto::cloud::ApiError;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";

/// Logins confirmed by a signed-in user; the owner signs in with [`PASSWORD`].
fn app() -> Router {
    remora_cloud::router(Config {
        auto_confirm: false,
        owner_password: Some(OwnerPassword::hash(PASSWORD)),
        ..common::config()
    })
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

/// A browser: it keeps the session cookie and sends the CSRF token it was given.
#[derive(Default, Clone)]
struct Browser {
    cookie: Option<String>,
    csrf: Option<String>,
}

impl Browser {
    async fn call(
        &mut self,
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Answer {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(csrf) = &self.csrf {
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
        if let Some(cookie) = parts.headers.get(header::SET_COOKIE) {
            let cookie = cookie.to_str().unwrap().split(';').next().unwrap();
            self.cookie = (!cookie.ends_with('=')).then(|| cookie.to_owned());
        }
        let bytes = body.collect().await.unwrap().to_bytes();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn sign_in(&mut self, app: &Router) {
        let signed_in = self
            .call(
                app,
                Method::POST,
                "/api/web/session",
                Some(json!({ "password": PASSWORD })),
            )
            .await;
        assert_eq!(signed_in.status, StatusCode::NO_CONTENT);
        let session = self.call(app, Method::GET, "/api/web/session", None).await;
        self.csrf = Some(
            session.body["signed_in"]["csrf"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }

    async fn decide(&mut self, app: &Router, qr: &Qr, decision: &str) -> StatusCode {
        let body = json!({ "decision": decision });
        self.call(app, Method::POST, &qr.api(), Some(body))
            .await
            .status
    }
}

/// A login started from the registered device `device_id`.
#[derive(Clone)]
struct Qr {
    ticket: String,
    url: String,
    id: String,
}

impl Qr {
    async fn generate(app: &Router, device_id: &str) -> Self {
        let request = Request::post("/api/v1/qrcode/gen/login")
            .header("x-param-device-id", device_id)
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let data: Value = remora_proto::cloud::parse_response(200, &bytes).unwrap();
        let url = data["qrcode_jump_url"].as_str().unwrap().to_owned();
        let page = url.strip_prefix(common::PUBLIC_URL).unwrap();
        Self {
            ticket: data["status_query_ticket"].as_str().unwrap().to_owned(),
            id: page.strip_prefix("/login/").unwrap().to_owned(),
            url,
        }
    }

    fn api(&self) -> String {
        format!("/api/web/logins/{}", self.id)
    }

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

#[tokio::test]
async fn a_signed_in_user_confirms_a_login() {
    let app = app();
    let device_id = common::register_named(&app, "client-a", "Desk").await;
    let qr = Qr::generate(&app, &device_id).await;
    assert_eq!(
        qr.log_in(&app).await.map(drop),
        Err(ApiError::QrCodeExpired)
    );

    let mut browser = Browser::default();
    let login = browser.call(&app, Method::GET, &qr.api(), None).await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(
        login.body["device"],
        json!({ "name": "Desk", "platform": 1 })
    );
    assert_eq!(login.body["confirmed"], false);
    assert!(login.body["asked_at"].as_u64().is_some_and(|at| at > 0));
    assert_eq!(qr.status(&app, 1).await, Ok(2), "opening the page scans it");

    assert_eq!(
        browser.decide(&app, &qr, "confirm").await,
        StatusCode::UNAUTHORIZED
    );
    browser.sign_in(&app).await;

    let poll = tokio::spawn({
        let (app, qr) = (app.clone(), qr.clone());
        async move { qr.status(&app, 2).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!poll.is_finished(), "the poll waits for a change");
    assert_eq!(
        browser.decide(&app, &qr, "confirm").await,
        StatusCode::NO_CONTENT
    );
    let status = tokio::time::timeout(Duration::from_secs(5), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, Ok(4));

    let login = qr.log_in(&app).await.unwrap();
    assert_eq!(login["user_id"], "1", "as the user who confirmed");
    assert_eq!(login["nickname"], "Owner");
    let gone = browser.call(&app, Method::GET, &qr.api(), None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_denied_login_is_over() {
    let app = app();
    let device_id = common::register(&app, "client-a").await;
    let qr = Qr::generate(&app, &device_id).await;
    let mut browser = Browser::default();
    browser.sign_in(&app).await;

    assert_eq!(
        browser.decide(&app, &qr, "deny").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(qr.status(&app, 1).await, Err(ApiError::QrCodeExpired));
    assert_eq!(
        qr.log_in(&app).await.map(drop),
        Err(ApiError::QrCodeExpired)
    );
    assert_eq!(
        browser.decide(&app, &qr, "confirm").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn changes_need_the_browsers_csrf_token() {
    let app = app();
    let device_id = common::register(&app, "client-a").await;
    let qr = Qr::generate(&app, &device_id).await;
    let mut browser = Browser::default();
    browser.sign_in(&app).await;

    let mut forged = Browser {
        csrf: Some("forged".into()),
        ..browser.clone()
    };
    let refused = forged
        .call(
            &app,
            Method::POST,
            &qr.api(),
            Some(json!({ "decision": "confirm" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["error"], "csrf");
    let mut stranger = Browser {
        cookie: Some("remora_session=unknown".into()),
        ..browser.clone()
    };
    assert_eq!(
        stranger.decide(&app, &qr, "confirm").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(qr.status(&app, 4).await, Ok(1), "still undecided");
}

#[tokio::test]
async fn signing_in_and_out() {
    let app = app();
    let mut browser = Browser::default();
    let session = browser
        .call(&app, Method::GET, "/api/web/session", None)
        .await;
    assert_eq!(
        session.body,
        json!({ "signed_in": null, "password": true, "oidc": null, "secure": true })
    );
    assert_eq!(session.headers[header::CACHE_CONTROL], "no-store");

    let wrong = browser
        .call(
            &app,
            Method::POST,
            "/api/web/session",
            Some(json!({ "password": "hunter2" })),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.body["error"], "wrong_password");
    assert!(wrong.headers.get(header::SET_COOKIE).is_none());

    let signed_in = browser
        .call(
            &app,
            Method::POST,
            "/api/web/session",
            Some(json!({ "password": PASSWORD })),
        )
        .await;
    let cookie = signed_in.headers[header::SET_COOKIE].to_str().unwrap();
    for attribute in [
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Path=/",
        "Max-Age=2592000",
    ] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
    let session = browser
        .call(&app, Method::GET, "/api/web/session", None)
        .await;
    assert_eq!(
        session.body["signed_in"]["user"],
        json!({ "id": "1", "name": "Owner" })
    );
    let kept = browser.clone();

    browser.csrf = Some(
        session.body["signed_in"]["csrf"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    let signed_out = browser
        .call(&app, Method::DELETE, "/api/web/session", None)
        .await;
    assert_eq!(signed_out.status, StatusCode::NO_CONTENT);
    assert!(browser.cookie.is_none(), "the cookie is cleared");

    let mut kept = kept;
    let session = kept.call(&app, Method::GET, "/api/web/session", None).await;
    assert_eq!(
        session.body["signed_in"],
        Value::Null,
        "the old cookie is dead"
    );
}

#[tokio::test]
async fn bodies_must_be_json() {
    let app = app();
    let request = Request::post("/api/web/session")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("password={PASSWORD}")))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn without_a_password_nobody_signs_in_with_one() {
    let app = remora_cloud::router(common::config());
    let mut browser = Browser::default();
    let session = browser
        .call(&app, Method::GET, "/api/web/session", None)
        .await;
    assert_eq!(session.body["password"], false);
    let refused = browser
        .call(
            &app,
            Method::POST,
            "/api/web/session",
            Some(json!({ "password": "" })),
        )
        .await;
    assert_eq!(refused.body["error"], "password_off");
}

#[tokio::test]
async fn auto_confirmed_logins_need_nobody() {
    let app = remora_cloud::router(common::config());
    let device_id = common::register(&app, "client-a").await;
    let qr = Qr::generate(&app, &device_id).await;
    assert_eq!(qr.status(&app, 1).await, Ok(4));
    let mut browser = Browser::default();
    let login = browser.call(&app, Method::GET, &qr.api(), None).await;
    assert_eq!(login.body["confirmed"], true);
    assert!(qr.log_in(&app).await.is_ok());
}

#[tokio::test]
async fn the_qr_code_opens_the_app() {
    let app = app();
    let device_id = common::register(&app, "client-a").await;
    let qr = Qr::generate(&app, &device_id).await;
    let request = Request::get(format!("/login/{}", qr.id))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
}
