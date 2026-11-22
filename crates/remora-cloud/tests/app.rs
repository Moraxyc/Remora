mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn fetch(app: &Router, method: Method, path: &str) -> (StatusCode, HeaderMap, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn every_app_route_gets_the_page() {
    let app = remora_cloud::router(common::config());
    let (_, _, page) = fetch(&app, Method::GET, "/").await;
    assert!(page.contains("_app/immutable/"), "the app's page");

    for path in ["/devices", "/some/deep/route"] {
        let (status, headers, body) = fetch(&app, Method::GET, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body, page, "{path}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/html");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        let policy = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(policy.contains("frame-ancestors 'none'"), "{policy}");
    }
}

#[tokio::test]
async fn hashed_files_are_cached_for_good() {
    let app = remora_cloud::router(common::config());
    let (_, _, page) = fetch(&app, Method::GET, "/").await;
    let (_, rest) = page.split_once("href=\"/_app/immutable/").unwrap();
    let path = format!("/_app/immutable/{}", rest.split('"').next().unwrap());

    let (status, headers, _) = fetch(&app, Method::GET, &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/javascript");
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
}

#[tokio::test]
async fn unknown_api_paths_are_not_the_apps() {
    let app = remora_cloud::router(common::config());
    for (method, path) in [
        (Method::GET, "/api/v1/unknown"),
        (Method::GET, "/api/web/unknown"),
        (Method::POST, "/devices"),
    ] {
        let (status, _, _) = fetch(&app, method, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn ios_compat_routes_require_auth_and_return_wire_envelopes() {
    let app = remora_cloud::router(common::config());
    let token = common::log_in(&app).await;
    for (method, path, status, code, msg, data) in [
        (
            Method::GET,
            "/api/v1/release/fetch/update/force",
            400,
            1151,
            "当前已是最新版本",
            json!({}),
        ),
        (
            Method::POST,
            "/api/v1/event/uni/report",
            200,
            0,
            "ok",
            json!({}),
        ),
        (
            Method::GET,
            "/api/v1/cloud_pc/disks",
            200,
            0,
            "ok",
            json!({ "disks": {}, "bind_map": {}, "bindable_map": {} }),
        ),
        (
            Method::POST,
            "/api/v1/room/app_flag",
            200,
            0,
            "ok",
            json!({}),
        ),
        (
            Method::GET,
            "/api/v1/keymapping/official/list_with_version",
            400,
            1182,
            "当前配置已是最新版本",
            json!({}),
        ),
        (
            Method::GET,
            "/api/v1/keymapping/list",
            200,
            0,
            "ok",
            json!([]),
        ),
    ] {
        let (unauthenticated, _, _) = fetch(&app, method.clone(), path).await;
        assert_eq!(unauthenticated, StatusCode::UNPROCESSABLE_ENTITY, "{path}");

        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), status, "{path}");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            json!({ "code": code, "msg": msg, "data": data }),
            "{path}"
        );
    }
}
