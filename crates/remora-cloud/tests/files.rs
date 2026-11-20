mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use remora_proto::cloud::ApiError;
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "/api/v1/tool/fp/token?filetype=1";
const WALLPAPER: &str = "/api/v1/device/wallpaper";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic";

/// A host on the owner's account: registered, logged in, and bound by creating a room.
struct Host {
    device_id: String,
    token: String,
}

impl Host {
    async fn bind(app: &Router, client_id: &str) -> Self {
        let device_id = common::register(app, client_id).await;
        let token = common::log_in(app).await;
        common::create_room(app, &token, &device_id).await;
        Self { device_id, token }
    }

    async fn upload_token(&self, app: &Router) -> Value {
        common::get_as(app, TOKEN, &self.token, &self.device_id)
            .await
            .unwrap()
    }

    async fn set_wallpaper(&self, app: &Router, url: &str) -> Result<Value, ApiError> {
        let body = json!({ "wallpaper_url": url }).to_string();
        common::post_as(app, WALLPAPER, &self.token, Some(&self.device_id), &body).await
    }

    /// `wallpaper_url` of each bound device in the device list, by device id.
    async fn wallpapers(&self, app: &Router) -> Vec<(String, Option<String>)> {
        let list: Value = common::get_as(app, "/api/v1/device/list", &self.token, &self.device_id)
            .await
            .unwrap();
        list["my_binded_devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|device| {
                let url = device
                    .get("wallpaper_url")
                    .map(|url| url.as_str().unwrap().to_owned());
                (device["device_id"].as_str().unwrap().to_owned(), url)
            })
            .collect()
    }
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, header::HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

/// Uploads like uu-remote-linux: the raw bytes, with the bare token as `Authorization` and
/// no account credentials.
async fn upload(app: &Router, grant: &Value, content: &[u8]) -> (StatusCode, Value) {
    let path = grant["req_url"].as_str().unwrap();
    let path = path.strip_prefix(common::PUBLIC_URL).unwrap();
    let request = Request::post(path)
        .header(header::AUTHORIZATION, grant["token"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "image/png")
        .body(Body::from(content.to_vec()))
        .unwrap();
    let (status, _, body) = send(app, request).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn fetch(app: &Router, url: &str) -> (StatusCode, header::HeaderMap, Vec<u8>) {
    let path = url.strip_prefix(common::PUBLIC_URL).unwrap();
    send(app, Request::get(path).body(Body::empty()).unwrap()).await
}

#[tokio::test]
async fn a_host_uploads_its_wallpaper_and_viewers_see_it() {
    let app = remora_cloud::router(common::config());
    let host = Host::bind(&app, "client-host").await;

    let grant = host.upload_token(&app).await;
    assert_eq!(
        grant["req_url"],
        format!("{}/api/files", common::PUBLIC_URL)
    );
    assert_eq!(grant["ttl"], 3600);
    assert_eq!(grant["max_size"], 10 * 1024 * 1024);

    let (status, uploaded) = upload(&app, &grant, PNG).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(uploaded["fsize"], PNG.len());
    assert_eq!(uploaded["mime"], "image/png");
    assert_eq!(uploaded["md5"].as_str().unwrap().len(), 32);
    let url = uploaded["url"].as_str().unwrap();
    assert!(url.starts_with("https://remora.test/api/files/"), "{url}");

    // The same token keeps working until it expires.
    let (status, again) = upload(&app, &grant, PNG).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(again["url"], uploaded["url"]);

    assert_eq!(
        host.wallpapers(&app).await,
        [(host.device_id.clone(), None)]
    );
    assert_eq!(host.set_wallpaper(&app, url).await, Ok(json!({})));
    assert_eq!(
        host.wallpapers(&app).await,
        [(host.device_id.clone(), Some(url.to_owned()))]
    );

    // Viewers fetch it without credentials, resizing parameters and all.
    let (status, headers, content) = fetch(&app, &format!("{url}?fop=imageView/1/w/2/h/2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content, PNG);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

    assert_eq!(host.set_wallpaper(&app, "").await, Ok(json!({})));
    assert_eq!(
        host.wallpapers(&app).await,
        [(host.device_id.clone(), None)]
    );
}

#[tokio::test]
async fn forms_carry_the_file_too() {
    let app = remora_cloud::router(common::config());
    let host = Host::bind(&app, "client-host").await;
    let grant = host.upload_token(&app).await;
    let mut body = b"--b\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n\
        --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"w.png\"\r\n\
        Content-Type: image/png\r\n\r\n"
        .to_vec();
    body.extend_from_slice(PNG);
    body.extend_from_slice(b"\r\n--b--\r\n");
    let request = Request::post("/api/files")
        .header(header::AUTHORIZATION, grant["token"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
        .body(Body::from(body))
        .unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    let uploaded: Value = serde_json::from_slice(&body).unwrap();
    let (_, _, content) = fetch(&app, uploaded["url"].as_str().unwrap()).await;
    assert_eq!(content, PNG);
}

#[tokio::test]
async fn uploads_need_a_live_token_and_an_image() {
    let app = remora_cloud::router(common::config());
    let host = Host::bind(&app, "client-host").await;
    let grant = host.upload_token(&app).await;

    assert_eq!(upload(&app, &grant, b"").await.0, StatusCode::BAD_REQUEST);
    let script = b"<svg onload=alert(1)>";
    assert_eq!(
        upload(&app, &grant, script).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let too_large = [PNG, &vec![0; 10 * 1024 * 1024]].concat();
    assert_eq!(
        upload(&app, &grant, &too_large).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let forged = json!({ "req_url": grant["req_url"], "token": host.token });
    assert_eq!(upload(&app, &forged, PNG).await.0, StatusCode::UNAUTHORIZED);

    let logout: Result<Value, _> =
        common::post_as(&app, "/api/v1/user/logout", &host.token, None, "{}").await;
    logout.unwrap();
    assert_eq!(upload(&app, &grant, PNG).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wallpapers_are_files_this_server_stored() {
    let app = remora_cloud::router(common::config());
    let host = Host::bind(&app, "client-host").await;
    let invalid = Err(ApiError::Rejected {
        http_status: 422,
        code: 1001,
    });
    for url in [
        "https://cdn.example/w.png".to_owned(),
        format!(
            "{}/api/files/0123456789abcdef0123456789abcdef",
            common::PUBLIC_URL
        ),
    ] {
        assert_eq!(host.set_wallpaper(&app, &url).await, invalid, "{url}");
    }

    let unbound = common::register(&app, "client-unbound").await;
    let body = json!({ "wallpaper_url": "" }).to_string();
    let result: Result<Value, _> =
        common::post_as(&app, WALLPAPER, &host.token, Some(&unbound), &body).await;
    assert_eq!(
        result,
        Err(ApiError::Rejected {
            http_status: 400,
            code: 2003
        })
    );
}

#[tokio::test]
async fn unbinding_forgets_the_devices_files() {
    let app = remora_cloud::router(common::config());
    let host = Host::bind(&app, "client-host").await;
    let viewer = Host::bind(&app, "client-viewer").await;
    let grant = host.upload_token(&app).await;
    let (_, uploaded) = upload(&app, &grant, PNG).await;
    let url = uploaded["url"].as_str().unwrap();
    host.set_wallpaper(&app, url).await.unwrap();

    let path = format!("/api/v1/device/{}/unbind", host.device_id);
    let unbound: Result<Value, _> =
        common::post_as(&app, &path, &viewer.token, Some(&viewer.device_id), "").await;
    unbound.unwrap();
    assert_eq!(fetch(&app, url).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        viewer.wallpapers(&app).await,
        [(viewer.device_id.clone(), None)]
    );
}
