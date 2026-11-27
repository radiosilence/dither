//! The files: originals and their copies, served with their type, `nosniff`
//! and a sandboxing policy, so no browser treats one as a document, and with
//! permission to be embedded anywhere, which is the point of an image host.
//! Their bytes never change under an address, so they are cached for a year.
//! Every file is served with byte ranges, which seeking a video needs and
//! Safari requires before it plays one at all.

use axum::body::Body as HttpBody;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use super::pages::{Missing, page};
use crate::db::Album;
use crate::metrics::{self, Served};
use crate::store::{self, Variant};
use crate::web::client::on_public_host;
use askama::Template;

use crate::web::{AppState, paths};

/// Image files: nothing at all, in a sandbox, should a browser ever render
/// one as a document. This and the video file's policy are written out rather
/// than built as a `Csp`, which starts from a page's policy and has no
/// directive without sources, as `sandbox` is.
const CSP_FILE: &str = "default-src 'none'; sandbox";
/// A video file. A browser opening one on its own plays it in a document of
/// its making, which this governs: it loads the video from here and styles
/// itself inline. Not sandboxed, since a sandboxed document has no origin of
/// its own for `'self'` to match, and Safari's then never loads the video; a
/// file served as video with `nosniff` is never read as a page regardless.
const CSP_VIDEO_FILE: &str = "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'";
/// A range from the start ending before this byte is a probe of the file
/// rather than a look at it.
const PROBE_MAX: u64 = 16;

/// `response`, marked as serving `album` (and `image`) for the counting
/// layer, and as a view or not. A private album is only ever its owner's,
/// whom nothing counts.
pub(super) fn counted(
    mut response: Response,
    album: &Album,
    image: Option<&str>,
    kind: Served,
    view: bool,
) -> Response {
    if album.private {
        return response;
    }
    response.extensions_mut().insert(crate::views::Counted {
        album: album.id.clone(),
        image: image.map(str::to_string),
        owner: album.owner.clone(),
        kind,
        view,
    });
    response
}

/// A 404 that nothing keeps. What is missing to one person can be there for
/// another (a private album's owner) or a moment later (an upload, whose
/// link is handed out before it is ready), and a shared cache keys neither.
pub async fn not_found() -> Response {
    let html = Missing.render().unwrap_or_default();
    let mut response = page(html, false, false);
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub async fn robots() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nAllow: /\n",
    )
        .into_response()
}

/// Whether the person asking may see `album`. A private album is its
/// owner's alone, and never on the public domain, which carries no session.
pub(super) async fn visible(state: &AppState, headers: &HeaderMap, album: &Album) -> bool {
    if !album.private {
        return true;
    }
    if on_public_host(&state.config, headers) {
        return false;
    }
    crate::auth::extract::current(state, headers)
        .await
        .is_some_and(|s| s.username == album.owner)
}

/// An original, `/<id>.<ext>`.
pub(super) async fn original(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    ext: &str,
) -> Response {
    file(state, headers, id, Variant::Original, ext).await
}

pub async fn thumb(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    match file.strip_suffix(".webp") {
        Some(id) => self::file(&state, &headers, id, Variant::Thumb, "webp").await,
        None => not_found().await,
    }
}

pub async fn display(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    match file.rsplit_once('.') {
        Some((id, ext @ ("webp" | "mp4"))) => {
            self::file(&state, &headers, id, Variant::Display, ext).await
        }
        _ => not_found().await,
    }
}

pub async fn preview(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    match file.strip_suffix(".jpg") {
        Some(id) => self::file(&state, &headers, id, Variant::Preview, "jpg").await,
        None => not_found().await,
    }
}

async fn file(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    variant: Variant,
    ext: &str,
) -> Response {
    if !crate::id::valid(id) {
        return not_found().await;
    }
    let Ok(Some((image, album))) = state.db.image(id).await else {
        return not_found().await;
    };
    if !visible(state, headers, &album).await {
        return not_found().await;
    }
    if !image.ready() {
        return not_found().await;
    }
    // A video's display copy is what plays, an image's a smaller picture.
    let display = if image.is_video() { "mp4" } else { "webp" };
    if variant == Variant::Display && ext != display {
        return not_found().await;
    }
    // A picture without a display copy is its own.
    let variant = if variant == Variant::Display && !image.display {
        Variant::Original
    } else {
        variant
    };
    let (mime, served, tag) = match variant {
        Variant::Original => {
            // `/abc.png` for a JPEG is a typo away from right: send it there.
            if !image.kind.matches_ext(ext) {
                let to = paths::original(id, image.kind.ext());
                return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)]).into_response();
            }
            (image.kind.mime(), Served::Original, 'o')
        }
        Variant::Thumb => ("image/webp", Served::Thumb, 't'),
        Variant::Display if image.is_video() => ("video/mp4", Served::Display, 'd'),
        Variant::Display => ("image/webp", Served::Display, 'd'),
        Variant::Preview => ("image/jpeg", Served::Preview, 'p'),
    };
    let etag = image.etag(tag);
    // What someone looks at on its own: the original, or a video's display
    // copy, which the viewer's HTML embeds. Thumbnails and previews are only
    // ever parts of pages and previews.
    let viewable = matches!(served, Served::Original | Served::Display);
    // A shared cache (a CDN in front) keeps a file ten minutes, so making its
    // album private or deleting it stops it being served within that. A
    // browser keeps it for good: whoever has it has seen it.
    let cache = if album.private {
        "private, no-store"
    } else {
        "public, max-age=31536000, s-maxage=600, immutable"
    };
    let modified = httpdate::fmt_http_date(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(image.created_at as u64),
    );
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&modified).expect("an HTTP date"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if mime.starts_with("video/") {
            CSP_VIDEO_FILE
        } else {
            CSP_FILE
        }),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("cross-origin"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
    {
        metrics::not_modified();
        // A revalidation: neither a view nor anything sent.
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    let filename = format!(
        "{id}.{}",
        match variant {
            Variant::Original => image.kind.ext(),
            Variant::Preview => "jpg",
            Variant::Thumb => "webp",
            Variant::Display => display,
        }
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename=\"{filename}\""))
            .expect("an id and an extension"),
    );
    let key = store::key(variant, id);
    if let Some(wanted) = Wanted::from(headers, &etag, &modified) {
        let response = partial(state, &key, wanted, served, h).await;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return response;
        }
        // A player's later ranges are the same view, and so is the probe
        // Safari sends before it plays (`bytes=0-1`).
        let view = viewable
            && match wanted {
                Wanted::Between(0, end) => end >= PROBE_MAX,
                Wanted::From(0) => true,
                _ => false,
            };
        return counted(response, &album, Some(id), served, view);
    }
    let body = match state.store.get(&key).await {
        Ok(Some(body)) => body,
        Ok(None) => {
            tracing::error!(
                image = id,
                ?variant,
                "an image's file is missing from the store"
            );
            return not_found().await;
        }
        Err(e) => {
            tracing::error!(image = id, ?variant, error = %e, "reading or making an image's file failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    match body {
        store::Body::Bytes(bytes) => {
            metrics::served(served, bytes.len() as u64);
            counted(
                (h, bytes).into_response(),
                &album,
                Some(id),
                served,
                viewable,
            )
        }
        store::Body::Stream { size, stream } => {
            metrics::served(served, size);
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
            let stream = stream.map(|r| r.map_err(std::io::Error::other));
            let response = (h, HttpBody::from_stream(stream)).into_response();
            counted(response, &album, Some(id), served, viewable)
        }
    }
}

/// The one range of bytes a request asks for. Several ranges in one request
/// are answered with the whole file, which the standard allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// `bytes=a-b`, inclusive.
    Between(u64, u64),
    /// `bytes=a-`.
    From(u64),
    /// `bytes=-n`: the last `n`.
    Last(u64),
}

impl Wanted {
    /// The range `headers` ask for, unless an `If-Range` names something
    /// other than this file as it is (`etag`, or its `modified` date).
    fn from(headers: &HeaderMap, etag: &str, modified: &str) -> Option<Self> {
        if let Some(condition) = headers.get(header::IF_RANGE) {
            let condition = condition.to_str().ok()?.trim();
            if condition != etag && condition != modified {
                return None;
            }
        }
        Self::parse(headers.get(header::RANGE)?.to_str().ok()?)
    }

    fn parse(value: &str) -> Option<Self> {
        let spec = value.trim().strip_prefix("bytes=")?.trim();
        if spec.contains(',') {
            return None;
        }
        let (a, b) = spec.split_once('-')?;
        let (a, b) = (a.trim(), b.trim());
        match (a.is_empty(), b.is_empty()) {
            (false, false) => {
                let (a, b) = (a.parse().ok()?, b.parse().ok()?);
                (a <= b).then_some(Wanted::Between(a, b))
            }
            (false, true) => Some(Wanted::From(a.parse().ok()?)),
            (true, false) => Some(Wanted::Last(b.parse().ok()?)),
            (true, true) => None,
        }
    }

    fn range(self) -> object_store::GetRange {
        use object_store::GetRange;
        match self {
            Wanted::Between(a, b) => GetRange::Bounded(a..b.saturating_add(1)),
            Wanted::From(a) => GetRange::Offset(a),
            Wanted::Last(n) => GetRange::Suffix(n),
        }
    }
}

/// Part of a file, as a range request asked: 206 with the part, or 416 when
/// the range is outside the file.
async fn partial(
    state: &AppState,
    key: &object_store::path::Path,
    wanted: Wanted,
    served: Served,
    mut h: HeaderMap,
) -> Response {
    let (body, range, total) = match state.store.get_range(key, wanted.range()).await {
        Ok(Some(store::Part::Within { body, range, total })) if range.start < range.end => {
            (body, range, total)
        }
        // Some stores answer an empty range rather than refusing it.
        Ok(Some(store::Part::Within { total, .. } | store::Part::Outside { total })) => {
            h.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).expect("digits"),
            );
            return (StatusCode::RANGE_NOT_SATISFIABLE, h).into_response();
        }
        Ok(None) => {
            tracing::error!(%key, "an image's file is missing from the store");
            return not_found().await;
        }
        Err(e) => {
            tracing::error!(%key, error = %e, "reading the store failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let len = range.end - range.start;
    h.insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {}-{}/{total}", range.start, range.end - 1))
            .expect("digits"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    metrics::served(served, len);
    let body = match body {
        store::Body::Bytes(bytes) => HttpBody::from(bytes),
        store::Body::Stream { stream, .. } => {
            HttpBody::from_stream(stream.map(|r| r.map_err(std::io::Error::other)))
        }
    };
    (StatusCode::PARTIAL_CONTENT, h, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_and_fall_within_the_file() {
        assert_eq!(Wanted::parse("bytes=0-99"), Some(Wanted::Between(0, 99)));
        assert_eq!(Wanted::parse("bytes=100-"), Some(Wanted::From(100)));
        assert_eq!(Wanted::parse("bytes=-500"), Some(Wanted::Last(500)));
        assert_eq!(Wanted::parse("bytes=0-1,5-6"), None);
        assert_eq!(Wanted::parse("bytes=9-3"), None);
        assert_eq!(Wanted::parse("items=0-1"), None);
        assert_eq!(Wanted::parse("bytes=-"), None);
        assert_eq!(
            Wanted::Between(0, 99).range(),
            object_store::GetRange::Bounded(0..100)
        );
    }
}
