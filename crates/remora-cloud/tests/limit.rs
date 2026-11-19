mod common;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use remora_cloud::{Config, OwnerPassword, TrustedProxies, TrustedProxy};
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";
const PROXY: &str = "127.0.0.1:40000";

/// Behind a reverse proxy on localhost; the owner signs in with [`PASSWORD`].
fn app() -> Router {
    let proxy = TrustedProxy::try_from("127.0.0.1".to_owned()).unwrap();
    remora_cloud::router(Config {
        owner_password: Some(OwnerPassword::hash(PASSWORD)),
        trusted_proxies: TrustedProxies::new([proxy]),
        ..common::config()
    })
}

/// Sends `request` over a connection from `peer`, forwarded for `client` if given.
async fn send(
    app: &Router,
    request: axum::http::request::Builder,
    peer: &str,
    client: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = request.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    if let Some(client) = client {
        request = request.header("x-forwarded-for", client);
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
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn sign_in(app: &Router, client: &str, password: &str) -> (StatusCode, Value) {
    let body = json!({ "password": password });
    let request = Request::post("/api/web/session");
    send(app, request, PROXY, Some(client), Some(body)).await
}

async fn generate(app: &Router, peer: &str, client: Option<&str>) -> (StatusCode, Value) {
    send(
        app,
        Request::post("/api/v1/qrcode/gen/login"),
        peer,
        client,
        None,
    )
    .await
}

#[tokio::test]
async fn login_pages_show_the_client_a_trusted_proxy_names() {
    let app = app();
    let address_of = async |peer: &str, client: Option<&str>| {
        let (_, generated) = generate(&app, peer, client).await;
        let url = generated["data"]["qrcode_jump_url"].as_str().unwrap();
        let id = url.rsplit_once('/').unwrap().1;
        let request = Request::get(format!("/api/web/logins/{id}"));
        let (_, login) = send(&app, request, PROXY, None, None).await;
        login["address"].clone()
    };
    assert_eq!(address_of(PROXY, Some("203.0.113.9")).await, "203.0.113.9");
    assert_eq!(
        address_of("198.51.100.1:5000", Some("203.0.113.9")).await,
        "198.51.100.1",
        "only a trusted proxy is believed"
    );
}

#[tokio::test]
async fn password_guesses_are_limited_per_client() {
    let app = app();
    for _ in 0..5 {
        let (status, _) = sign_in(&app, "203.0.113.9", "hunter2").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, body) = sign_in(&app, "203.0.113.9", PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "too_many_requests");

    let (status, _) = sign_in(&app, "203.0.113.10", PASSWORD).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "other clients still sign in"
    );
}

#[tokio::test]
async fn starting_logins_is_limited_per_client() {
    let app = app();
    for _ in 0..10 {
        let (status, _) = generate(&app, PROXY, Some("203.0.113.9")).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = generate(&app, PROXY, Some("203.0.113.9")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], 1429);
    let (status, _) = generate(&app, PROXY, Some("203.0.113.10")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn registering_is_limited_per_client() {
    let app = app();
    let register = async |n: u32| {
        let body = json!({ "client_id": format!("client-{n}") });
        let request = Request::post("/api/v1/device/windows/init");
        send(&app, request, PROXY, Some("203.0.113.9"), Some(body)).await
    };
    for n in 0..10 {
        assert_eq!(register(n).await.0, StatusCode::OK);
    }
    let (status, body) = register(10).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], 1429);
}
