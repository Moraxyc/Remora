//! Official client profile detection middleware and request extractor.

use std::convert::Infallible;

use axum::extract::{FromRequestParts, Request};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use remora_proto::profile::{Profile, detect_from_headers};

/// Detects an embedded [`Profile`] from request headers.
pub fn detect(headers: &HeaderMap) -> Option<&'static Profile> {
    detect_from_headers(
        headers
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
    )
}

/// Middleware that runs [`detect`] on incoming requests and inserts the matched
/// `&'static Profile` into request extensions.
pub async fn detect_profile(mut request: Request, next: Next) -> Response {
    if let Some(profile) = detect(request.headers()) {
        request.extensions_mut().insert(profile);
    }
    next.run(request).await
}

/// Extracts the detected [`Profile`] from request extensions, if headers matched one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "extractor for handlers reading detected client profile"
)]
pub struct ClientProfile(pub Option<&'static Profile>);

impl<S: Send + Sync> FromRequestParts<S> for ClientProfile {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Self(parts.extensions.get::<&'static Profile>().copied()))
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    async fn handler(ClientProfile(profile): ClientProfile, request: Request) -> String {
        let from_ext = request.extensions().get::<&'static Profile>().copied();
        assert_eq!(profile, from_ext);
        profile.map_or("none", |p| p.id.as_str()).to_owned()
    }

    #[tokio::test]
    async fn injects_detected_profile_into_request_extensions() {
        let app = Router::new()
            .route("/test", get(handler))
            .layer(from_fn(detect_profile));

        let matched = Request::get("/test")
            .header("X-Param-PLAT", "4")
            .header("X-Param-VN", "4.48.2")
            .header("X-Param-VC", "634")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(matched).await.unwrap();
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(&body[..], b"macos-4.48.2");

        let win_matched = Request::get("/test")
            .header("X-Param-PLAT", "1")
            .header("X-Param-VN", "4.48.2.5754")
            .header("X-Param-VC", "5754")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(win_matched).await.unwrap();
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(&body[..], b"windows-4.48.2");

        let unknown = Request::get("/test")
            .header("X-Param-PLAT", "1")
            .header("X-Param-VN", "4.99.0.0")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(unknown).await.unwrap();
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(&body[..], b"none");
    }
}
