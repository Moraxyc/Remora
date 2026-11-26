//! Images clients upload, and the wallpaper each device shows on its card.
//!
//! Clients ask `tool/fp/token` for an upload token and an upload URL, send the raw image there
//! with the token as `Authorization`, and get back the image's URL. Hosts then name that URL in
//! `device/wallpaper`, and viewers find it as `wallpaper_url` in the device list. Images are
//! served to anyone who has their URL, without credentials, as clients fetch them.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use md5::{Digest, Md5};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::device::{CallerDevice, Devices};
use crate::api::user::{Sessions, User};
use crate::api::{self, ApiError, Body, Empty};
use crate::contracts::{TokenKey, UPLOAD_TOKEN_TTL, Upload, UserToken, random};
use crate::{Database, PublicUrl};

/// Where clients upload files.
pub const UPLOAD_PATH: &str = "/api/files";

/// The largest file accepted. Clients skip larger images.
pub const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

/// How many of each uploader's latest files are kept, besides devices' wallpapers. Hosts may
/// name an earlier upload again when they switch back to its wallpaper.
const KEPT_UPLOADS: u32 = 4;

/// Sent with every served file, so that a browser never runs anything in it.
const FILE_CSP: &str = "default-src 'none'; sandbox";

/// The image formats accepted, by their leading bytes.
fn image_type(content: &[u8]) -> Option<&'static str> {
    if content.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if content.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if content.starts_with(b"RIFF") && content.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if content.starts_with(b"GIF87a") || content.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if content.starts_with(b"BM") && content.len() > 14 {
        Some("image/bmp")
    } else {
        None
    }
}

/// A stored file's id: random, so that its URL cannot be guessed.
#[derive(Clone, PartialEq, Eq)]
pub struct FileId(String);

impl FileId {
    fn generate() -> Self {
        Self(hex::encode(random::<16>()))
    }

    fn parse(id: &str) -> Option<Self> {
        let valid = id.len() == 32
            && id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        valid.then(|| Self(id.to_owned()))
    }

    /// The id in a file URL this server handed out, whatever origin it named then: a host may
    /// report a URL it cached before the server's public URL changed.
    fn from_url(url: &str) -> Option<Self> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let (_, path) = rest.split_once('/')?;
        let path = path.split(['?', '#']).next()?;
        Self::parse(path.strip_prefix(&UPLOAD_PATH[1..])?.strip_prefix('/')?)
    }

    /// Where it is served, relative to the server's origin.
    pub fn path(&self) -> String {
        format!("{UPLOAD_PATH}/{}", self.0)
    }

    pub fn url(&self, public_url: &PublicUrl) -> String {
        format!("{public_url}{}", self.path())
    }
}

/// Its URL is a credential, so it is never shown in logs.
impl fmt::Debug for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileId(..)")
    }
}

/// A file as stored.
struct File {
    mime: String,
    content: Vec<u8>,
}

/// Uploaded files and devices' wallpapers.
#[derive(Debug, Clone)]
pub struct Files(Database);

impl From<Database> for Files {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

impl Files {
    /// Stores a file of `user_id`, uploaded from `device_id`, and forgets that uploader's
    /// older files beyond the last [`KEPT_UPLOADS`], unless they are a device's wallpaper.
    fn store(
        &self,
        user_id: &str,
        device_id: Option<&str>,
        mime: &str,
        md5: &str,
        content: &[u8],
    ) -> rusqlite::Result<FileId> {
        let id = FileId::generate();
        let mut connection = self.0.connection();
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO files (id, user_id, device_id, mime, md5, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id.0, user_id, device_id, mime, md5, content],
        )?;
        transaction.execute(
            "DELETE FROM files
             WHERE user_id = ?1 AND device_id IS ?2
             AND id NOT IN (SELECT file_id FROM wallpapers)
             AND rowid NOT IN (
                 SELECT rowid FROM files WHERE user_id = ?1 AND device_id IS ?2
                 ORDER BY rowid DESC LIMIT ?3
             )",
            params![user_id, device_id, KEPT_UPLOADS],
        )?;
        transaction.commit()?;
        Ok(id)
    }

    fn get(&self, id: &FileId) -> rusqlite::Result<Option<File>> {
        self.0
            .connection()
            .query_row(
                "SELECT mime, content FROM files WHERE id = ?1",
                [&id.0],
                |row| {
                    Ok(File {
                        mime: row.get(0)?,
                        content: row.get(1)?,
                    })
                },
            )
            .optional()
    }

    /// Makes a file of `user_id` the wallpaper of `device_id`, or clears it with `None`;
    /// `false` if `user_id` has no such file.
    fn set_wallpaper(
        &self,
        device_id: &str,
        user_id: &str,
        file: Option<&FileId>,
    ) -> rusqlite::Result<bool> {
        let connection = self.0.connection();
        let Some(file) = file else {
            connection.execute("DELETE FROM wallpapers WHERE device_id = ?1", [device_id])?;
            return Ok(true);
        };
        let changed = connection.execute(
            "INSERT INTO wallpapers (device_id, file_id)
             SELECT ?1, id FROM files WHERE id = ?2 AND user_id = ?3
             ON CONFLICT (device_id) DO UPDATE SET file_id = excluded.file_id",
            [device_id, &file.0, user_id],
        )?;
        Ok(changed > 0)
    }

    /// The wallpapers of `user_id`'s devices, by device id.
    pub fn wallpapers_of_user(&self, user_id: &str) -> rusqlite::Result<HashMap<String, FileId>> {
        let connection = self.0.connection();
        let mut statement = connection.prepare_cached(
            "SELECT wallpapers.device_id, wallpapers.file_id
             FROM wallpapers JOIN devices ON devices.id = wallpapers.device_id
             WHERE devices.owner = ?1",
        )?;
        let rows = statement.query_map([user_id], |row| Ok((row.get(0)?, FileId(row.get(1)?))))?;
        rows.collect()
    }

    pub fn wallpaper_of(&self, device_id: &str) -> rusqlite::Result<Option<FileId>> {
        self.0
            .connection()
            .query_row(
                "SELECT file_id FROM wallpapers WHERE device_id = ?1",
                [device_id],
                |row| row.get(0).map(FileId),
            )
            .optional()
    }
}

/// What the file endpoints work with.
#[derive(Clone)]
pub struct FileService {
    pub files: Files,
    pub devices: Devices,
    pub sessions: Sessions,
    pub tokens: Arc<TokenKey>,
    pub public_url: PublicUrl,
}

#[derive(Serialize)]
pub struct TokenResponse {
    token: String,
    req_url: String,
    /// Seconds after which the client fetches a new token.
    ttl: u64,
    max_size: usize,
}

/// `GET /api/v1/tool/fp/token`, with any `filetype`: a token for uploading files, and where to.
pub async fn token(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(service): State<FileService>,
) -> Result<api::Ok<TokenResponse>, ApiError> {
    let device_id = match caller {
        Some(id) => service
            .devices
            .platform_of_owned(&id, &user.id)?
            .map(|_| id),
        None => None,
    };
    let token = service.tokens.issue_upload(Upload {
        sub: user.id,
        sid: user.session_id,
        device_id,
    });
    Ok(api::Ok(TokenResponse {
        token,
        req_url: format!("{}{UPLOAD_PATH}", service.public_url),
        ttl: UPLOAD_TOKEN_TTL.as_secs(),
        max_size: MAX_FILE_SIZE,
    }))
}

/// Why an upload failed, as `{"error": "<code>"}`. Clients only look for HTTP 200.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadError {
    Unauthorized,
    Empty,
    TooLarge,
    NotAnImage,
    Internal,
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Empty => (StatusCode::BAD_REQUEST, "empty"),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            Self::NotAnImage => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not_an_image"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        (status, Json(json!({ "error": code }))).into_response()
    }
}

impl From<rusqlite::Error> for UploadError {
    fn from(error: rusqlite::Error) -> Self {
        tracing::error!(%error, "database query failed");
        Self::Internal
    }
}

#[derive(Serialize)]
pub struct Uploaded {
    url: String,
    fsize: usize,
    md5: String,
    mime: &'static str,
}

/// `POST /api/files`: stores the image in the body, sent as it is or as the first file of a
/// `multipart/form-data` form, with the upload token as `Authorization`.
pub async fn upload(
    State(service): State<FileService>,
    request: Request,
) -> Result<Json<Uploaded>, UploadError> {
    let upload = upload_token(request.headers())
        .and_then(|token| service.tokens.verify_upload(token))
        .ok_or(UploadError::Unauthorized)?;
    let session = UserToken {
        user_id: upload.sub.clone(),
        session_id: upload.sid.clone(),
    };
    if !service.sessions.is_open(&session)? {
        return Err(UploadError::Unauthorized);
    }
    let content = read_file(request).await?;
    if content.is_empty() {
        return Err(UploadError::Empty);
    }
    if content.len() > MAX_FILE_SIZE {
        return Err(UploadError::TooLarge);
    }
    let mime = image_type(&content).ok_or(UploadError::NotAnImage)?;
    let md5 = hex::encode(Md5::digest(&content));
    let device_id = upload.device_id.as_deref();
    let id = service
        .files
        .store(&upload.sub, device_id, mime, &md5, &content)?;
    tracing::info!(user_id = %upload.sub, ?device_id, size = content.len(), mime, "file uploaded");
    Ok(Json(Uploaded {
        url: id.url(&service.public_url),
        fsize: content.len(),
        md5,
        mime,
    }))
}

/// The token in `Authorization`, which clients send bare, without a scheme.
fn upload_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    Some(value.strip_prefix("Bearer ").unwrap_or(value))
}

async fn read_file(request: Request) -> Result<Bytes, UploadError> {
    let multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("multipart/form-data"));
    if !multipart {
        return Bytes::from_request(request, &())
            .await
            .map_err(|_| UploadError::TooLarge);
    }
    let mut form = Multipart::from_request(request, &())
        .await
        .map_err(|_| UploadError::Empty)?;
    let mut first = None;
    while let Some(field) = form.next_field().await.map_err(|_| UploadError::Empty)? {
        let is_file = field.file_name().is_some();
        let content = field.bytes().await.map_err(|_| UploadError::TooLarge)?;
        if is_file {
            return Ok(content);
        }
        first.get_or_insert(content);
    }
    first.ok_or(UploadError::Empty)
}

/// `GET /api/files/{id}`: a stored file, to anyone who has its URL.
pub async fn serve(State(service): State<FileService>, Path(id): Path<String>) -> Response {
    let file = match FileId::parse(&id).map(|id| service.files.get(&id)) {
        Some(Ok(Some(file))) => file,
        Some(Err(error)) => {
            tracing::error!(%error, "database query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let content_type = HeaderValue::from_str(&file.mime)
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    (
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(FILE_CSP),
            ),
        ],
        file.content,
    )
        .into_response()
}

/// Body of `POST /api/v1/device/wallpaper`.
#[derive(Deserialize)]
pub struct WallpaperRequest {
    wallpaper_url: String,
}

/// `POST /api/v1/device/wallpaper`: the calling host's device shows one of the user's
/// uploaded images on its card; an empty URL clears it.
pub async fn set_wallpaper(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(service): State<FileService>,
    Body(request): Body<WallpaperRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    if service
        .devices
        .platform_of_owned(&device_id, &user.id)?
        .is_none()
    {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    let file = match request.wallpaper_url.as_str() {
        "" => None,
        url => Some(FileId::from_url(url).ok_or(ApiError::INVALID_REQUEST)?),
    };
    if !service
        .files
        .set_wallpaper(&device_id, &user.id, file.as_ref())?
    {
        return Err(ApiError::INVALID_REQUEST);
    }
    tracing::info!(user_id = %user.id, %device_id, cleared = file.is_none(), "wallpaper set");
    Ok(api::Ok(Empty {}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn file_urls_name_their_id_on_any_origin() {
        let id = FileId::parse(ID).unwrap();
        let public_url: PublicUrl = "https://remora.test".parse().unwrap();
        assert_eq!(
            id.url(&public_url),
            format!("https://remora.test/api/files/{ID}")
        );
        for url in [
            format!("https://remora.test/api/files/{ID}"),
            format!("http://old.example:8680/api/files/{ID}?fop=imageView/1/w/2/h/2/q/95"),
        ] {
            assert_eq!(FileId::from_url(&url), Some(id.clone()), "{url}");
        }
        for url in [
            format!("ftp://remora.test/api/files/{ID}"),
            format!("https://remora.test/api/files/{ID}/x"),
            format!("https://remora.test/files/{ID}"),
            "https://remora.test/api/files/../web".to_owned(),
            format!("https://remora.test/api/files/{}", ID.to_uppercase()),
        ] {
            assert_eq!(FileId::from_url(&url), None, "{url}");
        }
    }

    #[test]
    fn only_images_are_accepted() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_type(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(image_type(b"RIFF1234WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_type(b"BM0123456789abcdef"), Some("image/bmp"));
        assert_eq!(image_type(b"<svg onload=alert(1)>"), None);
        assert_eq!(image_type(b""), None);
    }

    #[test]
    fn debug_hides_file_ids() {
        assert_eq!(format!("{:?}", FileId::parse(ID).unwrap()), "FileId(..)");
    }

    fn uploads(files: &Files, user_id: &str, device_id: Option<&str>) -> Vec<FileId> {
        (0..KEPT_UPLOADS as usize + 2)
            .map(|_| {
                files
                    .store(user_id, device_id, "image/png", "", b"\x89PNG\r\n\x1a\n")
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn each_uploader_keeps_its_latest_files_and_wallpapers() {
        let database = Database::in_memory();
        database
            .connection()
            .execute(
                "INSERT INTO devices (id, client_id, name, platform, owner)
                 VALUES ('d', 'c', '', 1, '1')",
                [],
            )
            .unwrap();
        let files = Files::from(database);
        let first = files
            .store("1", Some("d"), "image/png", "", b"\x89PNG\r\n\x1a\n")
            .unwrap();
        assert!(files.set_wallpaper("d", "1", Some(&first)).unwrap());
        let mine = uploads(&files, "1", Some("d"));
        let other = uploads(&files, "1", None);

        assert!(files.get(&first).unwrap().is_some(), "the wallpaper stays");
        for (index, id) in mine.iter().enumerate() {
            let kept = index >= mine.len() - KEPT_UPLOADS as usize;
            assert_eq!(files.get(id).unwrap().is_some(), kept, "upload {index}");
        }
        assert!(files.get(other.last().unwrap()).unwrap().is_some());
        assert!(files.get(&other[0]).unwrap().is_none());
    }

    #[test]
    fn wallpapers_are_the_users_own_files() {
        let files = Files::from(Database::in_memory());
        let theirs = files
            .store("2", None, "image/png", "", b"\x89PNG\r\n\x1a\n")
            .unwrap();
        assert!(!files.set_wallpaper("d", "1", Some(&theirs)).unwrap());
        assert_eq!(files.wallpaper_of("d").unwrap(), None);
        assert!(files.set_wallpaper("d", "2", Some(&theirs)).unwrap());
        assert_eq!(files.wallpaper_of("d").unwrap(), Some(theirs));
        assert!(files.set_wallpaper("d", "2", None).unwrap());
        assert_eq!(files.wallpaper_of("d").unwrap(), None);
    }
}
