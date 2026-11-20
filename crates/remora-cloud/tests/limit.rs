mod common;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use remora_cloud::{Config, OwnerPassword, PeerAddress, TrustedProxies, TrustedProxy};
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
async fn unix_sockets_trust_forwarded_for_by_default() {
    let app = remora_cloud::router(common::config());
    let request = Request::post("/api/v1/qrcode/gen/login")
        .extension(ConnectInfo(PeerAddress::Unix))
        .header("x-forwarded-for", "203.0.113.9");
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let generated: Value = serde_json::from_slice(&bytes).unwrap();
    let url = generated["data"]["qrcode_jump_url"].as_str().unwrap();
    let id = url.rsplit_once('/').unwrap().1;
    let response = app
        .oneshot(
            Request::get(format!("/api/web/logins/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let login: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(login["address"], "203.0.113.9");
}

#[tokio::test]
async fn unix_listener_serves_and_extracts_forwarded_for() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let socket_path = std::env::temp_dir().join(format!("remora-test-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let app =
        remora_cloud::router(common::config()).into_make_service_with_connect_info::<PeerAddress>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let send_raw = |request: String| {
        let socket_path = socket_path.clone();
        async move {
            let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let text = String::from_utf8(response).unwrap();
            let (_, body) = text.split_once("\r\n\r\n").unwrap();
            serde_json::from_str::<Value>(body).unwrap()
        }
    };

    let generated = send_raw(
        "POST /api/v1/qrcode/gen/login HTTP/1.1\r\n\
         Host: remora.example\r\n\
         X-Forwarded-For: 203.0.113.9\r\n\
         Connection: close\r\n\r\n"
            .to_owned(),
    )
    .await;
    let url = generated["data"]["qrcode_jump_url"].as_str().unwrap();
    let id = url.rsplit_once('/').unwrap().1;

    let login = send_raw(format!(
        "GET /api/web/logins/{id} HTTP/1.1\r\n\
         Host: remora.example\r\n\
         Connection: close\r\n\r\n"
    ))
    .await;
    assert_eq!(login["address"], "203.0.113.9");

    server.abort();
    let _ = std::fs::remove_file(&socket_path);
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
