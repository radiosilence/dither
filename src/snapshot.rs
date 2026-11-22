//! The database, copied into the object store as it changes.
//!
//! The store is the one thing to keep: it holds every image and, beside them,
//! a compacted, gzipped copy of the database written a couple of seconds after
//! each burst of changes, plus one an hour for the last three days. A node
//! that starts with no database restores the latest copy, so with a bucket the
//! pod's own disk is a cache and losing it loses at most those seconds.
//!
//! One writer is assumed: the deployment runs a single replica and replaces
//! it rather than overlapping, as with any SQLite service.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use object_store::ObjectStoreExt as _;
use object_store::path::Path as Key;

use crate::db::Db;
use crate::store::Store;

const LATEST: &str = "state/dither.db.gz";
const HOURLY: &str = "state/hourly";
const KEEP_HOURLY: usize = 72;
/// How long after a change the copy is taken, so a burst of changes (an
/// upload of three hundred images) is one copy rather than three hundred.
const SETTLE: Duration = Duration::from_secs(2);

/// Put the latest copy at `path` when there is no database there. True when
/// it restored one.
pub async fn restore(store: &Store, path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let Some(gz) = store.read(&Key::from(LATEST)).await? else {
        return Ok(false);
    };
    let db = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut out)?;
        Ok(out)
    })
    .await??;
    let tmp = path.with_extension("restoring");
    tokio::fs::write(&tmp, &db)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path).await?;
    tracing::info!(bytes = db.len(), "restored the database from the store");
    Ok(true)
}

/// Write a copy now: the latest, and the hour's if it has none yet.
pub async fn snapshot(db: &Db, store: &Store) -> Result<()> {
    let tmp = db.path().with_extension("snapshot");
    let _ = tokio::fs::remove_file(&tmp).await;
    let into = tmp.to_string_lossy().into_owned();
    // VACUUM INTO reads one consistent view, beside writers, into a compact
    // new file.
    db.run(move |c| c.execute("VACUUM INTO ?1", [into]).map(drop))
        .await?;
    let raw = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;
    let gz = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw)?;
        enc.finish()
    })
    .await??;
    store
        .inner()
        .put(&Key::from(LATEST), gz.clone().into())
        .await?;

    let hour = crate::db::now() / 3_600_000;
    let hourly = Key::from(format!("{HOURLY}/{hour:010}.db.gz"));
    if store.inner().head(&hourly).await.is_err() {
        store.inner().put(&hourly, gz.into()).await?;
        prune(store).await?;
    }
    Ok(())
}

/// The oldest hourly copies past [`KEEP_HOURLY`]. Their names sort by hour.
async fn prune(store: &Store) -> Result<()> {
    let mut keys: Vec<Key> = store
        .inner()
        .list(Some(&Key::from(HOURLY)))
        .filter_map(|m| async move { m.ok().map(|m| m.location) })
        .collect()
        .await;
    keys.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
    let excess = keys.len().saturating_sub(KEEP_HOURLY);
    for key in &keys[..excess] {
        store.delete(key).await?;
    }
    Ok(())
}

/// Copy after every settled burst of writes until `stop` fires, then once
/// more if anything changed since.
pub async fn run(db: Db, store: Store, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut writes = db.writes();
    writes.borrow_and_update();
    let mut pending = false;
    loop {
        tokio::select! {
            changed = writes.changed() => {
                if changed.is_err() {
                    break;
                }
                pending = true;
                tokio::select! {
                    () = tokio::time::sleep(SETTLE) => {}
                    _ = stop.changed() => {}
                }
            }
            _ = stop.changed() => {}
            // A copy that failed is tried again without waiting for a write.
            () = tokio::time::sleep(Duration::from_secs(10)), if pending => {}
        }
        // Writes can land while stopping was the branch taken.
        if writes.has_changed().unwrap_or(false) {
            pending = true;
        }
        if pending {
            writes.borrow_and_update();
            match snapshot(&db, &store).await {
                Ok(()) => {
                    pending = false;
                    crate::metrics::snapshot(true);
                }
                Err(e) => {
                    crate::metrics::snapshot(false);
                    tracing::error!(error = %e, "database snapshot failed; retrying");
                }
            }
        }
        if *stop.borrow() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_snapshot_restores_into_an_empty_directory() {
        let db = Db::temp().await;
        let store = Store::memory();
        let album = db.create_album("ann", "holiday").await.unwrap();
        snapshot(&db, &store).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dither.db");
        assert!(restore(&store, &path).await.unwrap());
        let restored = Db::open(&path).await.unwrap();
        assert_eq!(
            restored.album(&album.id).await.unwrap().unwrap().title,
            "holiday"
        );
        // An existing database is never overwritten.
        assert!(!restore(&store, &path).await.unwrap());
    }

    #[tokio::test]
    async fn nothing_to_restore_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !restore(&Store::memory(), &dir.path().join("dither.db"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn hourly_copies_are_kept_to_three_days() {
        let store = Store::memory();
        for hour in 0..(KEEP_HOURLY + 5) {
            let key = Key::from(format!("{HOURLY}/{hour:010}.db.gz"));
            store.inner().put(&key, vec![0u8].into()).await.unwrap();
        }
        prune(&store).await.unwrap();
        let left: Vec<_> = store
            .inner()
            .list(Some(&Key::from(HOURLY)))
            .map(|m| m.unwrap().location.to_string())
            .collect()
            .await;
        assert_eq!(left.len(), KEEP_HOURLY);
        assert!(!left.iter().any(|k| k.ends_with("0000000000.db.gz")));
    }

    #[tokio::test]
    async fn the_loop_copies_after_a_write_and_on_stop() {
        let db = Db::temp().await;
        let store = Store::memory();
        let (stop_tx, stop) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run(db.clone(), store.clone(), stop));
        db.create_album("ann", "x").await.unwrap();
        stop_tx.send(true).unwrap();
        task.await.unwrap();
        assert!(store.read(&Key::from(LATEST)).await.unwrap().is_some());
    }
}
