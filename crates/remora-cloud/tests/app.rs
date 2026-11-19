mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
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
