//! The web app, embedded in the binary. Its files are served as they are; every other path
//! outside the APIs gets the app's page, which routes in the browser.

use axum::http::header::{self, HeaderValue};
use axum::http::{HeaderName, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$REMORA_WEB_DIR"]
struct Build;

/// The page every route of the app starts from.
const PAGE: &str = "app.html";

/// Files whose names carry a hash of their content, so they never change.
const IMMUTABLE: &str = "_app/immutable/";

/// Paths that belong to the APIs: unknown ones are not the app's.
const API_PREFIXES: [&str; 2] = ["/api/", "/socket.io/"];

/// Sent with everything the app serves. The page carries its own CSP for scripts and styles,
/// with hashes of its inline bootstrap; this adds what only a header can say.
const SECURITY_HEADERS: [(HeaderName, &str); 4] = [
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::X_FRAME_OPTIONS, "DENY"),
    (header::REFERRER_POLICY, "no-referrer"),
    (
        header::CONTENT_SECURITY_POLICY,
        "frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'",
    ),
];

pub async fn serve(method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if !matches!(method, Method::GET | Method::HEAD)
        || API_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let name = path.trim_start_matches('/');
    let (file, cache) = match Build::get(name).filter(|_| !name.is_empty()) {
        Some(file) if name.starts_with(IMMUTABLE) => (file, "public, max-age=31536000, immutable"),
        Some(file) => (file, "no-cache"),
        None => (
            Build::get(PAGE).expect("the build has its page; build.rs checks"),
            "no-cache",
        ),
    };
    let content_type = HeaderValue::from_str(file.metadata.mimetype())
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let headers = SECURITY_HEADERS.map(|(name, value)| (name, HeaderValue::from_static(value)));
    (
        headers,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        file.data,
    )
        .into_response()
}
