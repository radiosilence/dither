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
//! POST /work/{id}/files/{variant}/url                 → {url} to put it in the store | 404 when it does not sign
//! POST /work/{id}/files/{variant}/done {bytes, sha256} → 204 once what was put there is placed
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
    /// The longest copy a worker may send: what a conversion could make of
    /// the largest upload taken.
    pub limit: usize,
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
            put(file).layer(DefaultBodyLimit::max(work.limit)),
        )
        .route("/work/{id}/files/{variant}/url", post(put_url))
        .route("/work/{id}/files/{variant}/done", post(put_done))
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
/// short carries on where it stopped. A worker given a signed URL fetches it
/// from the store instead.
async fn source(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !crate::id::valid(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match work.hub.renew(&worker, &id, None).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::GONE.into_response(),
        Err(e) => return failed(e),
    }
    let from = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.strip_suffix('-'))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let read = work
        .hub
        .store()
        .get_range(
            &crate::jobs::upload(&id),
            object_store::GetRange::Offset(from),
        )
        .await;
    match read {
        Ok(Some(crate::store::Part::Within { body, range, total })) => {
            let body = match body {
                crate::store::Body::Bytes(bytes) => Body::from(bytes),
                crate::store::Body::Stream { stream, .. } => Body::from_stream(stream),
            };
            let mut response = (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_LENGTH,
                        (range.end - range.start).to_string(),
                    ),
                    (header::CONTENT_TYPE, "application/octet-stream".into()),
                ],
                body,
            )
                .into_response();
            if from > 0 {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                if let Ok(value) =
                    format!("bytes {}-{}/{total}", range.start, range.end - 1).parse()
                {
                    response.headers_mut().insert(header::CONTENT_RANGE, value);
                }
            }
            response
        }
        Ok(Some(crate::store::Part::Outside { .. })) => {
            StatusCode::RANGE_NOT_SATISFIABLE.into_response()
        }
        // The job is the worker's, but its upload is gone: 404, which the
        // worker takes as the job's end rather than as losing it.
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => failed(e),
    }
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

async fn put_url(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Path((id, variant)): Path<(String, String)>,
) -> Response {
    let (true, Some(variant)) = (crate::id::valid(&id), Variant::named(&variant)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match work.hub.put_url(&worker, &id, variant).await {
        Ok(Ok(Some(url))) => Json(serde_json::json!({ "url": url })).into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(())) => StatusCode::GONE.into_response(),
        Err(e) => failed(e),
    }
}

#[derive(serde::Deserialize)]
struct Done {
    bytes: u64,
    /// Hex.
    sha256: String,
}

async fn put_done(
    State(work): State<Work>,
    Extension(Named(worker)): Extension<Named>,
    Path((id, variant)): Path<(String, String)>,
    Json(done): Json<Done>,
) -> Response {
    let (true, Some(variant)) = (crate::id::valid(&id), Variant::named(&variant)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(sha256) = hex(&done.sha256) else {
        return (StatusCode::BAD_REQUEST, "sha256 is 64 hex digits").into_response();
    };
    match work
        .hub
        .put_done(&worker, &id, variant, done.bytes, sha256)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::GONE.into_response(),
        Err(e) => {
            tracing::warn!(job = %id, error = %e, "a copy a worker put did not arrive whole");
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
    }
}

fn hex(text: &str) -> Option<[u8; 32]> {
    let text = text.as_bytes();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in text.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        out[i] = u8::try_from(digit(pair[0])? * 16 + digit(pair[1])?).ok()?;
    }
    Some(out)
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
