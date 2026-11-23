//! The service over HTTP, as a browser, a crawler and the uploader see it.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use http_body_util::BodyExt;
use image::ImageEncoder;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{AppState, Inner};
use crate::auth::cookie::SESSION_COOKIE;
use crate::auth::session::Sessions;
use crate::config::Config;
use crate::db::Db;
use crate::store::{self, Store, Variant};
use crate::upload::Uploads;

struct App {
    router: Router,
    state: AppState,
    _dir: tempfile::TempDir,
}

/// The service as it runs: the web process, and a worker beside it over the
/// socket, sharing the state directory and the store.
async fn app_with(edit: impl Fn(&mut Config)) -> App {
    let dir = tempfile::tempdir().unwrap();
    let config_for = |dir: &std::path::Path| {
        let mut config = Config::test(dir.join("state"));
        edit(&mut config);
        config
    };
    let config = config_for(dir.path());
    std::fs::create_dir_all(&config.state_dir).unwrap();
    let store = Store::memory();
    let db = Db::open(&config.state_dir.join("dither.db")).await.unwrap();
    let jobs = Arc::new(crate::jobs::Hub::new(db.clone(), &config.state_dir).unwrap());
    jobs.listen().unwrap();
    let uploads = Arc::new(Uploads::new(db.clone(), store.clone(), &config, jobs).unwrap());
    {
        let (config, store) = (config_for(dir.path()), store.clone());
        tokio::spawn(async move {
            if let Err(e) = crate::worker::run(&config, store).await {
                tracing::warn!(error = %e, "the test's worker stopped");
            }
        });
    }
    let (stop, stopping) = tokio::sync::watch::channel(false);
    // Kept for the life of the test: a dropped sender reads as stopping.
    std::mem::forget(stop);
    let state = AppState::new(Inner {
        sessions: Sessions::new(db.clone()),
        http: reqwest::Client::new(),
        db,
        store,
        uploads,
        stopping,
        config,
    });
    App {
        router: super::router(state.clone()),
        state,
        _dir: dir,
    }
}

async fn app() -> App {
    app_with(|_| {}).await
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

impl App {
    async fn send(&self, req: Request<Body>) -> Reply {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(header::HOST, "app.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn get_as(&self, uri: &str, cookie: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(header::HOST, "app.test")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn write(&self, method: &str, uri: &str, body: Value, cookie: Option<&str>) -> Reply {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "app.test")
            .header("x-dither", "1")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, format!("{SESSION_COOKIE}={c}"));
        }
        self.send(req.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn post(&self, uri: &str, body: Value) -> Reply {
        self.write("POST", uri, body, None).await
    }

    async fn chunk(&self, upload: &str, bytes: &[u8], start: usize, size: usize) -> Reply {
        let end = start + bytes.len() - 1;
        self.send(
            Request::put(format!("/api/uploads/{upload}"))
                .header(header::HOST, "app.test")
                .header("x-dither", "1")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
    }

    /// A new album holding `files`, uploaded as the browser uploads them.
    /// Returns the album and each file's outcome.
    async fn upload(&self, files: &[&[u8]]) -> (String, Vec<Reply>) {
        let album = self
            .post("/api/albums", json!({ "title": "trip" }))
            .await
            .json()["id"]
            .as_str()
            .unwrap()
            .to_string();
        (album.clone(), self.upload_to(&album, files).await)
    }

    async fn upload_to(&self, album: &str, files: &[&[u8]]) -> Vec<Reply> {
        self.send_files(album, files, true).await
    }

    /// [`upload`](Self::upload), returning as soon as each file is queued.
    async fn upload_queued(&self, files: &[&[u8]]) -> (String, Vec<Reply>) {
        let album = self
            .post("/api/albums", json!({ "title": "trip" }))
            .await
            .json()["id"]
            .as_str()
            .unwrap()
            .to_string();
        (album.clone(), self.send_files(&album, files, false).await)
    }

    async fn send_files(&self, album: &str, files: &[&[u8]], wait: bool) -> Vec<Reply> {
        let batch = json!({ "files": files.iter().enumerate().map(|(n, f)| json!({ "name": format!("{n}.jpg"), "size": f.len() })).collect::<Vec<_>>() });
        let begun = self
            .post(&format!("/api/albums/{album}/uploads"), batch)
            .await;
        assert_eq!(begun.status, StatusCode::OK, "{}", begun.text());
        let mut out = Vec::new();
        for (file, upload) in files
            .iter()
            .zip(begun.json()["uploads"].as_array().unwrap())
        {
            let id = upload["id"].as_str().unwrap();
            for start in (0..file.len()).step_by(crate::upload::CHUNK as usize) {
                let end = (start + crate::upload::CHUNK as usize).min(file.len());
                let put = self.chunk(id, &file[start..end], start, file.len()).await;
                assert_eq!(put.status, StatusCode::OK, "{}", put.text());
            }
            let finished = self
                .post(&format!("/api/uploads/{id}/finish"), json!({}))
                .await;
            // Queued for the worker: most callers want what it made.
            if let Some(image) = finished.json()["image"]["id"].as_str()
                && wait
            {
                made(self, image).await;
            }
            out.push(finished);
        }
        out
    }

    async fn session(&self, sub: &str) -> String {
        self.state
            .sessions
            .create(sub, std::time::Duration::from_secs(60))
            .await
            .unwrap()
    }
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 241) as u8, 77])
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// A JPEG carrying a location in its EXIF, as a phone's does.
fn located_jpeg() -> Vec<u8> {
    let plain = jpeg(64, 48);
    let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0".to_vec();
    let text = b"GPS 51.5072N 0.1276W home\0";
    exif.extend_from_slice(&0x010Eu16.to_le_bytes());
    exif.extend_from_slice(&2u16.to_le_bytes());
    exif.extend_from_slice(&(text.len() as u32).to_le_bytes());
    exif.extend_from_slice(&26u32.to_le_bytes());
    exif.extend_from_slice(&0u32.to_le_bytes());
    exif.extend_from_slice(text);
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&plain[2..]);
    out
}

#[tokio::test]
async fn another_site_cannot_sign_anyone_out() {
    let app = app().await;
    let ann = app.session("ann").await;
    let from = |site: &str| {
        Request::get("/auth/logout")
            .header(header::HOST, "app.test")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={ann}"))
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap()
    };
    for elsewhere in ["cross-site", "same-site"] {
        app.send(from(elsewhere)).await;
        assert!(app.state.sessions.get(&ann).await.is_some(), "{elsewhere}");
    }
    app.send(from("same-origin")).await;
    assert!(app.state.sessions.get(&ann).await.is_none());
}

fn image_id(reply: &Reply) -> String {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()["image"]["id"].as_str().unwrap().to_string()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn an_upload_is_served_as_a_file_a_page_and_an_album() {
    let app = app().await;
    let (album, done) = app.upload(&[&jpeg(3000, 2000)]).await;
    let id = image_id(&done[0]);

    let file = app.get(&format!("/{id}.jpg")).await;
    assert_eq!(file.status, StatusCode::OK);
    assert_eq!(file.header("content-type"), "image/jpeg");
    assert_eq!(
        file.header("cache-control"),
        "public, max-age=31536000, s-maxage=600, immutable"
    );
    assert_eq!(file.header("x-content-type-options"), "nosniff");
    assert_eq!(file.header("cross-origin-resource-policy"), "cross-origin");
    assert!(file.header("content-security-policy").contains("sandbox"));
    assert!(image::load_from_memory(&file.body).is_ok());

    let thumb = app.get(&format!("/t/{id}.webp")).await;
    assert_eq!(thumb.header("content-type"), "image/webp");
    let display = app.get(&format!("/d/{id}.webp")).await;
    let shown = image::load_from_memory(&display.body).unwrap();
    assert_eq!((shown.width(), shown.height()), (2048, 1365));

    let page = app.get(&format!("/{id}")).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = page.text();
    assert!(
        html.contains(&format!(
            "<meta property=\"og:image\" content=\"http://app.test/p/{id}.jpg\">"
        )),
        "{html}"
    );
    assert!(html.contains("<meta name=\"twitter:card\" content=\"summary_large_image\">"));
    assert!(html.contains("<meta property=\"og:image:type\" content=\"image/jpeg\">"));
    assert!(html.contains("<meta property=\"og:image:width\" content=\"1200\">"));
    assert!(html.contains("<meta property=\"og:image:height\" content=\"800\">"));

    let preview = app.get(&format!("/p/{id}.jpg")).await;
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(preview.header("content-type"), "image/jpeg");
    assert!(preview.body.len() <= crate::media::PREVIEW_BYTES);
    let shown = image::load_from_memory(&preview.body).unwrap();
    assert_eq!((shown.width(), shown.height()), (1200, 800));
    assert_eq!(
        app.state
            .store
            .read(&store::key(Variant::Preview, &id))
            .await
            .unwrap()
            .as_deref(),
        Some(&preview.body[..])
    );
    // One uploaded before previews were made at upload gets one when first
    // asked for, and keeps it.
    let key = store::key(Variant::Preview, &id);
    app.state.store.delete(&key).await.unwrap();
    let made = app.get(&format!("/p/{id}.jpg")).await;
    assert_eq!(made.status, StatusCode::OK);
    let shown = image::load_from_memory(&made.body).unwrap();
    assert_eq!((shown.width(), shown.height()), (1200, 800));
    assert_eq!(
        app.state.store.read(&key).await.unwrap().as_deref(),
        Some(&made.body[..])
    );
    assert_eq!(
        app.get(&format!("/p/{id}.webp")).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(html.contains("<meta name=\"robots\" content=\"noindex\">"));
    assert!(!html.contains("<script"), "public pages carry no script");
    assert!(
        !page
            .header("content-security-policy")
            .contains("script-src"),
        "nor may they load any"
    );
    assert_eq!(page.header("x-robots-tag"), "noindex");

    let album_page = app.get(&format!("/a/{album}")).await;
    let html = album_page.text();
    assert!(html.contains("<h1>trip</h1>"), "{html}");
    assert!(
        html.contains(&format!("srcset=\"/d/{id}.webp 2048w, /{id}.jpg 3000w\"")),
        "{html}"
    );
    assert!(html.contains("<meta property=\"og:title\" content=\"trip\">"));
}

#[tokio::test]
async fn a_location_in_a_photo_never_reaches_a_visitor() {
    let app = app().await;
    let input = located_jpeg();
    assert!(contains(&input, b"51.5072N"));
    let (_, done) = app.upload(&[&input]).await;
    let id = image_id(&done[0]);
    let file = app.get(&format!("/{id}.jpg")).await;
    assert!(!contains(&file.body, b"51.5072N"));
    assert!(!contains(&file.body, b"GPS"));
}

#[tokio::test]
async fn an_unchanged_image_is_answered_not_modified() {
    let app = app().await;
    let (_, done) = app.upload(&[&jpeg(40, 30)]).await;
    let id = image_id(&done[0]);
    let first = app.get(&format!("/{id}.jpg")).await;
    let etag = first.header("etag").to_string();
    let again = app
        .send(
            Request::get(format!("/{id}.jpg"))
                .header(header::HOST, "app.test")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());
}

#[tokio::test]
async fn the_wrong_extension_is_sent_to_the_right_one_and_nonsense_is_not_found() {
    let app = app().await;
    let (_, done) = app.upload(&[&jpeg(40, 30)]).await;
    let id = image_id(&done[0]);
    let wrong = app.get(&format!("/{id}.png")).await;
    assert_eq!(wrong.status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(wrong.header("location"), format!("/{id}.jpg"));
    assert_eq!(app.get(&format!("/{id}.jpeg")).await.status, StatusCode::OK);
    for nonsense in [
        "/nonexistent",
        "/aaaaaaaaaa",
        "/aaaaaaaaaa.jpg",
        "/a/aaaaaaaaaa",
        "/t/x.webp",
        "/../etc/passwd",
    ] {
        assert_eq!(
            app.get(nonsense).await.status,
            StatusCode::NOT_FOUND,
            "{nonsense}"
        );
    }
}

#[tokio::test]
async fn a_private_album_is_its_owners_alone() {
    let app = app().await;
    let (album, done) = app.upload(&[&jpeg(40, 30)]).await;
    let id = image_id(&done[0]);
    let set = app
        .post(&format!("/albums/{album}/meta"), json!({ "private": true }))
        .await;
    assert_eq!(set.status, StatusCode::NO_CONTENT);

    let stranger = app.session("bob").await;
    for uri in [
        format!("/a/{album}"),
        format!("/{id}"),
        format!("/{id}.jpg"),
        format!("/t/{id}.webp"),
    ] {
        assert_eq!(
            app.get_as(&uri, &stranger).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    // The owner (with no identity provider, everyone without a cookie is "dev") sees it, uncached.
    let mine = app.get(&format!("/{id}.jpg")).await;
    assert_eq!(mine.status, StatusCode::OK);
    assert_eq!(mine.header("cache-control"), "private, no-store");
}

#[tokio::test]
async fn nobody_reaches_someone_elses_album() {
    let app = app().await;
    let (album, done) = app.upload(&[&jpeg(40, 30)]).await;
    let id = image_id(&done[0]);
    let bob = app.session("bob").await;
    assert_eq!(
        app.get_as(&format!("/albums/{album}"), &bob).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get_as(&format!("/images/{id}/view"), &bob).await.status,
        StatusCode::NOT_FOUND
    );
    let edits = [
        (
            "POST",
            format!("/albums/{album}/meta"),
            json!({ "title": "mine" }),
        ),
        (
            "POST",
            format!("/albums/{album}/order"),
            json!({ "ids": [id] }),
        ),
        ("POST", format!("/albums/{album}/delete"), json!({})),
        (
            "POST",
            format!("/images/{id}/caption"),
            json!({ "caption": "mine" }),
        ),
        (
            "POST",
            format!("/api/albums/{album}/uploads"),
            json!({ "files": [{ "name": "x", "size": 1 }] }),
        ),
    ];
    for (method, uri, body) in edits {
        let reply = app.write(method, &uri, body, Some(&bob)).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{uri}: {}",
            reply.text()
        );
    }
    let moved = app
        .write(
            "POST",
            "/images/move",
            json!({ "ids": [id], "to": null, "title": "stolen" }),
            Some(&bob),
        )
        .await;
    assert_eq!(moved.json()["moved"], 0);
    let deleted = app
        .write("POST", "/images/delete", json!({ "ids": [id] }), Some(&bob))
        .await;
    assert_eq!(deleted.json()["deleted"], 0);
    // Everything is still where it was.
    assert_eq!(app.get(&format!("/{id}.jpg")).await.status, StatusCode::OK);
    assert_eq!(
        app.state.db.album(&album).await.unwrap().unwrap().title,
        "trip"
    );
}

#[tokio::test]
async fn a_write_without_the_marker_header_is_refused() {
    let app = app().await;
    let forged = app
        .send(
            Request::post("/api/albums")
                .header(header::HOST, "app.test")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=x"))
                .unwrap(),
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn without_a_session_pages_go_to_sign_in_and_everything_else_is_refused() {
    let app = app().await;
    let page = app.get_as("/albums/aaaaaaaaaa", "expired").await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert!(page.header("location").starts_with("/auth/login?next="));
    let api = app
        .write("POST", "/api/albums", json!({}), Some("expired"))
        .await;
    assert_eq!(api.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_chunked_upload_resumes_where_the_server_got_to() {
    let app = app().await;
    let file = jpeg(400, 300);
    let album = app.post("/api/albums", json!({})).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let begun = app
        .post(
            &format!("/api/albums/{album}/uploads"),
            json!({ "files": [{ "name": "a.jpg", "size": file.len() }] }),
        )
        .await;
    let id = begun.json()["uploads"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let half = file.len() / 2;
    assert_eq!(
        app.chunk(&id, &file[..half], 0, file.len()).await.json()["received"],
        half
    );
    // The same chunk again, as a client that lost the answer sends it.
    let again = app.chunk(&id, &file[..half], 0, file.len()).await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.json()["received"], half);
    // Finishing early says how far it got.
    let early = app
        .post(&format!("/api/uploads/{id}/finish"), json!({}))
        .await;
    assert_eq!(early.status, StatusCode::CONFLICT);
    app.chunk(&id, &file[half..], half, file.len()).await;
    let status = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(status.json()["received"], file.len());
    image_id(
        &app.post(&format!("/api/uploads/{id}/finish"), json!({}))
            .await,
    );
}

#[tokio::test]
async fn what_is_not_an_image_is_refused_with_a_reason() {
    let app = app().await;
    let (_, done) = app.upload(&[b"<svg onload=alert(1)>"]).await;
    assert_eq!(done[0].status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(done[0].json()["error"].as_str().unwrap().contains("SVG"));

    let album = app.post("/api/albums", json!({})).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let big = app
        .post(
            &format!("/api/albums/{album}/uploads"),
            json!({ "files": [{ "name": "huge.jpg", "size": 1u64 << 40 }] }),
        )
        .await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn deleting_an_album_deletes_its_files() {
    let app = app().await;
    let (album, done) = app.upload(&[&jpeg(40, 30), &jpeg(30, 40)]).await;
    let ids: Vec<String> = done.iter().map(image_id).collect();
    assert_eq!(
        app.get(&format!("/p/{}.jpg", ids[0])).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.post(&format!("/albums/{album}/delete"), json!({}))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    for id in &ids {
        assert_eq!(
            app.get(&format!("/{id}.jpg")).await.status,
            StatusCode::NOT_FOUND
        );
        for v in [Variant::Original, Variant::Thumb, Variant::Preview] {
            assert!(
                app.state
                    .store
                    .read(&store::key(v, id))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[tokio::test]
async fn images_reorder_move_and_take_captions() {
    let app = app().await;
    let (album, done) = app
        .upload(&[&jpeg(40, 30), &jpeg(30, 40), &jpeg(20, 20)])
        .await;
    let ids: Vec<String> = done.iter().map(image_id).collect();
    let order = app
        .post(
            &format!("/albums/{album}/order"),
            json!({ "ids": [ids[2], ids[0]] }),
        )
        .await;
    assert_eq!(order.status, StatusCode::NO_CONTENT);
    let now: Vec<String> = app
        .state
        .db
        .images(&album)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(now, [ids[2].clone(), ids[0].clone(), ids[1].clone()]);

    app.post(
        &format!("/images/{}/caption", ids[0]),
        json!({ "caption": "the first one\nwith a second line" }),
    )
    .await;
    let page = app.get(&format!("/a/{album}")).await.text();
    assert!(page.contains("the first one\nwith a second line"));
    assert!(
        page.contains("<meta property=\"og:description\" content=\"the first one\">"),
        "{page}"
    );

    let moved = app
        .post(
            "/images/move",
            json!({ "ids": [ids[1]], "to": null, "title": "elsewhere" }),
        )
        .await;
    assert_eq!(moved.json()["moved"], 1);
    let to = moved.json()["to"].as_str().unwrap().to_string();
    assert_eq!(
        app.state.db.album(&to).await.unwrap().unwrap().title,
        "elsewhere"
    );
    assert_eq!(app.state.db.images(&album).await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_app_pages_draw_and_carry_their_policy() {
    let app = app().await;
    let (album, done) = app.upload(&[&jpeg(40, 30)]).await;
    let id = image_id(&done[0]);
    let library = app.get("/").await;
    assert_eq!(library.status, StatusCode::OK);
    assert!(
        library
            .text()
            .contains(&format!("href=\"/albums/{album}\""))
    );
    let csp = library.header("content-security-policy");
    assert!(
        csp.contains("frame-ancestors 'none'") && !csp.contains("unsafe-inline"),
        "{csp}"
    );
    assert_eq!(library.header("x-frame-options"), "DENY");

    let page = app.get(&format!("/albums/{album}")).await;
    assert!(page.text().contains(&format!("id=\"i-{id}\"")));
    assert!(page.text().contains(&format!("http://app.test/a/{album}")));

    let viewer = app.get(&format!("/images/{id}/view")).await;
    let html = viewer.text();
    assert!(html.contains("id=\"viewer-body\""));
    assert!(
        html.contains(&format!("[img]http://app.test/{id}.jpg[/img]")),
        "{html}"
    );
}

#[tokio::test]
async fn the_live_stream_opens_with_the_grid() {
    let app = app().await;
    let (album, _) = app.upload(&[&jpeg(40, 30)]).await;
    let res = app
        .router
        .clone()
        .oneshot(
            Request::get(format!("/stream?album={album}"))
                .header(header::HOST, "app.test")
                .header(header::ACCEPT, "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut body = res.into_body();
    let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    let text = String::from_utf8_lossy(&frame);
    assert!(text.contains("event: datastar-patch-elements"), "{text}");
    assert!(text.contains("id=\"grid\""), "{text}");
}

#[tokio::test]
async fn a_public_domain_of_its_own_serves_nothing_but_public_pages() {
    let app = app_with(|c| c.public_url = "https://pics.test".into()).await;
    let (album, done) = app.upload(&[&jpeg(40, 30)]).await;
    let id = image_id(&done[0]);
    assert_eq!(
        done[0].json()["image"]["page"],
        format!("https://pics.test/{id}")
    );
    let on_public = |uri: String| {
        Request::get(uri)
            .header(header::HOST, "pics.test")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.send(on_public(format!("/a/{album}"))).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.send(on_public(format!("/{id}.jpg"))).await.status,
        StatusCode::OK
    );
    for hidden in [
        format!("/albums/{album}"),
        "/auth/login".into(),
        "/api/uploads/aaaaaaaaaa".into(),
    ] {
        assert_eq!(
            app.send(on_public(hidden.clone())).await.status,
            StatusCode::NOT_FOUND,
            "{hidden}"
        );
    }
    let front = app.send(on_public("/".into())).await;
    assert_eq!(front.status, StatusCode::OK);
    assert!(
        !front.text().contains("dither"),
        "the public front page names nothing"
    );
    // A private album is never shown there, even to its owner.
    app.post(&format!("/albums/{album}/meta"), json!({ "private": true }))
        .await;
    assert_eq!(
        app.send(on_public(format!("/a/{album}"))).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn crawlers_are_welcome_and_the_probe_answers() {
    let app = app().await;
    assert!(app.get("/robots.txt").await.text().contains("Allow: /"));
    assert_eq!(app.get("/healthz").await.text(), "ok");
    let css = app.get(crate::web::assets::css_url()).await;
    assert_eq!(
        css.header("cache-control"),
        "public, max-age=31536000, immutable"
    );
}

/// Video `id`'s row once it is no longer processing, waiting up to two
/// minutes for the worker.
async fn made(app: &App, id: &str) -> crate::db::Image {
    for _ in 0..1200 {
        let (image, _) = app.state.db.image(id).await.unwrap().unwrap();
        if image.status != crate::db::Status::Processing {
            return image;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("video {id} was never made");
}

impl App {
    async fn get_with(&self, uri: &str, headers: &[(header::HeaderName, &str)]) -> Reply {
        let mut req = Request::get(uri).header(header::HOST, "app.test");
        for (name, value) in headers {
            req = req.header(name, *value);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }
}

#[tokio::test]
async fn a_video_is_made_in_the_background_then_played_in_ranges_and_previewed() {
    if crate::video::tests::tools().await.is_none() {
        return;
    }
    let app = app().await;
    let dir = tempfile::tempdir().unwrap();
    let phone = std::fs::read(crate::video::tests::phone_clip(dir.path())).unwrap();
    let (album, done) = app.upload(&[&phone]).await;
    let id = image_id(&done[0]);
    assert_eq!(done[0].json()["image"]["processing"], true);

    let image = made(&app, &id).await;
    assert!(image.ready(), "{}", image.failure);
    assert_eq!((image.width, image.height), (360, 640));
    assert!(image.display, "HEVC gets a playback copy");
    assert!(image.audio);
    assert!(app.state.uploads.jobs.progress(&id).is_none());

    let whole = app
        .get_with(
            &format!("/{id}.mp4"),
            &[(header::ACCEPT_ENCODING, "gzip, br")],
        )
        .await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.header("content-type"), "video/mp4");
    assert_eq!(whole.header("accept-ranges"), "bytes");
    assert_eq!(
        whole.header("content-encoding"),
        "",
        "video is never compressed again"
    );
    assert_eq!(whole.body.len() as u64, image.bytes);
    let text = String::from_utf8_lossy(&whole.body);
    assert!(!text.contains("51.5072") && !text.contains("ISO6709"));
    let len = whole.body.len();

    let part = app
        .get_with(&format!("/{id}.mp4"), &[(header::RANGE, "bytes=0-99")])
        .await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.header("content-range"), format!("bytes 0-99/{len}"));
    assert_eq!(part.body, whole.body.slice(0..100));
    let tail = app
        .get_with(&format!("/{id}.mp4"), &[(header::RANGE, "bytes=-10")])
        .await;
    assert_eq!(tail.body, whole.body.slice(len - 10..));
    let beyond = app
        .get_with(
            &format!("/{id}.mp4"),
            &[(header::RANGE, &format!("bytes={len}-"))],
        )
        .await;
    assert_eq!(beyond.status, StatusCode::RANGE_NOT_SATISFIABLE);
    let none = app
        .get_with(&format!("/{id}.mp4"), &[(header::RANGE, "bytes=-0")])
        .await;
    assert_eq!(none.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(beyond.header("content-range"), format!("bytes */{len}"));
    // A range of a version that is not this one is answered whole.
    let stale = app
        .get_with(
            &format!("/{id}.mp4"),
            &[(header::RANGE, "bytes=0-9"), (header::IF_RANGE, "\"old\"")],
        )
        .await;
    assert_eq!((stale.status, stale.body.len()), (StatusCode::OK, len));

    let play = app.get(&format!("/d/{id}.mp4")).await;
    assert_eq!(play.status, StatusCode::OK);
    assert_eq!(play.header("content-type"), "video/mp4");
    assert_ne!(play.body, whole.body);
    assert_eq!(
        app.get(&format!("/d/{id}.webp")).await.status,
        StatusCode::NOT_FOUND
    );
    let thumb = app.get(&format!("/t/{id}.webp")).await;
    assert_eq!(thumb.header("content-type"), "image/webp");
    let frame = image::load_from_memory(&thumb.body).unwrap();
    assert!(frame.height() > frame.width(), "the frame is upright");
    let preview = app.get(&format!("/p/{id}.jpg")).await;
    assert_eq!(preview.header("content-type"), "image/jpeg");

    let page = app.get(&format!("/{id}")).await.text();
    assert!(page.contains("<video"), "{page}");
    assert!(
        page.contains("/assets/player.js?v="),
        "a video page loads the player"
    );
    let csp = app.get(&format!("/{id}")).await;
    let csp = csp.header("content-security-policy");
    assert!(
        csp.contains("script-src 'self'") && csp.contains("media-src 'self'"),
        "{csp}"
    );
    assert!(page.contains("data-player"));
    assert!(page.contains("<meta property=\"og:type\" content=\"video.other\">"));
    assert!(page.contains(&format!(
        "<meta property=\"og:video\" content=\"http://app.test/d/{id}.mp4\">"
    )));
    assert!(page.contains(&format!(
        "<meta property=\"og:image\" content=\"http://app.test/p/{id}.jpg\">"
    )));
    assert!(page.contains("· 0:02 ·"), "{page}");
    let public = app.get(&format!("/a/{album}")).await.text();
    assert!(public.contains(&format!("src=\"/d/{id}.mp4\"")), "{public}");
}

#[tokio::test]
async fn an_image_declared_a_video_is_still_held_to_the_image_limit() {
    let app = app_with(|c| c.max_upload_bytes = 1000).await;
    let album = app.post("/api/albums", json!({})).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let file = jpeg(400, 300);
    assert!(file.len() > 1000);
    let begun = app
        .post(
            &format!("/api/albums/{album}/uploads"),
            json!({ "files": [{ "name": "a.mp4", "size": file.len(), "type": "video/mp4" }] }),
        )
        .await;
    if app.state.config.video.mode == crate::config::VideoMode::Off {
        assert_eq!(begun.status, StatusCode::PAYLOAD_TOO_LARGE);
        return;
    }
    let id = begun.json()["uploads"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    app.chunk(&id, &file, 0, file.len()).await;
    let done = app
        .post(&format!("/api/uploads/{id}/finish"), json!({}))
        .await;
    assert_eq!(done.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        done.json()["error"]
            .as_str()
            .unwrap()
            .contains("limit for an image")
    );
}

#[tokio::test]
async fn a_video_dither_does_not_take_is_refused_before_it_is_sent() {
    let app = app().await;
    let album = app.post("/api/albums", json!({})).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let begun = app
        .post(
            &format!("/api/albums/{album}/uploads"),
            json!({ "files": [{ "name": "camcorder.mts", "size": 500_000_000u64, "type": "video/mp2t" }] }),
        )
        .await;
    assert_eq!(begun.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(begun.json()["error"].as_str().unwrap().contains("MP4"));
}

#[tokio::test]
async fn a_video_deleted_while_it_is_made_stops_and_leaves_nothing() {
    if crate::video::tests::tools().await.is_none() {
        return;
    }
    let app = app().await;
    let dir = tempfile::tempdir().unwrap();
    // MPEG-4 Part 2, which no browser plays, so it is transcoded: long
    // enough to be deleted part way.
    let path = crate::video::tests::clip(
        dir.path(),
        "old.mp4",
        30,
        "1280x720",
        &["-c:v", "mpeg4", "-q:v", "20", "-c:a", "aac"],
    );
    let file = std::fs::read(path).unwrap();
    let (album, done) = app.upload_queued(&[&file]).await;
    let id = image_id(&done[0]);
    // Grids and pages show its progress to its owner, and nothing to anyone else.
    assert_eq!(
        app.get(&format!("/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!app.get(&format!("/a/{album}")).await.text().contains(&id));
    let videos = &app.state.uploads.jobs;
    for _ in 0..3000 {
        if videos.progress(&id).is_some_and(|p| p > 110) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        videos.progress(&id).is_some_and(|p| p < 950),
        "transcoding, and not yet done"
    );
    let gone = app
        .post("/images/delete", json!({ "ids": [id.clone()] }))
        .await;
    assert_eq!(gone.json()["deleted"], 1);
    assert!(videos.progress(&id).is_none());
    // The worker stops, and tidies what it was working in.
    let jobs = crate::jobs::dir(&app.state.config.state_dir);
    let left = || std::fs::read_dir(&jobs).unwrap().count();
    for _ in 0..1000 {
        if left() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(left(), 0, "the worker let it go and tidied up");
    for v in [
        Variant::Original,
        Variant::Display,
        Variant::Thumb,
        Variant::Preview,
    ] {
        assert!(
            app.state
                .store
                .read(&store::key(v, &id))
                .await
                .unwrap()
                .is_none(),
            "{v:?} was left behind"
        );
    }
}
