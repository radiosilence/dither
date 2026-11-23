//! The JSON the uploader speaks. It is also what an app would: create an
//! album, declare a batch of files, send each in chunks, finish each.
//!
//! ```text
//! POST   /api/albums                 {title?}            → {id}
//! POST   /api/albums/{id}/uploads    {files: [{name, size, type?}]} → {uploads: [{id}], chunk}
//! PUT    /api/uploads/{id}           Content-Range: bytes a-b/size, the bytes → {received}
//! GET    /api/uploads/{id}                               → {received, size, image?, error?}
//! POST   /api/uploads/{id}/finish                        → {image: {..., processing}} | {error}
//! ```
//!
//! A file's `type` is the MIME type its browser gives: a video's is held to
//! the video limit rather than the image one. The bytes decide what it is
//! when it finishes, and a video finishes as processing: its copies follow.
//!
//! ```text
//! DELETE /api/uploads/{id}
//! ```

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::json;

use crate::auth::session::Session;
use crate::db::Upload;
use crate::error::{AppError, AppResult};
use crate::upload::{CHUNK, ChunkError, Outcome};
use crate::web::AppState;

/// More files than this in one batch is refused: a client can send several.
const BATCH_MAX: usize = 1000;
const NAME_MAX: usize = 200;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

#[derive(serde::Deserialize, Default)]
pub struct NewAlbum {
    #[serde(default)]
    title: String,
}

pub async fn create_album(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    body: Option<Json<NewAlbum>>,
) -> AppResult<Json<serde_json::Value>> {
    let title: String = body
        .map(|b| b.0.title)
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    let album = state.db.create_album(&session.sub, &title).await?;
    Ok(Json(json!({ "id": album.id })))
}

#[derive(serde::Deserialize)]
pub struct File {
    name: String,
    size: u64,
    #[serde(default, rename = "type")]
    mime: String,
}

/// The video types dither takes, as browsers name them: the containers
/// `sanitize::sniff_video` knows.
pub const VIDEO_TYPES: [&str; 7] = [
    "video/mp4",
    "video/quicktime",
    "video/x-m4v",
    "video/3gpp",
    "video/3gpp2",
    "video/webm",
    "video/x-matroska",
];

impl File {
    /// Whether the browser says it is a video dither takes, or its name does
    /// when the browser says nothing.
    fn video(&self) -> bool {
        let ext = self
            .name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase());
        VIDEO_TYPES.contains(&self.mime.as_str())
            || (self.mime.is_empty()
                && matches!(
                    ext.as_deref(),
                    Some("mp4" | "mov" | "m4v" | "3gp" | "webm" | "mkv")
                ))
    }
}

#[derive(serde::Deserialize)]
pub struct Batch {
    files: Vec<File>,
}

pub async fn begin(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(album): Path<String>,
    Json(batch): Json<Batch>,
) -> Response {
    if batch.files.is_empty() || batch.files.len() > BATCH_MAX {
        return error(
            StatusCode::BAD_REQUEST,
            format!("send between 1 and {BATCH_MAX} files at a time"),
        );
    }
    // A video this server does not take is told so now, not after it has
    // all arrived.
    let videos = state.uploads.max_video_bytes();
    if let Some(f) = batch
        .files
        .iter()
        .find(|f| (f.mime.starts_with("video/") || f.video()) && (videos.is_none() || !f.video()))
    {
        let why = if videos.is_none() {
            format!("{}: this server doesn't take videos", f.name)
        } else {
            format!(
                "{}: MP4, MOV and WebM videos are taken, not {}",
                f.name, f.mime
            )
        };
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, why);
    }
    let max = |f: &File| match videos {
        Some(max) if f.video() => max,
        _ => state.config.max_upload_bytes,
    };
    if let Some(f) = batch.files.iter().find(|f| f.size == 0 || f.size > max(f)) {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "{} is {}; the limit is {}",
                f.name,
                crate::web::human(f.size),
                crate::web::human(max(f))
            ),
        );
    }
    let total: u64 = batch.files.iter().map(|f| f.size).sum();
    if !state.uploads.room_for(total) {
        return error(
            StatusCode::INSUFFICIENT_STORAGE,
            "the disk is nearly full, so nothing more can be stored",
        );
    }
    let files = batch
        .files
        .into_iter()
        .map(|f| (f.name.chars().take(NAME_MAX).collect::<String>(), f.size))
        .collect();
    match state.db.begin_uploads(&session.sub, &album, files).await {
        Ok(Some(uploads)) => Json(json!({
            "uploads": uploads.iter().map(|u| json!({ "id": u.id })).collect::<Vec<_>>(),
            "chunk": CHUNK,
        }))
        .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such album"),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// The upload, if it exists and is `session`'s.
async fn mine(state: &AppState, session: &Session, id: &str) -> AppResult<Upload> {
    if !crate::id::valid(id) {
        return Err(AppError::NotFound);
    }
    match state.db.upload(id).await? {
        Some(u) if u.owner == session.sub => Ok(u),
        _ => Err(AppError::NotFound),
    }
}

/// `bytes start-end/size`, inclusive, as a start and a length.
fn content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let v = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let (range, size) = v.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, size): (u64, u64, u64) =
        (start.parse().ok()?, end.parse().ok()?, size.parse().ok()?);
    (end >= start && end < size).then(|| (start, end - start + 1, size))
}

pub async fn chunk(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let upload = match mine(&state, &session, &id).await {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    if upload.image.is_some() || upload.error.is_some() {
        return error(StatusCode::GONE, "this upload has already ended");
    }
    let Some((start, len, size)) = content_range(&headers) else {
        return error(
            StatusCode::BAD_REQUEST,
            "a chunk needs a Content-Range of bytes a-b/size",
        );
    };
    if size != upload.size {
        return error(
            StatusCode::BAD_REQUEST,
            "the Content-Range's size is not the upload's",
        );
    }
    match state
        .uploads
        .append(&upload, start, len, body.into_data_stream())
        .await
    {
        Ok(received) => Json(json!({ "received": received })).into_response(),
        Err(ChunkError::Conflict(received)) => {
            (StatusCode::CONFLICT, Json(json!({ "received": received }))).into_response()
        }
        Err(ChunkError::TooLong) => error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the chunk is longer than it says",
        ),
        Err(ChunkError::Full) => error(
            StatusCode::INSUFFICIENT_STORAGE,
            "the disk is nearly full, so nothing more can be stored",
        ),
        // No `received`: a client must not read this as a place to resume.
        Err(ChunkError::Ended) => error(StatusCode::GONE, "this upload has already ended"),
        // Both are worth sending again, which a client knows from the status.
        Err(ChunkError::Broken(e)) => {
            tracing::info!(upload = %id, error = %e, "a chunk broke off");
            error(
                StatusCode::REQUEST_TIMEOUT,
                "the chunk did not arrive whole; send it again",
            )
        }
        Err(ChunkError::Io(e)) => {
            tracing::error!(upload = %id, error = %e, "saving a chunk failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the chunk could not be saved; send it again",
            )
        }
    }
}

pub async fn status(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let u = mine(&state, &session, &id).await?;
    Ok(Json(json!({
        "received": u.received,
        "size": u.size,
        "image": u.image,
        "error": u.error,
    })))
}

pub async fn finish(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> Response {
    let upload = match mine(&state, &session, &id).await {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    if upload.image.is_none() && upload.error.is_none() && upload.received < upload.size {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "received": upload.received, "error": "the upload is incomplete" })),
        )
            .into_response();
    }
    match state.uploads.finish(upload).await {
        Outcome::Done(image) => match state.db.image(&image).await {
            Ok(Some((i, _))) => {
                let public = &state.config.public_url;
                Json(json!({ "image": {
                    "id": i.id,
                    "page": format!("{public}/{}", i.id),
                    "file": format!("{public}/{}.{}", i.id, i.kind.ext()),
                    "thumb": format!("/t/{}.webp", i.id),
                    "width": i.width,
                    "height": i.height,
                    "bytes": i.bytes,
                    "processing": !i.ready(),
                }}))
                .into_response()
            }
            // Deleted between finishing and answering.
            Ok(None) => error(StatusCode::GONE, "the image was deleted"),
            Err(e) => AppError::from(e).into_response(),
        },
        Outcome::Rejected(why) => error(StatusCode::UNPROCESSABLE_ENTITY, why),
        Outcome::Failed(why) => error(StatusCode::SERVICE_UNAVAILABLE, why),
    }
}

pub async fn cancel(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let upload = mine(&state, &session, &id).await?;
    if upload.image.is_none() {
        state.uploads.cancel(&upload).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(v: &str) -> Option<(u64, u64, u64)> {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_RANGE, v.parse().unwrap());
        content_range(&h)
    }

    #[test]
    fn content_ranges_parse_to_a_start_and_a_length() {
        assert_eq!(range("bytes 0-99/1000"), Some((0, 100, 1000)));
        assert_eq!(range("bytes 900-999/1000"), Some((900, 100, 1000)));
        assert_eq!(range("bytes 900-1000/1000"), None);
        assert_eq!(range("bytes 5-4/10"), None);
        assert_eq!(range("bytes */10"), None);
        assert_eq!(range("0-9/10"), None);
    }
}
