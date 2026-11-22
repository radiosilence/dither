//! The database, copied into the object store as it changes.
//!
//! The store is the one thing to keep: it holds every image and, beside them,
//! a compacted, gzipped copy of the database written a couple of seconds after
//! each burst of changes, plus one an hour for the last three days. A node
//! that starts with no database restores the latest copy, so with a bucket the
//! pod's own disk is a cache and losing it loses at most those seconds.
//!
//! Each copy carries a generation, bumped as it is taken. A database on disk
//! whose generation is behind the latest copy's is one an older run left (a
//! disk reattached after another run carried on elsewhere), and the copy
//! replaces it; the old file is kept beside it rather than deleted.
//!
//! One writer is assumed: the deployment runs a single replica and replaces
//! it rather than overlapping, as with any SQLite service.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use object_store::ObjectStoreExt as _;
use object_store::path::Path as Key;
use rusqlite::OptionalExtension as _;

use crate::db::Db;
use crate::store::Store;

const LATEST: &str = "state/dither.db.gz";
const HOURLY: &str = "state/hourly";
const KEEP_HOURLY: usize = 72;
/// How long after a change the copy is taken, so a burst of changes (an
/// upload of three hundred images) is one copy rather than three hundred.
const SETTLE: Duration = Duration::from_secs(2);

/// Put the latest copy at `path` when there is no database there, or when the
/// one there is behind it. True when it restored one.
pub async fn restore(store: &Store, path: &Path) -> Result<bool> {
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
    if path.exists() {
        let (here, copy) = (generation(path)?, generation(&tmp)?);
        if copy <= here {
            tokio::fs::remove_file(&tmp).await?;
            return Ok(false);
        }
        let aside = path.with_extension(format!("db.behind-{}", crate::db::now()));
        tracing::warn!(
            here,
            copy,
            kept = %aside.display(),
            "the database on disk is behind the store's latest copy; restoring the copy"
        );
        // The write-ahead log goes with its database: left behind, it would
        // be replayed into the copy.
        for (from, to) in sidecars(path).into_iter().zip(sidecars(&aside)) {
            if from.exists() {
                tokio::fs::rename(&from, &to).await?;
            }
        }
    }
    tokio::fs::rename(&tmp, path).await?;
    tracing::info!(bytes = db.len(), "restored the database from the store");
    Ok(true)
}

/// A database file and the write-ahead log and index SQLite keeps beside it.
fn sidecars(path: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [path.to_path_buf(), with("-wal"), with("-shm")]
}

/// The generation of the database at `path`: 0 for one that predates them.
fn generation(path: &Path) -> Result<i64> {
    let c =
        rusqlite::Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    let n = c
        .query_row("SELECT n FROM generation", [], |r| r.get(0))
        .optional()
        .unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "reading the database's generation failed");
            None
        });
    Ok(n.unwrap_or(0))
}

/// Write a copy now: the latest, and the hour's if it has none yet.
pub async fn snapshot(db: &Db, store: &Store) -> Result<()> {
    db.next_generation().await?;
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

/// Write a copy, recording how it went. True when it was written.
async fn copy(db: &Db, store: &Store) -> bool {
    match snapshot(db, store).await {
        Ok(()) => {
            crate::metrics::snapshot(true);
            true
        }
        Err(e) => {
            crate::metrics::snapshot(false);
            tracing::error!(error = %e, "database snapshot failed; retrying");
            false
        }
    }
}

/// Copy once at the start, then after every settled burst of writes until
/// `stop` fires, then once more if anything changed since.
pub async fn run(db: Db, store: Store, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut writes = db.writes();
    // The copy at the start holds whatever landed before this was watching,
    // and puts this run's generation in the store.
    let mut pending = !copy(&db, &store).await;
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
            pending = !copy(&db, &store).await;
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
        // A database as new as the copy is kept.
        assert!(!restore(&store, &path).await.unwrap());
    }

    #[tokio::test]
    async fn a_database_behind_the_latest_copy_is_replaced_and_kept_aside() {
        let store = Store::memory();
        let live = Db::temp().await;
        live.create_album("ann", "first").await.unwrap();
        snapshot(&live, &store).await.unwrap();
        // Another disk, left holding that copy.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dither.db");
        assert!(restore(&store, &path).await.unwrap());
        // The live database carries on and is copied again.
        let later = live.create_album("ann", "later").await.unwrap();
        snapshot(&live, &store).await.unwrap();
        // The old disk comes back: it is behind, so the copy replaces it.
        assert!(restore(&store, &path).await.unwrap());
        let restored = Db::open(&path).await.unwrap();
        assert!(restored.album(&later.id).await.unwrap().is_some());
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".behind-")
        }));
        // The live database is as new as the copy, and stays.
        assert!(!restore(&store, live.path()).await.unwrap());
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
