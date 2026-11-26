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
use crate::config::Config;
use crate::db::Db;
use crate::store::{self, Store, Variant};
use crate::upload::Uploads;

struct App {
    router: Router,
    state: AppState,
    /// The session of `dev`, an admin, which requests carry unless they name
    /// another.
    cookie: String,
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
    db.test_accounts().await;
    let dev = db.account_named("dev").await.unwrap().unwrap();
    let cookie = db
        .open_session(dev.id, std::time::Duration::from_secs(3600))
        .await
        .unwrap();
    let jobs = Arc::new(crate::jobs::Hub::new(db.clone(), store.clone(), &config).unwrap());
    let hub = jobs.clone();
    let uploads = Arc::new(Uploads::new(db.clone(), store.clone(), &config, jobs).unwrap());
    // The worker API on a port of its own, as it is served, and a worker
    // reaching it there.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let queue = format!("http://{}", listener.local_addr().unwrap());
    let work = super::work::router(super::work::Work {
        hub: hub.clone(),
        token: config.worker_token.as_deref().map(super::work::digest),
        limit: usize::try_from(4 * config.max_upload_bytes.max(config.video.max_bytes))
            .unwrap_or(usize::MAX),
    });
    tokio::spawn(async move { axum::serve(listener, work).await });
    {
        let mut config = config_for(dir.path());
        config.queue_url = queue;
        // A worker on another machine: a state directory of its own.
        config.state_dir = dir.path().join("worker");
        tokio::spawn(async move {
            if let Err(e) = crate::worker::run(&config).await {
                tracing::warn!(error = %e, "the test's worker stopped");
            }
        });
    }
    // Connected before anything is uploaded, as a deployment's worker is: a
    // picture finished before then is answered as processing, not made.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !hub.takes_pictures() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the test's worker connects");
    let (stop, stopping) = tokio::sync::watch::channel(false);
    // Kept for the life of the test: a dropped sender reads as stopping.
    std::mem::forget(stop);
    let state = AppState::new(Inner {
        verifier: koan_auth::password::Verifier::new(),
        sign_ins: koan_auth::limit::RateLimiter::default(),
        password_changes: koan_auth::limit::FailureLimiter::new(5),
        setup: Some(koan_auth::setup::SetupCode::new().unwrap()),
        db,
        store,
        uploads,
        views: crate::views::Views::default(),
        stopping,
        config,
    });
    App {
        router: super::router(state.clone()),
        state,
        cookie,
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
        self.get_as(uri, &self.cookie).await
    }

    /// As someone signed in to nothing.
    async fn get_anonymous(&self, uri: &str) -> Reply {
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
        let cookie = cookie.unwrap_or(&self.cookie);
        req = req.header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"));
        self.send(req.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn post(&self, uri: &str, body: Value) -> Reply {
        self.write("POST", uri, body, None).await
    }

    /// A form posted as a `kit::Form` posts it: through Datastar, its fields
    /// form-encoded.
    async fn submit(&self, uri: &str, fields: &[(&str, &str)], cookie: Option<&str>) -> Reply {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let cookie = cookie.unwrap_or(&self.cookie);
        self.send(
            Request::post(uri)
                .header(header::HOST, "app.test")
                .header("datastar-request", "true")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    async fn chunk(&self, upload: &str, bytes: &[u8], start: usize, size: usize) -> Reply {
        let end = start + bytes.len() - 1;
        self.send(
            Request::put(format!("/api/uploads/{upload}"))
                .header(header::HOST, "app.test")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={}", self.cookie))
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

    /// A session for the account `name`, made a user's if it is new.
    async fn session(&self, name: &str) -> String {
        let db = &self.state.db;
        let id = match db.account_named(name).await.unwrap() {
            Some(user) => user.id,
            None => {
                db.create_account(name, koan_auth::Role::User)
                    .await
                    .unwrap()
                    .id
            }
        };
        db.open_session(id, std::time::Duration::from_secs(3600))
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
        assert!(
            app.state.db.holder(&ann).await.unwrap().is_some(),
            "{elsewhere}"
        );
    }
    app.send(from("same-origin")).await;
    assert!(app.state.db.holder(&ann).await.unwrap().is_none());
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
    assert_eq!(
        app.get(&format!("/p/{id}.webp")).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(html.contains("<meta name=\"robots\" content=\"noindex, nofollow\">"));
    assert!(!html.contains("<script"), "public pages carry no script");
    assert!(
        page.header("content-security-policy")
            .contains("script-src 'none'"),
        "nor may they load any"
    );
    assert_eq!(page.header("x-robots-tag"), "noindex");

    let album_page = app.get(&format!("/a/{album}")).await;
    let html = album_page.text();
    assert!(
        html.contains("<h1 class=\"normal-case\">trip</h1>"),
        "{html}"
    );
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
        let missing = app.get_as(&uri, &stranger).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{uri}");
        // A shared cache keeping it would show the owner the same.
        assert_eq!(missing.header("cache-control"), "no-store", "{uri}");
    }
    // The owner sees it, uncached.
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
    // Its editor is not bob's: he is sent to the page anyone with its link sees.
    let editor = app.get_as(&format!("/albums/{album}"), &bob).await;
    assert_eq!(editor.status, StatusCode::SEE_OTHER);
    assert_eq!(
        editor.header("location"),
        format!("http://app.test/a/{album}")
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
        .write(
            "POST",
            &format!("/images/delete?ids={id}"),
            json!({}),
            Some(&bob),
        )
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
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
                .header(header::COOKIE, format!("{SESSION_COOKIE}={}", app.cookie))
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
    assert!(page.header("location").starts_with("/login?next="));
    let api = app
        .write("POST", "/api/albums", json!({}), Some("expired"))
        .await;
    assert_eq!(api.status, StatusCode::UNAUTHORIZED);
}

/// A form's post from this origin, as a browser sends one.
fn form(uri: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::post(uri)
        .header(header::HOST, "app.test")
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, format!("{SESSION_COOKIE}={c}"));
    }
    req.body(Body::from(body.to_string())).unwrap()
}

/// The session a response's `Set-Cookie` opens.
fn opened(reply: &Reply) -> String {
    let set = reply.header("set-cookie");
    set.strip_prefix(&format!("{SESSION_COOKIE}="))
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("no session cookie in {set:?}"))
        .to_string()
}

#[tokio::test]
async fn a_password_signs_in_and_a_wrong_one_does_not() {
    let app = app().await;
    let wrong = app
        .send(form("/login", "username=ann&password=nope&next=%2F", None))
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.text().contains("Wrong username or password."));
    let right = app
        .send(form(
            "/login",
            "username=ann&password=a+test+password&next=%2Falbums",
            None,
        ))
        .await;
    assert_eq!(right.status, StatusCode::SEE_OTHER);
    assert_eq!(right.header("location"), "/albums");
    let ann = opened(&right);
    assert_eq!(app.get_as("/", &ann).await.status, StatusCode::OK);
    // Another site's form signs nobody in.
    let mut forged = form("/login", "username=ann&password=a+test+password", None);
    forged
        .headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(app.send(forged).await.status, StatusCode::FORBIDDEN);
    // Signing out ends the session.
    app.get_as("/auth/logout", &ann).await;
    assert_eq!(app.get_as("/", &ann).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_first_admin_needs_the_logged_code_and_is_made_once() {
    let app = app().await;
    app.state
        .db
        .run(|c| c.execute("DELETE FROM users", []).map(drop))
        .await
        .unwrap();
    let login = app.get_anonymous("/login").await;
    assert_eq!(login.header("location"), "/setup");
    let code = app.state.setup.as_ref().unwrap().as_str().to_string();
    let wrong = app
        .send(form(
            "/setup",
            "code=nope&username=ada&password=long+enough&confirm=long+enough",
            None,
        ))
        .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    assert!(wrong.text().contains("the setup code"), "{}", wrong.text());
    let mismatch = app
        .send(form(
            "/setup",
            &format!("code={code}&username=ada&password=long+enough&confirm=other+enough"),
            None,
        ))
        .await;
    assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
    let made = app
        .send(form(
            "/setup",
            &format!("code={code}&username=ada&password=long+enough&confirm=long+enough"),
            None,
        ))
        .await;
    assert_eq!(made.status, StatusCode::SEE_OTHER);
    let ada = opened(&made);
    assert_eq!(app.get_as("/users", &ada).await.status, StatusCode::OK);
    // Once there is an account, there is no setup.
    let again = app
        .send(form(
            "/setup",
            &format!("code={code}&username=eve&password=long+enough&confirm=long+enough"),
            None,
        ))
        .await;
    assert_eq!(again.header("location"), "/login");
    assert!(app.state.db.account_named("eve").await.unwrap().is_none());
}

/// The invite link the accounts page answered with.
fn invite_link(reply: &Reply) -> (String, String) {
    let text = reply.text();
    let at = text.find("/join#invite=").expect("an invite link");
    let fragment = &text[at + "/join#".len()..];
    let fragment = &fragment[..fragment.find('"').unwrap()];
    let fragment = fragment.replace("&#38;", "&").replace("&amp;", "&");
    let mut invite = String::new();
    let mut username = String::new();
    for (k, v) in url::form_urlencoded::parse(fragment.as_bytes()) {
        match &*k {
            "invite" => invite = v.into_owned(),
            "username" => username = v.into_owned(),
            _ => {}
        }
    }
    (invite, username)
}

#[tokio::test]
async fn an_invite_is_spent_by_choosing_a_password() {
    let app = app().await;
    let made = app
        .submit("/users", &[("newuser", "eve"), ("newrole", "user")], None)
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.text());
    let (invite, username) = invite_link(&made);
    assert_eq!(username, "eve");
    let body = |password: &str| {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("invite", &invite)
            .append_pair("password", password)
            .append_pair("confirm", password)
            .finish()
    };
    let short = app.send(form("/join", &body("short"), None)).await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST);
    let joined = app
        .send(form("/join", &body("a long password"), None))
        .await;
    assert_eq!(joined.status, StatusCode::SEE_OTHER);
    let eve = opened(&joined);
    assert_eq!(app.get_as("/", &eve).await.status, StatusCode::OK);
    // Used once: the password it set spent it.
    let again = app
        .send(form("/join", &body("another password"), None))
        .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    // Taken names are refused.
    let taken = app
        .submit("/users", &[("newuser", "eve"), ("newrole", "user")], None)
        .await;
    assert!(taken.text().contains("already an account"));
    // An unknown role is refused, not taken as a user.
    let unknown = app
        .submit(
            "/users",
            &[("newuser", "fay"), ("newrole", "Readonly")],
            None,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn only_admins_reach_the_accounts_page() {
    let app = app().await;
    let ann = app.session("ann").await;
    assert_eq!(
        app.get_as("/users", &ann).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.submit(
            "/users",
            &[("newuser", "x"), ("newrole", "user")],
            Some(&ann)
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let page = app.get("/users").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("ann"));
}

#[tokio::test]
async fn a_deleted_account_takes_its_albums_and_sessions() {
    let app = app().await;
    let bob = app.session("bob").await;
    let album = app
        .write("POST", "/api/albums", json!({}), Some(&bob))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let id = app.state.db.account_named("bob").await.unwrap().unwrap().id;
    let gone = app
        .write("POST", &format!("/users/{id}/delete"), json!({}), None)
        .await;
    assert_eq!(gone.status, StatusCode::OK);
    assert!(app.state.db.album(&album).await.unwrap().is_none());
    assert_eq!(app.get_as("/", &bob).await.status, StatusCode::SEE_OTHER);
    // An admin does not delete themselves here.
    let dev = app.state.db.account_named("dev").await.unwrap().unwrap().id;
    let mine = app
        .write("POST", &format!("/users/{dev}/delete"), json!({}), None)
        .await;
    assert!(mine.text().contains("your own account"), "{}", mine.text());
    // Nor resets their own password, which would sign them out.
    let reset = app
        .write("POST", &format!("/users/{dev}/reset"), json!({}), None)
        .await;
    assert!(
        reset.text().contains("your own account"),
        "{}",
        reset.text()
    );
    assert_eq!(app.get("/").await.status, StatusCode::OK);
    assert!(app.state.db.account_named("dev").await.unwrap().is_some());
}

#[tokio::test]
async fn an_account_changes_its_own_password_and_signs_out_elsewhere() {
    let app = app().await;
    let here = app.session("ann").await;
    let elsewhere = app.session("ann").await;
    let page = app.get_as("/account", &here).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("ann"));
    let change = |current: &'static str, password: &'static str, confirm: &'static str| {
        [
            ("current", current),
            ("password", password),
            ("confirm", confirm),
        ]
    };
    let wrong = app
        .submit(
            "/account/password",
            &change("not it", "a new password", "a new password"),
            Some(&here),
        )
        .await;
    assert!(wrong.text().contains("The current password is wrong."));
    let mismatch = app
        .submit(
            "/account/password",
            &change("a test password", "a new password", "another one"),
            Some(&here),
        )
        .await;
    assert!(mismatch.text().contains("match"));
    let short = app
        .submit(
            "/account/password",
            &change("a test password", "short", "short"),
            Some(&here),
        )
        .await;
    assert!(short.text().contains("at least 8"));
    // Nothing refused ended a session.
    assert_eq!(app.get_as("/", &here).await.status, StatusCode::OK);
    // A reset made meanwhile wins: the session that asked is gone.
    let bob = app.session("bob").await;
    let bob_id = app.state.db.account_named("bob").await.unwrap().unwrap().id;
    let checked = app
        .state
        .db
        .account_named("bob")
        .await
        .unwrap()
        .unwrap()
        .password_hash;
    app.state.db.reset_password(bob_id).await.unwrap();
    assert!(matches!(
        app.state
            .db
            .change_password(
                bob_id,
                &checked,
                &bob,
                "a new password",
                std::time::Duration::from_secs(60)
            )
            .await,
        Err(koan_auth::accounts::AccountError::NoSuchUser(_))
    ));

    let changed = app
        .submit(
            "/account/password",
            &change("a test password", "a new password", "a new password"),
            Some(&here),
        )
        .await;
    assert!(changed.text().contains("Your password is changed"));
    // This browser stays signed in with a new session; the others end.
    let renewed = opened(&changed);
    assert_eq!(app.get_as("/", &renewed).await.status, StatusCode::OK);
    assert_eq!(app.get_as("/", &here).await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.get_as("/", &elsewhere).await.status,
        StatusCode::SEE_OTHER
    );
    let signed_in = app
        .send(form(
            "/login",
            "username=ann&password=a+new+password&next=%2F",
            None,
        ))
        .await;
    assert_eq!(signed_in.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_readonly_account_still_changes_its_own_password() {
    let app = app().await;
    let ann = app.session("ann").await;
    let id = app.state.db.account_named("ann").await.unwrap().unwrap().id;
    app.write(
        "POST",
        &format!("/users/{id}/role"),
        json!({ "role": "readonly" }),
        None,
    )
    .await;
    let changed = app
        .submit(
            "/account/password",
            &[
                ("current", "a test password"),
                ("password", "a new password"),
                ("confirm", "a new password"),
            ],
            Some(&ann),
        )
        .await;
    assert!(
        changed.text().contains("Your password is changed"),
        "{}",
        changed.text()
    );
    let renewed = opened(&changed);
    let write = app
        .write("POST", "/api/albums", json!({}), Some(&renewed))
        .await;
    assert_eq!(write.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_readonly_account_sees_and_changes_nothing() {
    let app = app().await;
    let ann = app.session("ann").await;
    let id = app.state.db.account_named("ann").await.unwrap().unwrap().id;
    let changed = app
        .write(
            "POST",
            &format!("/users/{id}/role"),
            json!({ "role": "readonly" }),
            None,
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_eq!(app.get_as("/", &ann).await.status, StatusCode::OK);
    let write = app
        .write("POST", "/api/albums", json!({}), Some(&ann))
        .await;
    assert_eq!(write.status, StatusCode::FORBIDDEN);
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
    let deleted = app
        .post(&format!("/albums/{album}/delete"), json!({}))
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
    // The page's question closes, and the page goes to the albums.
    assert!(deleted.text().contains("_kitconfirm"), "{}", deleted.text());
    assert!(deleted.text().contains("location.assign('/')"));
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
    // The signed-in pages say what they run; the public ones do not.
    let version = format!("dither {}", env!("CARGO_PKG_VERSION"));
    assert!(page.text().contains(&version));
    assert!(
        !app.get(&format!("/a/{album}"))
            .await
            .text()
            .contains(&version)
    );
    assert!(page.text().contains(&format!("http://app.test/a/{album}")));

    let viewer = app.get(&format!("/images/{id}/view")).await;
    let html = viewer.text();
    assert!(html.contains("id=\"viewer-body\""));
    // Each image's caption is its own element, so the next image's does not
    // inherit what was typed into this one's.
    assert!(html.contains(&format!("id=\"caption-{id}\"")), "{html}");
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
                .header(header::COOKIE, format!("{SESSION_COOKIE}={}", app.cookie))
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
        "/login".into(),
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
    let css = app.get(&crate::web::assets::css_url()).await;
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
    assert!(!image.display, "one copy is kept");
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
    // Opened on its own, the browser's player loads it: no sandbox, whose
    // opaque origin `'self'` would not match.
    let policy = whole.header("content-security-policy");
    assert!(
        policy.contains("media-src 'self'") && !policy.contains("sandbox"),
        "{policy}"
    );
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

    // HEVC, converted: the one copy is H.264 at the video's own size.
    let kept = dir.path().join("kept.mp4");
    std::fs::write(&kept, &whole.body).unwrap();
    let tools = crate::video::tests::tools().await.unwrap();
    let probe = tools
        .probe(crate::sanitize::Container::Mp4, &kept)
        .await
        .unwrap();
    assert_eq!(probe.video.codec, "h264");
    assert_eq!(probe.shown(), (360, 640));
    // A display address names it too.
    let play = app.get(&format!("/d/{id}.mp4")).await;
    assert_eq!(play.status, StatusCode::OK);
    assert_eq!(play.body, whole.body);
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
        "<meta property=\"og:video\" content=\"http://app.test/{id}.mp4\">"
    )));
    assert!(page.contains(&format!(
        "<meta property=\"og:image\" content=\"http://app.test/p/{id}.jpg\">"
    )));
    assert!(page.contains("· 0:02 ·"), "{page}");
    let public = app.get(&format!("/a/{album}")).await.text();
    assert!(public.contains(&format!("src=\"/{id}.mp4\"")), "{public}");
    // The name it was uploaded under is its owner's alone.
    for html in [&public, &page] {
        assert!(!html.contains("0.jpg"), "{html}");
        assert!(html.contains("data-title=\"a video\""), "{html}");
    }
}

#[tokio::test]
async fn a_video_kept_as_recorded_is_its_one_copy() {
    let Some(tools) = crate::video::tests::tools().await else {
        return;
    };
    let app = app().await;
    let dir = tempfile::tempdir().unwrap();
    let phone = std::fs::read(crate::video::tests::phone_clip(dir.path())).unwrap();
    let album = app.post("/api/albums", json!({})).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let begun = app
        .post(
            &format!("/api/albums/{album}/uploads"),
            json!({ "files": [{ "name": "phone.mov", "size": phone.len(), "type": "video/quicktime" }], "as_recorded": true }),
        )
        .await;
    let upload = begun.json()["uploads"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    app.chunk(&upload, &phone, 0, phone.len()).await;
    let id = image_id(
        &app.post(&format!("/api/uploads/{upload}/finish"), json!({}))
            .await,
    );
    let image = made(&app, &id).await;
    assert!(image.ready() && image.as_recorded && !image.display);
    let kept = dir.path().join("kept.mp4");
    std::fs::write(&kept, &app.get(&format!("/{id}.mp4")).await.body).unwrap();
    let probe = tools
        .probe(crate::sanitize::Container::Mp4, &kept)
        .await
        .unwrap();
    assert_eq!(probe.video.codec, "hevc", "as recorded");
    assert_eq!(probe.shown(), (360, 640));
    // Kept as recorded, and still without the encoder's SEI message, where
    // a camera writes its own.
    let body = std::fs::read(&kept).unwrap();
    assert!(
        !body.windows(11).any(|w| w == b"x265 (build"),
        "SEI survived"
    );
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
async fn a_picture_that_cannot_be_made_is_refused_to_its_sender_and_leaves_nothing() {
    let app = app().await;
    // Its header reads, so finishing queues it; its data does not.
    let mut broken = jpeg(300, 200);
    broken.truncate(broken.len() / 3);
    let (album, done) = app.upload(&[&broken]).await;
    assert_eq!(
        done[0].status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        done[0].text()
    );
    assert!(
        done[0].json()["error"]
            .as_str()
            .unwrap()
            .contains("can't be read")
    );
    let images = app.state.db.images(&album).await.unwrap();
    assert!(images.is_empty(), "{images:?}");
}

#[tokio::test]
async fn an_albums_app_address_sends_anyone_but_its_owner_to_its_link() {
    let app = app().await;
    let (album, _) = app.upload(&[&jpeg(64, 48)]).await;
    // Its owner gets the editor.
    let own = app.get(&format!("/albums/{album}")).await;
    assert_eq!(own.status, StatusCode::OK);
    assert!(own.text().contains("data-album"));
    // Anyone else, a link preview's fetcher included, the page it shares.
    let bob = app.session("bob").await;
    let other = app.get_as(&format!("/albums/{album}"), &bob).await;
    assert_eq!(other.status, StatusCode::SEE_OTHER);
    assert_eq!(
        other.header("location"),
        format!("http://app.test/a/{album}")
    );
    // Not the app's own requests, which a redirect would swap a page into.
    let fetched = app
        .write("GET", &format!("/albums/{album}"), json!({}), Some(&bob))
        .await;
    assert_eq!(fetched.status, StatusCode::NOT_FOUND);
    // Unless it is private, when it is as if there were none.
    app.post(&format!("/albums/{album}/meta"), json!({ "private": true }))
        .await;
    let hidden = app.get_as(&format!("/albums/{album}"), &bob).await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    let none = app.get_as("/albums/AAAAAAAAAA", &bob).await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);
    // A browser whose session has lapsed is likely its owner's: sign-in,
    // coming back to the address as it was.
    app.post(
        &format!("/albums/{album}/meta"),
        json!({ "private": false }),
    )
    .await;
    let lapsed = app.get_as(&format!("/albums/{album}?x=1"), "expired").await;
    assert_eq!(lapsed.status, StatusCode::SEE_OTHER);
    let to = lapsed.header("location");
    assert!(
        to.starts_with("/login?next=") && to.contains("x%3D1"),
        "{to}"
    );
    let fetched = app
        .write(
            "GET",
            &format!("/albums/{album}"),
            json!({}),
            Some("expired"),
        )
        .await;
    assert_eq!(
        fetched.status,
        StatusCode::UNAUTHORIZED,
        "the app's own request"
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
    // Grids and pages show its progress to its owner, and nothing to anyone
    // else, in a way nothing keeps: its link works once it is made.
    let early = app.get(&format!("/{id}")).await;
    assert_eq!(early.status, StatusCode::NOT_FOUND);
    assert_eq!(early.header("cache-control"), "no-store");
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
        .post(&format!("/images/delete?ids={id}"), json!({}))
        .await;
    assert_eq!(gone.status, StatusCode::OK, "{}", gone.text());
    assert!(videos.progress(&id).is_none());
    // The worker stops, and tidies what it was working in, and the upload
    // goes from the store.
    let jobs = crate::jobs::dir(&app._dir.path().join("worker"));
    let left = || std::fs::read_dir(&jobs).unwrap().count();
    for _ in 0..1000 {
        if left() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(left(), 0, "the worker let it go and tidied up");
    assert!(
        app.state
            .store
            .read(&crate::jobs::upload(&id))
            .await
            .unwrap()
            .is_none()
    );
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

#[tokio::test]
async fn a_cdn_in_front_names_the_visitor() {
    let app = app_with(|c| {
        c.trusted_proxies = vec!["198.51.100.0/24".parse().unwrap()];
        c.address_header = Some(header::HeaderName::from_static("cf-connecting-ip"));
    })
    .await;
    let (_, done) = app.upload(&[&jpeg(300, 200)]).await;
    let id = image_id(&done[0]);
    let page = format!("/{id}");
    let forwarded = header::HeaderName::from_static("x-forwarded-for");
    let client = header::HeaderName::from_static("cf-connecting-ip");
    // One visitor through two of the CDN's edges, by its header and by its
    // `X-Forwarded-For` (once forging an earlier entry); another through the
    // first edge; and one that skipped the CDN, claiming other addresses.
    for (chain, claimed) in [
        ("198.51.100.1", Some("192.0.2.1")),
        ("198.51.100.2", Some("192.0.2.1")),
        ("192.0.2.9, 192.0.2.1, 198.51.100.2", None),
        ("198.51.100.1", Some("192.0.2.2")),
        ("203.0.113.7", Some("192.0.2.3")),
        ("192.0.2.4, 203.0.113.7", Some("192.0.2.5")),
    ] {
        let mut headers = vec![
            (header::USER_AGENT, "a browser"),
            (forwarded.clone(), chain),
        ];
        if let Some(address) = claimed {
            headers.push((client.clone(), address));
        }
        app.get_with(&page, &headers).await;
    }
    app.state.views.flush(&app.state.db, true).await;
    let stats = app.state.db.view_stats(&id, 0).await.unwrap();
    assert_eq!(stats.all.views, 6);
    assert_eq!(stats.all.visitors, 3);
}

#[tokio::test]
async fn views_are_counted_anonymously_and_go_with_their_image() {
    let app = app().await;
    let (album, done) = app.upload(&[&jpeg(300, 200)]).await;
    let id = image_id(&done[0]);
    let page = format!("/{id}");
    let forwarded = header::HeaderName::from_static("x-forwarded-for");
    let from = |address| {
        [
            (header::USER_AGENT, "a browser"),
            (forwarded.clone(), address),
            (header::REFERER, "https://forum.example/t/1?page=2"),
        ]
    };
    // One visitor twice, behind a proxy that appends their address; another
    // once; a link preview; and the owner, who is not counted.
    app.get_with(&page, &from("10.0.0.9, 192.0.2.1")).await;
    // The last of several header lines is the proxy's.
    app.get_with(
        &page,
        &[
            (header::USER_AGENT, "a browser"),
            (forwarded.clone(), "192.0.2.9"),
            (forwarded.clone(), "192.0.2.1"),
            (header::REFERER, "https://forum.example/t/1?page=2"),
        ],
    )
    .await;
    // A HEAD carries nothing and counts for nothing.
    app.send(
        Request::head(&page)
            .header(header::HOST, "app.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    app.get_with(&page, &from("192.0.2.2")).await;
    // A link preview: the page once, as a share, and its picture.
    let whatsapp = [(header::USER_AGENT, "WhatsApp/2.24")];
    app.get_with(&page, &whatsapp).await;
    app.get_with(&format!("/p/{id}.jpg"), &whatsapp).await;
    let owner = app.session("dev").await;
    app.get_as(&page, &owner).await;
    let part = app
        .get_with(
            &format!("/{id}.jpg"),
            &[
                (header::RANGE, "bytes=0-99"),
                (forwarded.clone(), "192.0.2.1"),
            ],
        )
        .await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    // The rest of that file, and the file as one of dither's pages asks for
    // it: neither is a view of its own.
    app.get_with(
        &format!("/{id}.jpg"),
        &[
            (header::RANGE, "bytes=100-"),
            (forwarded.clone(), "192.0.2.1"),
        ],
    )
    .await;
    app.get_with(
        &format!("/{id}.jpg"),
        &[
            (header::REFERER, "http://app.test/a/x"),
            (forwarded.clone(), "192.0.2.1"),
        ],
    )
    .await;
    // Safari's probe before it plays, and a revalidation: neither a view,
    // and a revalidation not even a hit.
    app.get_with(
        &format!("/{id}.jpg"),
        &[
            (header::RANGE, "bytes=0-1"),
            (forwarded.clone(), "192.0.2.1"),
        ],
    )
    .await;
    let unchanged = app
        .get_with(
            &format!("/{id}.jpg"),
            &[(header::IF_NONE_MATCH, part.header("etag"))],
        )
        .await;
    assert_eq!(unchanged.status, StatusCode::NOT_MODIFIED);
    app.state.views.flush(&app.state.db, true).await;

    let stats = app.state.db.view_stats(&id, 0).await.unwrap();
    // Three page views and the file asked for on its own.
    assert_eq!(stats.all.views, 4);
    // The range request carried no user agent: a visitor of its own.
    assert_eq!(stats.all.visitors, 3);
    assert_eq!(stats.recent, stats.all);
    assert_eq!(stats.shared, vec![("WhatsApp".to_string(), 1)]);
    assert_eq!(stats.referrers, vec![("forum.example".to_string(), 3)]);
    let album_stats = app.state.db.view_stats(&album, 0).await.unwrap();
    assert_eq!(album_stats.all, stats.all);
    let (image, _) = app.state.db.image(&id).await.unwrap().unwrap();
    let original: (i64, i64) = app
        .state
        .db
        .run({
            let id = id.clone();
            move |c| {
                c.query_row(
                    "SELECT hits, bytes FROM views WHERE target = ?1 AND kind = 'original'",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            }
        })
        .await
        .unwrap();
    // Two ranges that make the file once, the file whole, and the probe.
    assert_eq!(
        original,
        (4, 2 * image.bytes as i64 + 2),
        "ranges count their own lengths"
    );

    let viewer = app.get(&format!("/images/{id}/view")).await.text();
    assert!(viewer.contains("4 views"), "{viewer}");
    assert!(viewer.contains("forum.example 3"), "{viewer}");

    // An album's totals are its images' as they are now.
    let moved = app
        .post(
            "/images/move",
            json!({ "ids": [id], "to": null, "title": "elsewhere" }),
        )
        .await;
    let to = moved.json()["to"].as_str().unwrap().to_string();
    assert_eq!(
        app.state.db.view_stats(&album, 0).await.unwrap().all.views,
        0
    );
    assert_eq!(app.state.db.view_stats(&to, 0).await.unwrap().all.views, 4);

    // Counts gathered before a delete and written after it are dropped.
    app.get_with(&page, &from("192.0.2.3")).await;
    let gone = app
        .post(&format!("/images/delete?ids={id}"), json!({}))
        .await;
    assert!(gone.status.is_success(), "{}", gone.text());
    app.state.views.flush(&app.state.db, true).await;
    let stats = app.state.db.view_stats(&id, 0).await.unwrap();
    assert_eq!(stats, crate::views::Stats::default());
}
