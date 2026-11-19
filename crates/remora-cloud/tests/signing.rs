mod common;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use remora_proto::cloud::{ApiError, SignKey, parse_response, sign};
use serde_json::Value;
use tower::ServiceExt;

const BODY: &str = r#"{"name":"host","client_id":"client-a"}"#;
const PATH: &str = "/api/v1/device/windows/init";

fn key() -> SignKey {
    SignKey::new("test-sign-key")
}

/// The headers a client sends besides the signature, in the client's own casing.
const HEADERS: [(&str, &str); 6] = [
    ("X-Param-Plat", "1"),
    ("x-param-vn", "4.45.0.4318"),
    ("x-param-client-id", "client-a"),
    ("x-param-ts", "1700000000"),
    ("Content-Type", "application/json"),
    ("User-Agent", "test"),
];

async fn send(
    app: &Router,
    path: &str,
    body: &str,
    signature: Option<&str>,
) -> Result<(), ApiError> {
    let mut request = Request::post(path);
    for (name, value) in HEADERS {
        request = request.header(name, value);
    }
    if let Some(signature) = signature {
        // Header names are case-insensitive; clients send this one in mixed case.
        request = request.header("X-Param-SIGN", signature);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    parse_response::<Value>(status, &bytes).map(drop)
}

fn signature(path: &str, body: &str) -> String {
    sign(&key(), "POST", path, HEADERS, body.as_bytes())
}

const INVALID_SIGNATURE: Result<(), ApiError> = Err(ApiError::Rejected {
    http_status: 403,
    code: 1002,
});

#[tokio::test]
async fn accepts_correctly_signed_requests() {
    let app = remora_cloud::router(remora_cloud::Config {
        sign_key: Some(key()),
        ..common::config()
    });
    let signed = signature(PATH, BODY);
    assert_eq!(send(&app, PATH, BODY, Some(&signed)).await, Ok(()));

    let with_query = format!("{PATH}?filetype=1");
    let signed = signature(&with_query, BODY);
    assert_eq!(send(&app, &with_query, BODY, Some(&signed)).await, Ok(()));
}

#[tokio::test]
async fn rejects_missing_or_wrong_signatures() {
    let app = remora_cloud::router(remora_cloud::Config {
        sign_key: Some(key()),
        ..common::config()
    });
    let signed = signature(PATH, BODY);
    let tampered = BODY.replace("client-a", "client-b");
    let other_key = sign(
        &SignKey::new("other"),
        "POST",
        PATH,
        HEADERS,
        BODY.as_bytes(),
    );

    assert_eq!(send(&app, PATH, BODY, None).await, INVALID_SIGNATURE);
    assert_eq!(
        send(&app, PATH, &tampered, Some(&signed)).await,
        INVALID_SIGNATURE
    );
    assert_eq!(
        send(&app, PATH, BODY, Some(&other_key)).await,
        INVALID_SIGNATURE
    );
    let wrong_path = format!("{PATH}?x=1");
    assert_eq!(
        send(&app, &wrong_path, BODY, Some(&signed)).await,
        INVALID_SIGNATURE
    );
}

#[tokio::test]
async fn without_a_key_signatures_are_not_checked() {
    let app = remora_cloud::router(common::config());
    assert_eq!(send(&app, PATH, BODY, None).await, Ok(()));
    assert_eq!(send(&app, PATH, BODY, Some("bogus")).await, Ok(()));
}
