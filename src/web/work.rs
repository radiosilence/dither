//! The worker API: what workers anywhere ask of the queue (`jobs.rs`). It is
//! served on a port of its own (`WORKER_ADDR`), never through the ingress,
//! and with a token (`WORKER_TOKEN`) whenever that port is reachable from
//! beyond the machine. Each request names its worker in `x-worker`.
//!
//! ```text
//! POST /work/claim                 {pictures, videos, makes_videos} → Job | 204 after a wait
//! POST /work/{id}/renew            {done?}            → 204 | 410 when it is no longer the worker's
//! GET  /work/{id}/source           Range: bytes=n-    → the upload, from n | 410 | 404 when it is lost
//! PUT  /work/{id}/files/{variant}  the bytes          → 204 | 410
//! POST /work/{id}/end              {outcome, ms}      → 204
//! POST /work/release               (stopping)         → 204
//! ```

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json};

use crate::jobs::{Hub, Outcome, POLL, Want};
use crate::store::Variant;

#[derive(Clone)]
pub struct Work {
    pub hub: Arc<Hub>,
    /// SHA-256 of the token workers present, compared as such so the
    /// comparison takes as long whatever it is given.
    pub token: Option<[u8; 32]>,
}

pub fn digest(token: &str) -> [u8; 32] {
    <sha2::Sha256 as sha2::Digest>::digest(token.as_bytes()).into()
}

pub fn router(work: Work) -> Router {
    Router::new()
        .route("/work/claim", post(claim))
        .route("/work/release", post(release))
        .route("/work/{id}/renew", post(renew))
        .route("/work/{id}/source", get(source))
        .route(
            "/work/{id}/files/{variant}",
            put(file).layer(DefaultBodyLimit::disable()),
        )
        .route("/work/{id}/end", post(end))
        .layer(middleware::from_fn_with_state(work.clone(), allowed))
        .with_state(work)
}

/// The worker a request names: its token checked, and its name one an id
/// could be, since it is written to the database and logged.
#[derive(Clone)]
struct Named(String);

async fn allowed(State(work): State<Work>, mut request: Request, next: Next) -> Response {
    let headers = request.headers();
    if let Some(want) = work.token {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(digest);
        if given != Some(want) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let Some(name) = headers
        .get("x-worker")
        .and_then(|v| v.to_str().ok())
        .filter(|n| crate::id::valid(n))
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "x-worker names no worker").into_response();
    };
    request.extensions_mut().insert(Named(name));
    next.run(request).await
}

fn failed(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "a worker's request failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

async fn claim(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Json(want): Json<Want>,
) -> Response {
    match work.hub.claim(&worker, want, POLL).await {
        Ok(Some(job)) => Json(job).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => failed(e),
    }
}

#[derive(serde::Deserialize)]
struct Renew {
    done: Option<u16>,
}

async fn renew(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Path(id): Path<String>,
    Json(body): Json<Renew>,
) -> Response {
    if !crate::id::valid(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let done = body.done.map(|d| d.min(1000));
    match work.hub.renew(&worker, &id, done).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::GONE.into_response(),
        Err(e) => failed(e),
    }
}

/// The upload, from the offset a `Range: bytes=n-` asks for, so a fetch cut
/// short carries on where it stopped.
async fn source(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    use tokio::io::AsyncSeekExt as _;
    if !crate::id::valid(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match work.hub.renew(&worker, &id, None).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::GONE.into_response(),
        Err(e) => return failed(e),
    }
    let path = crate::jobs::source(work.hub.state_dir(), &id);
    // The job is the worker's, but its upload is gone: 404, which the
    // worker takes as the job's end rather than as losing it.
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => return failed(e),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return failed(e),
    };
    let from = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.strip_suffix('-'))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if from > len {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    }
    if let Err(e) = file.seek(std::io::SeekFrom::Start(from)).await {
        return failed(e);
    }
    let stream = futures_util::stream::unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt as _;
        let mut buf = vec![0u8; 1 << 20];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), file))
            }
            Err(e) => Some((Err(e), file)),
        }
    });
    (
        if from > 0 {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        },
        [
            (header::CONTENT_LENGTH, (len - from).to_string()),
            (header::CONTENT_TYPE, "application/octet-stream".into()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn file(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Path((id, variant)): Path<(String, String)>,
    body: Body,
) -> Response {
    if !crate::id::valid(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(variant) = Variant::named(&variant) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match work
        .hub
        .put(&worker, &id, variant, body.into_data_stream())
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::GONE.into_response(),
        Err(e) => failed(e),
    }
}

#[derive(serde::Deserialize)]
struct End {
    #[serde(flatten)]
    outcome: Outcome,
    ms: u64,
}

async fn end(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Path(id): Path<String>,
    Json(body): Json<End>,
) -> Response {
    if !crate::id::valid(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match work.hub.end(&worker, &id, body.outcome, body.ms).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => failed(e),
    }
}

async fn release(State(work): State<Work>, Extension(Named(worker)): Extension<Named>) -> Response {
    match work.hub.release_all(&worker).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => failed(e),
    }
}
