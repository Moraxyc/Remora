mod common;

use remora_proto::cloud::ApiError;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Info {
    user_id: String,
    nickname: String,
}

const PATH: &str = "/api/v1/user/info";

#[tokio::test]
async fn user_info_needs_a_valid_token() {
    let app = remora_cloud::router(common::config());
    let token = common::log_in(&app).await;

    let info: Info = common::get(&app, PATH, Some(&token)).await.unwrap();
    assert_eq!(info.user_id, "1");
    assert!(!info.nickname.is_empty());

    let expired = Err(ApiError::TokenExpired);
    assert_eq!(
        common::get::<Info>(&app, PATH, None).await.map(drop),
        expired
    );
    assert_eq!(
        common::get::<Info>(&app, PATH, Some("bogus"))
            .await
            .map(drop),
        expired
    );
}

#[tokio::test]
async fn logging_out_ends_only_that_session() {
    let app = remora_cloud::router(common::config());
    let token = common::log_in(&app).await;
    let other = common::log_in(&app).await;

    let logout: Result<serde_json::Value, _> =
        common::post_as(&app, "/api/v1/user/logout", &token, None, "").await;
    assert_eq!(logout, Ok(serde_json::json!({})));

    let info = |token| common::get::<Info>(&app, PATH, Some(token));
    assert_eq!(info(&token).await.map(drop), Err(ApiError::TokenExpired));
    assert!(info(&other).await.is_ok());
}
