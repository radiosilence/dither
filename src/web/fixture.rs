//! A service for the web layer's tests: the web process as `main` builds it,
//! with a worker beside it, against a store in memory and a state directory
//! of the test's own.

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
use crate::store::Store;
use crate::upload::Uploads;

pub(super) struct App {
    pub(super) router: Router,
    pub(super) state: AppState,
    /// The session of `dev`, an admin, which requests carry unless they name
    /// another.
    pub(super) cookie: String,
    pub(super) _dir: tempfile::TempDir,
}

/// The service as it runs: the web process, and a worker beside it over the
/// socket, sharing the state directory and the store.
pub(super) async fn app_with(edit: impl Fn(&mut Config)) -> App {
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

pub(super) async fn app() -> App {
    app_with(|_| {}).await
}

pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: Bytes,
}

impl Reply {
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub(super) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub(super) fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

impl App {
    pub(super) async fn send(&self, req: Request<Body>) -> Reply {
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

    pub(super) async fn get(&self, uri: &str) -> Reply {
        self.get_as(uri, &self.cookie).await
    }

    /// As someone signed in to nothing.
    pub(super) async fn get_anonymous(&self, uri: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(header::HOST, "app.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    pub(super) async fn get_as(&self, uri: &str, cookie: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(header::HOST, "app.test")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    pub(super) async fn write(
        &self,
        method: &str,
        uri: &str,
        body: Value,
        cookie: Option<&str>,
    ) -> Reply {
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

    pub(super) async fn post(&self, uri: &str, body: Value) -> Reply {
        self.write("POST", uri, body, None).await
    }

    /// A form posted as a `kit::Form` posts it: through Datastar, its fields
    /// form-encoded.
    pub(super) async fn submit(
        &self,
        uri: &str,
        fields: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> Reply {
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

    pub(super) async fn chunk(
        &self,
        upload: &str,
        bytes: &[u8],
        start: usize,
        size: usize,
    ) -> Reply {
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
    pub(super) async fn upload(&self, files: &[&[u8]]) -> (String, Vec<Reply>) {
        let album = self
            .post("/api/albums", json!({ "title": "trip" }))
            .await
            .json()["id"]
            .as_str()
            .unwrap()
            .to_string();
        (album.clone(), self.upload_to(&album, files).await)
    }

    pub(super) async fn upload_to(&self, album: &str, files: &[&[u8]]) -> Vec<Reply> {
        self.send_files(album, files, true).await
    }

    /// [`upload`](Self::upload), returning as soon as each file is queued.
    pub(super) async fn upload_queued(&self, files: &[&[u8]]) -> (String, Vec<Reply>) {
        let album = self
            .post("/api/albums", json!({ "title": "trip" }))
            .await
            .json()["id"]
            .as_str()
            .unwrap()
            .to_string();
        (album.clone(), self.send_files(&album, files, false).await)
    }

    pub(super) async fn send_files(&self, album: &str, files: &[&[u8]], wait: bool) -> Vec<Reply> {
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
    pub(super) async fn session(&self, name: &str) -> String {
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

pub(super) fn jpeg(w: u32, h: u32) -> Vec<u8> {
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
pub(super) fn located_jpeg() -> Vec<u8> {
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

/// Video `id`'s row once it is no longer processing, waiting up to two
/// minutes for the worker.
pub(super) async fn made(app: &App, id: &str) -> crate::db::Image {
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
    pub(super) async fn get_with(
        &self,
        uri: &str,
        headers: &[(header::HeaderName, &str)],
    ) -> Reply {
        let mut req = Request::get(uri).header(header::HOST, "app.test");
        for (name, value) in headers {
            req = req.header(name, *value);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }
}
