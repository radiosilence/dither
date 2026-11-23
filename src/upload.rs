//! Uploads: chunks into a file in the state directory, then processing into
//! the store.
//!
//! A file arrives in chunks of at most [`CHUNK`], each a request of its own,
//! so a large photo on a phone's connection never holds one request open past
//! a proxy's timeout, and a dropped connection resumes from what arrived. Once
//! every byte is in, `finish` cleans and resizes it under a budget of memory
//! for processing at once, which is what bounds what a batch of three hundred
//! photos can take, writes the copies to the store, and only then adds the
//! image's row: a row never names a file that is not there.
//!
//! The copies are recorded as strays before they are written (see the
//! `strays` table), and recording the image claims them. Copies a failure or
//! a crash strands are ones the database knows it made, which the sweep
//! removes; it never deletes a copy merely because no row names it, so a
//! database restored from an older copy leaves newer images alone.
//!
//! Processing runs in a task of its own that the request only waits on, so a
//! client that disconnects mid-way does not stop it, and asking again joins
//! the run already going or reads how it ended.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{FutureExt as _, Stream, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore, watch};

use crate::db::{Db, NewImage, Upload};
use crate::media;
use crate::store::{self, Store, Variant};

/// The share of a disk uploads leave free when no floor is set.
const DEFAULT_FREE_SHARE: f64 = 0.15;
/// The processing budget's unit.
const MIB: u64 = 1 << 20;
/// What processing reads of a file to size it before holding any of the
/// budget: every header dither takes ends well within it.
const HEAD: u64 = 4 << 20;

/// The largest chunk a client sends. Traefik reads a request's body within
/// 60 seconds by default, so a chunk this size arrives in time on anything
/// faster than about 0.6 Mbit/s, which a phone on a poor signal still
/// manages.
pub const CHUNK: u64 = 4 << 20;
const DAY: i64 = 24 * 3600 * 1000;

/// How processing an upload ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The image, by id.
    Done(String),
    /// The file is not something dither can host; sending it again won't help.
    Rejected(String),
    /// Something on this side failed; finishing again may work.
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChunkError {
    /// The chunk does not start where the upload has reached.
    Conflict(u64),
    /// More bytes than the upload declared, or than the chunk said.
    TooLong,
    /// The disk is nearly full.
    Full,
    /// The upload has already finished, failed or been cancelled.
    Ended,
    /// The body stopped short or broke off; sending the chunk again works.
    Broken(String),
    /// Saving it here failed.
    Io(String),
}

type Locks = parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>;

pub struct Uploads {
    db: Db,
    store: Store,
    dir: PathBuf,
    /// Memory for processing, in MiB.
    budget: Arc<Semaphore>,
    budget_bytes: u64,
    max_pixels: u64,
    /// Free space kept on the disks uploads write to; `None` keeps a share of
    /// each disk instead ([`DEFAULT_FREE_SHARE`]).
    min_free: Option<u64>,
    locks: Locks,
    running: parking_lot::Mutex<HashMap<String, watch::Receiver<Option<Outcome>>>>,
}

/// An upload's lock, held until this drops. A lock lives in the map only
/// while someone holds or waits for it.
struct Held<'a> {
    guard: Option<OwnedMutexGuard<()>>,
    lock: Arc<Mutex<()>>,
    id: &'a str,
    locks: &'a Locks,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.guard.take();
        let mut locks = self.locks.lock();
        // The map's reference and this one: nobody else holds or waits.
        if Arc::strong_count(&self.lock) == 2 {
            locks.remove(self.id);
        }
    }
}

impl Uploads {
    pub fn new(
        db: Db,
        store: Store,
        state_dir: &Path,
        process_memory: u64,
        max_pixels: u64,
        min_free: Option<u64>,
    ) -> std::io::Result<Self> {
        let dir = state_dir.join("incoming");
        std::fs::create_dir_all(&dir)?;
        // Whole MiB, and no more than one acquire can ask for.
        let mib = (process_memory / MIB).clamp(1, u64::from(u32::MAX));
        Ok(Self {
            db,
            store,
            dir,
            budget: Arc::new(Semaphore::new(mib as usize)),
            budget_bytes: mib * MIB,
            max_pixels,
            min_free,
            locks: Default::default(),
            running: Default::default(),
        })
    }

    /// Where an upload's bytes collect. Callers pass ids checked by
    /// [`crate::id::valid`], so this cannot name anything outside the
    /// directory.
    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    async fn lock<'a>(&'a self, id: &'a str) -> Held<'a> {
        let lock = self.locks.lock().entry(id.to_string()).or_default().clone();
        Held {
            guard: Some(lock.clone().lock_owned().await),
            lock,
            id,
            locks: &self.locks,
        }
    }

    /// Whether `needed` more bytes fit with room to spare, on the disk the
    /// chunks land on and, for a file store, the one it writes to. Each disk
    /// keeps `min_free`, or by default a share of its size: the kubelet
    /// starts evicting at 10% free, and the machine's other services share
    /// the disk.
    pub fn room_for(&self, needed: u64) -> bool {
        let disks = [Some(self.dir.as_path()), self.store.dir()];
        disks.into_iter().flatten().all(|dir| {
            let Some((free, total)) = store::disk(dir) else {
                return true;
            };
            let keep = self
                .min_free
                .unwrap_or((total as f64 * DEFAULT_FREE_SHARE) as u64);
            free >= keep.saturating_add(needed)
        })
    }

    /// Append a chunk that starts at `start` and is `len` bytes long. Returns
    /// how much of the file has arrived.
    pub async fn append<S, E>(
        &self,
        upload: &Upload,
        start: u64,
        len: u64,
        body: S,
    ) -> Result<u64, ChunkError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        let _held = self.lock(&upload.id).await;
        // Read again under the lock: a retry, or a cancel, may have landed
        // meanwhile.
        let current = self
            .db
            .upload(&upload.id)
            .await
            .map_err(|e| ChunkError::Io(e.to_string()))?
            .ok_or(ChunkError::Ended)?;
        if current.image.is_some() || current.error.is_some() {
            return Err(ChunkError::Ended);
        }
        if start != current.received {
            return Err(ChunkError::Conflict(current.received));
        }
        if start + len > current.size || len > CHUNK {
            return Err(ChunkError::TooLong);
        }
        if !self.room_for(current.size - start) {
            return Err(ChunkError::Full);
        }
        let path = self.file(&upload.id);
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .await
            .map_err(|e| ChunkError::Io(e.to_string()))?;
        // The database counts what arrived and the file holds it. A file short
        // of the count (a disk restored without it, or a crash before the
        // bytes reached it) resumes from what it holds, rather than the gap
        // being filled with zeros.
        let held = file
            .metadata()
            .await
            .map_err(|e| ChunkError::Io(e.to_string()))?
            .len();
        if held < start {
            self.db
                .set_received(&upload.owner, &upload.id, held)
                .await
                .map_err(|e| ChunkError::Io(e.to_string()))?;
            return Err(ChunkError::Conflict(held));
        }
        // Whatever a broken earlier attempt left past the recorded length goes.
        file.set_len(start)
            .await
            .map_err(|e| ChunkError::Io(e.to_string()))?;
        let mut file = tokio::io::BufWriter::new({
            let mut f = file;
            tokio::io::AsyncSeekExt::seek(&mut f, std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| ChunkError::Io(e.to_string()))?;
            f
        });
        let mut written = 0u64;
        let mut body = body;
        while let Some(frame) = body.next().await {
            let frame = frame.map_err(|e| ChunkError::Broken(e.to_string()))?;
            written += frame.len() as u64;
            if written > len {
                return Err(ChunkError::TooLong);
            }
            file.write_all(&frame)
                .await
                .map_err(|e| ChunkError::Io(e.to_string()))?;
        }
        if written != len {
            return Err(ChunkError::Broken(format!(
                "the chunk ended after {written} of {len} bytes"
            )));
        }
        file.flush()
            .await
            .map_err(|e| ChunkError::Io(e.to_string()))?;
        let received = start + written;
        let recorded = self
            .db
            .set_received(&upload.owner, &upload.id, received)
            .await
            .map_err(|e| ChunkError::Io(e.to_string()))?;
        if !recorded {
            return Err(ChunkError::Ended);
        }
        Ok(received)
    }

    /// Process a complete upload, or join the run already going, or read how
    /// it ended.
    pub async fn finish(self: &Arc<Self>, upload: Upload) -> Outcome {
        if let Some(image) = upload.image {
            return Outcome::Done(image);
        }
        if let Some(error) = upload.error {
            return Outcome::Rejected(error);
        }
        let mut rx = {
            let mut running = self.running.lock();
            match running.get(&upload.id) {
                Some(rx) => rx.clone(),
                None => {
                    let (tx, rx) = watch::channel(None);
                    running.insert(upload.id.clone(), rx.clone());
                    let this = self.clone();
                    tokio::spawn(async move {
                        let id = upload.id.clone();
                        // A panic still ends the run: left in `running`, the
                        // upload could never be finished again, and every
                        // shutdown would wait out the drain for it.
                        let outcome = std::panic::AssertUnwindSafe(this.process(upload))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| {
                                tracing::error!(upload = %id, "processing panicked");
                                Outcome::Failed(
                                    "the image could not be processed; try again".into(),
                                )
                            });
                        this.running.lock().remove(&id);
                        let _ = tx.send(Some(outcome));
                    });
                    rx
                }
            }
        };
        match rx.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().expect("waited for an outcome"),
            Err(_) => Outcome::Failed("processing stopped; try again".into()),
        }
    }

    async fn reject(&self, upload: &Upload, why: String) -> Outcome {
        crate::metrics::rejected();
        let _ = tokio::fs::remove_file(self.file(&upload.id)).await;
        if let Err(e) = self.db.fail_upload(&upload.owner, &upload.id, &why).await {
            tracing::warn!(error = %e, "could not record a rejected upload");
        }
        tracing::info!(upload = %upload.id, reason = %why, "upload refused");
        Outcome::Rejected(why)
    }

    /// The file is not what the database counts: count again from the file,
    /// so the client sends the rest.
    async fn recount(&self, upload: &Upload) -> Outcome {
        let _held = self.lock(&upload.id).await;
        let held = tokio::fs::metadata(self.file(&upload.id))
            .await
            .map_or(0, |m| m.len())
            .min(upload.size);
        if let Err(e) = self.db.set_received(&upload.owner, &upload.id, held).await {
            tracing::warn!(upload = %upload.id, error = %e, "recounting an upload failed");
        }
        Outcome::Failed("the upload is incomplete; send it again".into())
    }

    async fn process(&self, upload: Upload) -> Outcome {
        // Read again: a run that finished first, or a cancel, may have ended
        // it since this request read it.
        match self.db.upload(&upload.id).await {
            Ok(Some(u)) => {
                if let Some(image) = u.image {
                    return Outcome::Done(image);
                }
                if let Some(error) = u.error {
                    return Outcome::Rejected(error);
                }
            }
            Ok(None) => return Outcome::Rejected("its album was deleted while it uploaded".into()),
            Err(_) => return Outcome::Failed("the image could not be saved; try again".into()),
        }
        let started = Instant::now();
        let path = self.file(&upload.id);
        // Sized from its first bytes, so the files waiting for the budget are
        // not held in memory beside it.
        let mut head = Vec::new();
        let read = async {
            use tokio::io::AsyncReadExt as _;
            tokio::fs::File::open(&path)
                .await?
                .take(HEAD)
                .read_to_end(&mut head)
                .await
        };
        if read.await.is_err() || (head.len() as u64) < upload.size.min(HEAD) {
            return self.recount(&upload).await;
        }
        let cost = match media::cost(&head, upload.size, self.max_pixels) {
            Ok(cost) => cost,
            // A header longer than the first bytes: size it from the whole file.
            Err(media::Reject::Broken(_)) if (head.len() as u64) < upload.size => {
                match tokio::fs::read(&path).await {
                    Ok(all) => {
                        head = all;
                        match media::cost(&head, upload.size, self.max_pixels) {
                            Ok(cost) => cost,
                            Err(reject) => return self.reject(&upload, reject.to_string()).await,
                        }
                    }
                    Err(_) => return self.recount(&upload).await,
                }
            }
            Err(reject) => return self.reject(&upload, reject.to_string()).await,
        };
        if cost > self.budget_bytes {
            let why = format!(
                "the image needs about {} of memory to process, and this server allows {}",
                crate::web::human(cost),
                crate::web::human(self.budget_bytes)
            );
            return self.reject(&upload, why).await;
        }
        let processed = {
            let permits = u32::try_from(cost.div_ceil(MIB)).unwrap_or(u32::MAX).max(1);
            let Ok(_permit) = self.budget.acquire_many(permits).await else {
                return Outcome::Failed("shutting down".into());
            };
            let bytes = if head.len() as u64 == upload.size {
                head
            } else {
                match tokio::fs::read(&path).await {
                    Ok(b) if b.len() as u64 == upload.size => b,
                    _ => return self.recount(&upload).await,
                }
            };
            let max = self.max_pixels;
            match tokio::task::spawn_blocking(move || media::process(&bytes, max)).await {
                Ok(Ok(p)) => p,
                Ok(Err(reject)) => return self.reject(&upload, reject.to_string()).await,
                Err(e) => {
                    tracing::error!(upload = %upload.id, error = %e, "processing panicked");
                    return self
                        .reject(&upload, "the image could not be processed".into())
                        .await;
                }
            }
        };

        let id = crate::id::new();
        if let Err(e) = self.db.add_stray(&id).await {
            tracing::error!(upload = %upload.id, error = %e, "recording an upload's copies failed");
            return Outcome::Failed("the image could not be saved; try again".into());
        }
        let size = processed.original.len() as u64;
        let mut puts = vec![
            (Variant::Original, Bytes::from(processed.original)),
            (Variant::Thumb, Bytes::from(processed.thumb)),
            (Variant::Preview, Bytes::from(processed.preview)),
        ];
        let display = processed.display.is_some();
        if let Some(d) = processed.display {
            puts.push((Variant::Display, Bytes::from(d)));
        }
        for (variant, bytes) in puts {
            if let Err(e) = self.store.put(&store::key(variant, &id), bytes).await {
                tracing::error!(upload = %upload.id, error = %e, "storing an upload failed");
                store::discard(&self.store, &self.db, vec![id]).await;
                return Outcome::Failed("the image could not be stored; try again".into());
            }
        }
        let image = NewImage {
            id: id.clone(),
            kind: processed.kind,
            width: processed.width,
            height: processed.height,
            bytes: size,
            animated: processed.animated,
            display,
            sha256: processed.sha256.to_vec(),
        };
        match self
            .db
            .finish_upload(&upload.owner, &upload.id, image)
            .await
        {
            Ok(true) => {
                let _ = tokio::fs::remove_file(&path).await;
                let took = started.elapsed();
                crate::metrics::uploaded(size, took);
                tracing::info!(image = %id, bytes = size, ms = took.as_millis() as u64, "stored");
                Outcome::Done(id)
            }
            Ok(false) => {
                store::discard(&self.store, &self.db, vec![id]).await;
                let why = match self.db.upload(&upload.id).await {
                    // Still open: the sweep took the copies' record first,
                    // which only an hour between storing and recording
                    // allows. Finishing again works.
                    Ok(Some(u)) if u.image.is_none() && u.error.is_none() => {
                        return Outcome::Failed("the image could not be saved; try again".into());
                    }
                    Ok(Some(u)) => u.error.unwrap_or_else(|| "it ended another way".into()),
                    Ok(None) => "its album was deleted while it uploaded".into(),
                    Err(_) => {
                        return Outcome::Failed("the image could not be saved; try again".into());
                    }
                };
                // Cancelled while it was processed, or its album deleted.
                let _ = tokio::fs::remove_file(&path).await;
                Outcome::Rejected(why)
            }
            Err(e) => {
                tracing::error!(upload = %upload.id, error = %e, "recording an upload failed");
                store::discard(&self.store, &self.db, vec![id]).await;
                Outcome::Failed("the image could not be saved; try again".into())
            }
        }
    }

    /// Wait, up to `limit`, for processing in progress to finish.
    pub async fn drain(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !self.running.lock().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Give up on an upload that has not finished.
    pub async fn cancel(&self, upload: &Upload) -> crate::db::Result<()> {
        let _held = self.lock(&upload.id).await;
        let _ = tokio::fs::remove_file(self.file(&upload.id)).await;
        self.db
            .fail_upload(&upload.owner, &upload.id, "cancelled")
            .await
    }

    /// Tidy up: uploads abandoned for a day, files no upload names, and
    /// stored copies recorded as strays an hour ago that no image claimed (a
    /// crash between storing and recording, or a delete whose copies
    /// outlived it).
    pub async fn sweep(&self) {
        self.sweep_older_than(Duration::from_secs(3600)).await;
    }

    /// [`sweep`](Self::sweep), taking strays older than `age`: younger ones
    /// may belong to an upload about to record them.
    async fn sweep_older_than(&self, age: Duration) {
        match self.db.sweep_uploads(crate::db::now() - DAY).await {
            Ok(ids) => {
                for id in &ids {
                    let _ = tokio::fs::remove_file(self.file(id)).await;
                }
                if !ids.is_empty() {
                    tracing::info!(count = ids.len(), "swept abandoned uploads");
                }
            }
            Err(e) => tracing::warn!(error = %e, "sweeping uploads failed"),
        }
        if let Ok(mut entries) = tokio::fs::read_dir(&self.dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                let stale = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(24 * 3600));
                if stale && matches!(self.db.upload(&name).await, Ok(None)) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
        match self
            .db
            .claim_strays(crate::db::now() - age.as_millis() as i64)
            .await
        {
            Ok(ids) => {
                if !ids.is_empty() {
                    tracing::info!(count = ids.len(), "removing stored copies no image claimed");
                }
                for id in ids {
                    // Claimed, so the record is gone: one the store would not
                    // delete is recorded again for the next sweep.
                    if !self.store.delete_image(&id).await
                        && let Err(e) = self.db.add_stray(&id).await
                    {
                        tracing::warn!(image = %id, error = %e, "recording stored copies again failed");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "sweeping stored copies failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    pub fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 0]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut out)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    async fn setup() -> (Arc<Uploads>, Db, Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::temp().await;
        let store = Store::memory();
        let uploads = Arc::new(
            Uploads::new(
                db.clone(),
                store.clone(),
                dir.path(),
                1 << 30,
                50_000_000,
                Some(0),
            )
            .unwrap(),
        );
        (uploads, db, store, dir)
    }

    fn chunks(bytes: &[u8]) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Unpin {
        futures_util::stream::iter(
            bytes
                .chunks(1000)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
    }

    async fn begin(db: &Db, size: u64) -> Upload {
        let album = db.create_album("ann", "").await.unwrap();
        db.begin_uploads("ann", &album.id, vec![("a.jpg".into(), size)])
            .await
            .unwrap()
            .unwrap()
            .remove(0)
    }

    #[tokio::test]
    async fn chunks_resume_and_a_complete_file_becomes_an_image() {
        let (uploads, db, store, _dir) = setup().await;
        let file = jpeg(300, 200);
        let upload = begin(&db, file.len() as u64).await;
        let half = file.len() / 2;
        assert_eq!(
            uploads
                .append(&upload, 0, half as u64, chunks(&file[..half]))
                .await,
            Ok(half as u64)
        );
        // A retry of the first chunk is told where to resume.
        assert_eq!(
            uploads
                .append(&upload, 0, half as u64, chunks(&file[..half]))
                .await,
            Err(ChunkError::Conflict(half as u64))
        );
        let rest = (file.len() - half) as u64;
        assert_eq!(
            uploads
                .append(&upload, half as u64, rest, chunks(&file[half..]))
                .await,
            Ok(file.len() as u64)
        );
        let upload = db.upload(&upload.id).await.unwrap().unwrap();
        let Outcome::Done(id) = uploads.finish(upload.clone()).await else {
            panic!("expected an image");
        };
        let (image, _) = db.image(&id).await.unwrap().unwrap();
        assert_eq!((image.width, image.height), (300, 200));
        assert!(
            store
                .read(&store::key(Variant::Original, &id))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .read(&store::key(Variant::Thumb, &id))
                .await
                .unwrap()
                .is_some()
        );
        // Finishing again reads the outcome rather than storing it twice.
        let again = db.upload(&upload.id).await.unwrap().unwrap();
        assert_eq!(uploads.finish(again).await, Outcome::Done(id));
        assert!(!uploads.file(&upload.id).exists());
    }

    #[tokio::test]
    async fn a_chunk_longer_than_it_said_is_refused() {
        let (uploads, db, _, _dir) = setup().await;
        let upload = begin(&db, 10).await;
        assert_eq!(
            uploads.append(&upload, 0, 4, chunks(b"0123456789")).await,
            Err(ChunkError::TooLong)
        );
        assert_eq!(
            uploads.append(&upload, 0, 11, chunks(b"01234567890")).await,
            Err(ChunkError::TooLong)
        );
    }

    #[tokio::test]
    async fn a_file_that_is_not_an_image_is_rejected_for_good() {
        let (uploads, db, _, _dir) = setup().await;
        let body = b"<svg onload=alert(1)>";
        let upload = begin(&db, body.len() as u64).await;
        uploads
            .append(&upload, 0, body.len() as u64, chunks(body))
            .await
            .unwrap();
        let upload = db.upload(&upload.id).await.unwrap().unwrap();
        let Outcome::Rejected(why) = uploads.finish(upload.clone()).await else {
            panic!("expected a rejection");
        };
        assert!(why.contains("SVG"));
        let upload = db.upload(&upload.id).await.unwrap().unwrap();
        assert_eq!(uploads.finish(upload).await, Outcome::Rejected(why));
    }

    #[tokio::test]
    async fn concurrent_finishes_store_one_image() {
        let (uploads, db, _, _dir) = setup().await;
        let file = jpeg(64, 64);
        let upload = begin(&db, file.len() as u64).await;
        uploads
            .append(&upload, 0, file.len() as u64, chunks(&file))
            .await
            .unwrap();
        let upload = db.upload(&upload.id).await.unwrap().unwrap();
        let (a, b) = tokio::join!(
            uploads.finish(upload.clone()),
            uploads.finish(upload.clone())
        );
        assert_eq!(a, b);
        assert_eq!(db.images(&upload.album).await.unwrap().len(), 1);
    }

    async fn stored(store: &Store, id: &str) -> bool {
        store
            .read(&store::key(Variant::Original, id))
            .await
            .unwrap()
            .is_some()
    }

    #[tokio::test]
    async fn the_sweep_removes_recorded_strays_and_nothing_it_does_not_know() {
        let (uploads, db, store, _dir) = setup().await;
        let (_, ids) = crate::db::tests::album_with(&db, "ann", 1).await;
        // Copies a crash left between storing and recording, which the
        // database recorded first; and copies it never heard of, as a
        // database restored from an older copy would not have.
        db.add_stray("strayXXXXX").await.unwrap();
        for id in ["strayXXXXX", "unknownXXX", &ids[0]] {
            store
                .put(&store::key(Variant::Original, id), Bytes::from_static(b"x"))
                .await
                .unwrap();
        }
        // Too new to sweep: an upload may be about to record it.
        uploads.sweep().await;
        assert!(stored(&store, "strayXXXXX").await);
        tokio::time::sleep(Duration::from_millis(5)).await;
        uploads.sweep_older_than(Duration::ZERO).await;
        assert!(!stored(&store, "strayXXXXX").await);
        assert!(stored(&store, "unknownXXX").await);
        assert!(stored(&store, &ids[0]).await);
    }

    #[tokio::test]
    async fn an_image_too_big_for_the_memory_budget_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::temp().await;
        let uploads = Arc::new(
            Uploads::new(
                db.clone(),
                Store::memory(),
                dir.path(),
                8 << 20,
                50_000_000,
                Some(0),
            )
            .unwrap(),
        );
        let file = jpeg(1500, 1500);
        let upload = begin(&db, file.len() as u64).await;
        uploads
            .append(&upload, 0, file.len() as u64, chunks(&file))
            .await
            .unwrap();
        let upload = db.upload(&upload.id).await.unwrap().unwrap();
        let Outcome::Rejected(why) = uploads.finish(upload).await else {
            panic!("expected a rejection");
        };
        assert!(why.contains("memory"), "{why}");
    }

    #[tokio::test]
    async fn locks_go_once_nobody_holds_them() {
        let (uploads, db, _, _dir) = setup().await;
        let file = jpeg(64, 64);
        let upload = begin(&db, file.len() as u64).await;
        let half = (file.len() / 2) as u64;
        uploads
            .append(&upload, 0, half, chunks(&file[..half as usize]))
            .await
            .unwrap();
        assert!(uploads.locks.lock().is_empty());
        // Held while another waits: it stays until both are done.
        let held = uploads.lock(&upload.id).await;
        let waiting = uploads.cancel(&upload);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        assert_eq!(uploads.locks.lock().len(), 1);
        drop(held);
        waiting.await.unwrap();
        assert!(uploads.locks.lock().is_empty());
    }

    #[tokio::test]
    async fn a_lost_file_resumes_from_what_it_holds_rather_than_zeros() {
        let (uploads, db, _, _dir) = setup().await;
        let file = jpeg(300, 200);
        let upload = begin(&db, file.len() as u64).await;
        let half = file.len() / 2;
        uploads
            .append(&upload, 0, half as u64, chunks(&file[..half]))
            .await
            .unwrap();
        // The disk lost it, while the database still counts half.
        std::fs::remove_file(uploads.file(&upload.id)).unwrap();
        let rest = (file.len() - half) as u64;
        assert_eq!(
            uploads
                .append(&upload, half as u64, rest, chunks(&file[half..]))
                .await,
            Err(ChunkError::Conflict(0))
        );
        assert_eq!(db.upload(&upload.id).await.unwrap().unwrap().received, 0);
    }

    #[tokio::test]
    async fn a_file_short_of_its_count_is_counted_again_and_sent_again() {
        let (uploads, db, _, _dir) = setup().await;
        let file = jpeg(300, 200);
        let size = file.len() as u64;
        let upload = begin(&db, size).await;
        uploads
            .append(&upload, 0, size, chunks(&file))
            .await
            .unwrap();
        // The tail never reached the disk.
        let half = size / 2;
        std::fs::OpenOptions::new()
            .write(true)
            .open(uploads.file(&upload.id))
            .unwrap()
            .set_len(half)
            .unwrap();
        let stale = db.upload(&upload.id).await.unwrap().unwrap();
        assert!(matches!(uploads.finish(stale).await, Outcome::Failed(_)));
        let upload = db.upload(&upload.id).await.unwrap().unwrap();
        assert_eq!(upload.received, half);
        uploads
            .append(&upload, half, size - half, chunks(&file[half as usize..]))
            .await
            .unwrap();
        let upload = db.upload(&upload.id).await.unwrap().unwrap();
        assert!(matches!(uploads.finish(upload).await, Outcome::Done(_)));
    }

    #[tokio::test]
    async fn a_finish_read_before_another_run_ended_reads_how_it_ended() {
        let (uploads, db, _, _dir) = setup().await;
        let file = jpeg(64, 64);
        let upload = begin(&db, file.len() as u64).await;
        uploads
            .append(&upload, 0, file.len() as u64, chunks(&file))
            .await
            .unwrap();
        let stale = db.upload(&upload.id).await.unwrap().unwrap();
        let Outcome::Done(id) = uploads.finish(stale.clone()).await else {
            panic!("expected an image");
        };
        // Read while the first run was going, asked after it ended.
        assert_eq!(uploads.finish(stale).await, Outcome::Done(id));
    }
}
