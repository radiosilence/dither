//! dither: an image host. Albums of images with captions, behind the estate's
//! sign-in, shared by unlisted links that preview anywhere a link can.

mod auth;
mod config;
mod db;
mod disk;
mod error;
mod hex;
mod i18n;
mod icon;
mod id;
mod jobs;
mod media;
mod metrics;
mod mp4;
mod sandbox;
mod sanitize;
mod snapshot;
mod store;
mod upload;
mod video;
mod views;
mod web;
mod worker;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Router;
use axum::routing::get;
use tokio::sync::watch;

use crate::config::Config;
use crate::db::Db;
use crate::store::Store;
use crate::upload::Uploads;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("serve") => serve().await,
        // Make uploads' copies, for a `serve` reached at QUEUE_URL.
        Some("worker") => worker::run(&Config::for_worker()?).await,
        // Move every image and the database from one store to another: how a
        // disk store becomes a bucket. Safe to repeat; it resumes.
        Some("copy-store") => {
            let (Some(from), Some(to)) = (args.next(), args.next()) else {
                bail!("usage: dither copy-store <from-url> <to-url>");
            };
            let (from, to) = (Store::open_any(&from)?, Store::open_any(&to)?);
            let (copied, skipped) = store::copy(&from, &to).await?;
            println!("copied {copied}, already there {skipped}");
            Ok(())
        }
        Some("version" | "--version") => {
            println!("dither {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            bail!(
                "unknown command {other}: dither [serve | worker | copy-store <from> <to> | version]"
            )
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    std::fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("creating {}", config.state_dir.display()))?;
    let mut store = Store::open(&config.storage, config.cache_bytes)?;
    if store.signs() && config.disk_cache_bytes > 0 {
        let dir = config.state_dir.join("cache");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // Above the floor uploads keep, by room for the largest upload: what a
        // worker on the same disk writes while making one, since it cannot
        // ask this process's cache for room. An upload here takes room back
        // from the cache as it arrives (`Uploads::room_for`).
        let uploads = config.min_free_bytes.unwrap_or_else(|| {
            store::disk(&dir).map_or(0, |(_, total)| {
                (total as f64 * store::DEFAULT_FREE_SHARE) as u64
            })
        });
        let largest = config
            .max_upload_bytes
            .max(config.max_video_bytes().unwrap_or(0));
        let floor = uploads + largest;
        let budget = config.disk_cache_bytes;
        let disk = tokio::task::spawn_blocking(move || disk::Disk::open(&dir, budget, floor))
            .await?
            .context("opening the disk cache")?;
        store = store.with_disk(disk);
    }
    let db_path = config.state_dir.join("dither.db");
    snapshot::restore(&store, &db_path)
        .await
        .context("restoring the database")?;
    let db = Db::open(&db_path).await.context("opening the database")?;
    let jobs = Arc::new(jobs::Hub::new(db.clone(), store.clone(), &config)?);
    jobs.adopt(&config.state_dir);
    {
        let listener = tokio::net::TcpListener::bind(&config.worker_addr)
            .await
            .with_context(|| format!("binding {} for workers", config.worker_addr))?;
        tracing::info!(addr = %config.worker_addr, "the worker API");
        let router = web::work::router(web::work::Work {
            hub: jobs.clone(),
            token: config.worker_token.as_deref().map(web::work::digest),
            limit: usize::try_from(4 * config.max_upload_bytes.max(config.video.max_bytes))
                .unwrap_or(usize::MAX),
        });
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "the worker API stopped");
            }
        });
    }
    let uploads = Arc::new(Uploads::new(db.clone(), store.clone(), &config, jobs)?);

    let (snapshot_stop, snapshot_rx) = watch::channel(false);
    let snapshots = tokio::spawn(snapshot::run(db.clone(), store.clone(), snapshot_rx));
    {
        let uploads = uploads.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(300)).await;
            loop {
                uploads.sweep().await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }

    let (stopping_tx, stopping) = watch::channel(false);
    let state = web::AppState::new(web::Inner {
        sessions: auth::session::Sessions::new(db.clone()),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("building the HTTP client")?,
        db: db.clone(),
        store: store.clone(),
        uploads: uploads.clone(),
        views: views::Views::default(),
        stopping,
        config,
    });
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                state.views.flush(&state.db, false).await;
            }
        });
    }

    if let Some(addr) = state.config.metrics_addr.clone() {
        let state = state.clone();
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!(%addr, "metrics at /metrics");
        let router = Router::new().route(
            "/metrics",
            get(move || async move {
                let (albums, images, bytes) = state.db.totals().await.unwrap_or_default();
                metrics::render(&metrics::Totals {
                    albums,
                    images,
                    bytes,
                    free: store::disk(&state.config.state_dir).map(|(free, _)| free),
                })
            }),
        );
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "the metrics listener stopped");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&state.config.bind_addr)
        .await
        .with_context(|| format!("binding {}", state.config.bind_addr))?;
    tracing::info!(
        addr = %state.config.bind_addr,
        app = %state.config.app_url,
        public = %state.config.public_url,
        storage = %store.label(),
        version = env!("CARGO_PKG_VERSION"),
        "serving"
    );
    let router = web::router(state.clone());
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        signal().await;
        tracing::info!("stopping");
        // Live streams end, so the graceful shutdown is not held open by
        // a browser tab.
        let _ = stopping_tx.send(true);
    })
    .await?;

    // The last counts are written and an upload being processed finishes,
    // then the database is copied a last time, so nothing acknowledged is
    // lost with the pod.
    state.views.flush(&state.db, true).await;
    uploads.drain(Duration::from_secs(30)).await;
    let _ = snapshot_stop.send(true);
    let _ = snapshots.await;
    tracing::info!("stopped");
    Ok(())
}

/// A request to stop: Ctrl-C, or SIGTERM from Kubernetes.
pub(crate) async fn signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = interrupt => {}
        () = terminate => {}
    }
}
