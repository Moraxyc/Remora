mod common;

use axum::Router;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use remora_proto::control::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Gen {
    qrcode_jump_url: String,
    status_query_ticket: String,
}

#[derive(Debug, Deserialize)]
struct Status {
    status: i64,
}

#[derive(Debug, Deserialize)]
struct Login {
    token: String,
    user_id: String,
    nickname: Option<String>,
    email: String,
    member_expired_at: u64,
    is_verified_realname: bool,
    coin_balance: u64,
    family_name: String,
    given_name: String,
    is_new_registration: bool,
    member_type: Option<u8>,
    member_display_status: Option<u8>,
    member_additional_desc: Option<String>,
    member_trial_status: Option<u8>,
}

async fn generate(app: &Router) -> Gen {
    common::post(app, "/api/v1/qrcode/gen/login", None)
        .await
        .unwrap()
}

async fn status(app: &Router, ticket: &str, url: &str) -> Result<Status, ApiError> {
    let body = json!({ "token": ticket, "qrcode_jump_url": url, "login_status": 1 });
    common::post(app, "/api/v1/qrcode/login/status", Some(&body.to_string())).await
}

async fn login(app: &Router, ticket: &str, url: &str) -> Result<Login, ApiError> {
    let body = json!({ "token": ticket, "qrcode_jump_url": url });
    common::post(app, "/api/v1/login/by_qrcode", Some(&body.to_string())).await
}

const QR_EXPIRED: ApiError = ApiError::QrCodeExpired;

#[tokio::test]
async fn qr_login_is_confirmed_and_yields_a_token_with_csrf() {
    let app = remora_cloud::router(common::config());
    let qr = generate(&app).await;
    assert_ne!(qr.qrcode_jump_url, qr.status_query_ticket);

    let polled = status(&app, &qr.status_query_ticket, &qr.qrcode_jump_url).await;
    assert_eq!(polled.unwrap().status, 4);

    let login = login(&app, &qr.status_query_ticket, &qr.qrcode_jump_url)
        .await
        .unwrap();
    assert!(!login.user_id.is_empty());
    assert!(login.nickname.is_some_and(|name| !name.is_empty()));
    assert_eq!(login.email, "");
    assert_eq!(login.member_expired_at, 0);
    assert!(!login.is_verified_realname);
    assert_eq!(login.coin_balance, 0);
    assert_eq!(login.family_name, "");
    assert_eq!(login.given_name, "");
    assert!(!login.is_new_registration);
    assert_eq!(login.member_type, Some(0));
    assert_eq!(login.member_display_status, Some(0));
    assert_eq!(login.member_additional_desc.as_deref(), Some(""));
    assert_eq!(login.member_trial_status, Some(0));

    // Clients take `csrf` from the JWT payload and refuse tokens without it.
    let payload = login.token.split('.').nth(1).unwrap();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    assert!(claims["csrf"].as_str().is_some_and(|csrf| !csrf.is_empty()));
}

#[tokio::test]
async fn a_qr_login_is_used_once() {
    let app = remora_cloud::router(common::config());
    let qr = generate(&app).await;
    let (ticket, url) = (&qr.status_query_ticket, &qr.qrcode_jump_url);

    login(&app, ticket, url).await.unwrap();
    assert_eq!(login(&app, ticket, url).await.map(drop), Err(QR_EXPIRED));
    assert_eq!(status(&app, ticket, url).await.map(drop), Err(QR_EXPIRED));
}

#[tokio::test]
async fn rejects_unknown_or_mismatched_qr_logins() {
    let app = remora_cloud::router(common::config());
    let qr = generate(&app).await;
    let other = generate(&app).await;
    let (ticket, url) = (&qr.status_query_ticket, &qr.qrcode_jump_url);

    assert_eq!(
        status(&app, "unknown", url).await.map(drop),
        Err(QR_EXPIRED)
    );
    let crossed = status(&app, ticket, &other.qrcode_jump_url).await;
    assert_eq!(crossed.map(drop), Err(QR_EXPIRED));
    let crossed = login(&app, ticket, &other.qrcode_jump_url).await;
    assert_eq!(crossed.map(drop), Err(QR_EXPIRED));

    // A mismatched attempt does not burn the real login.
    assert!(login(&app, ticket, url).await.is_ok());
}
