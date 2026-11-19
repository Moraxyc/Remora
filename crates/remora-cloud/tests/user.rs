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
    let app = remora_cloud::router(None);
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
