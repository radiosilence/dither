//! dither: an image host. Albums of images with captions, behind the estate's
//! sign-in, shared by unlisted links that preview anywhere a link can.

mod auth;
mod config;
mod db;
mod error;
mod i18n;
mod icon;
mod id;
mod media;
mod metrics;
mod sanitize;
mod snapshot;
mod store;
mod upload;
mod video;
mod web;

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
        // Move every image and the database from one store to another: how a
        // disk store becomes a bucket. Safe to repeat; it resumes.
        Some("copy-store") => {
            let (Some(from), Some(to)) = (args.next(), args.next()) else {
                bail!("usage: dither copy-store <from-url> <to-url>");
            };
            let (from, to) = (Store::open(&from, 0)?, Store::open(&to, 0)?);
            let (copied, skipped) = store::copy(&from, &to).await?;
            println!("copied {copied}, already there {skipped}");
            Ok(())
        }
        Some("version" | "--version") => {
            println!("dither {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            bail!("unknown command {other}: dither [serve | copy-store <from> <to> | version]")
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    std::fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("creating {}", config.state_dir.display()))?;
    let store = Store::open(&config.storage, config.cache_bytes)?;
    let db_path = config.state_dir.join("dither.db");
    snapshot::restore(&store, &db_path)
        .await
        .context("restoring the database")?;
    let db = Db::open(&db_path).await.context("opening the database")?;
    let uploads = Arc::new(Uploads::new(
        db.clone(),
        store.clone(),
        &config,
        video::Tools::find(&config.video).await?,
    )?);
    uploads.videos.start();

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
        stopping,
        config,
    });

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
                    free: state.store.free_bytes(),
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
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            signal().await;
            tracing::info!("stopping");
            // Live streams end, so the graceful shutdown is not held open by
            // a browser tab.
            let _ = stopping_tx.send(true);
        })
        .await?;

    // An upload being processed finishes, then the database is copied a
    // last time, so nothing acknowledged is lost with the pod.
    uploads.drain(Duration::from_secs(30)).await;
    let _ = snapshot_stop.send(true);
    let _ = snapshots.await;
    tracing::info!("stopped");
    Ok(())
}

async fn signal() {
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
