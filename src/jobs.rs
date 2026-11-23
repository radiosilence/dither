//! The processing queue, as the web process keeps it, and what it and a
//! worker say to each other.
//!
//! An upload that finishes is a job: its image's row, added as processing,
//! and its file moved to `STATE_DIR/jobs/<id>`. Workers (`dither worker`,
//! `worker.rs`) may run anywhere: beside the web process, or on other
//! machines. They reach the web process over its worker API
//! (`web::work`) and nothing else, so the database and the store have one
//! owner, and any number of workers pull from one queue.
//!
//! A worker asks for a job and is held until one is waiting. Its claim is a
//! lease on the row, renewed while it works; a worker that stops renewing
//! loses the job to the next to ask. It fetches the upload, reports its
//! progress (each report renews the lease, and is answered with whether to
//! carry on), sends what it made, and says how the job ended, which the web
//! process records: made, refused, failed for good, or put back for later.
//! The database is the record: whatever is lost in between, the rows still
//! say what is waiting, being made, made or failed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, broadcast};

use crate::db::{Db, Made};
use crate::store::{self, Store};

/// Why a job failed, as its row records it: keys the pages word.
pub const FAILED_UNREADABLE: &str = "unreadable";
pub const FAILED_TOO_LONG: &str = "too-long";
pub const FAILED_TOO_LARGE: &str = "too-large";
/// A video, and no worker that can make videos.
pub const FAILED_UNSUPPORTED: &str = "unsupported";
pub const FAILED: &str = "failed";

/// How long a claim lasts unrenewed. A worker renews it well within this.
pub const LEASE: Duration = Duration::from_secs(60);
/// How long a worker's request for a job is held when none is waiting.
pub const POLL: Duration = Duration::from_secs(25);
/// Claims before a job that keeps failing on this side is marked failed.
const ATTEMPTS: u32 = 3;
/// How long a worker counts as there since it last asked for anything:
/// longer than it is ever held waiting for a job.
const ALIVE: Duration = Duration::from_secs(60);
/// How long a job's progress stands with no word of it: longer than any step
/// of a transcode between reports.
const STALE: Duration = Duration::from_secs(300);

/// Where a worker works on its jobs.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("jobs")
}

/// Where a worker works on job `id`.
pub fn work(state_dir: &Path, id: &str) -> PathBuf {
    dir(state_dir).join(format!("{id}.work"))
}

/// Job `id`'s upload in the store, until the job ends. Ids are checked by
/// [`crate::id`] or made by it.
pub fn upload(id: &str) -> object_store::path::Path {
    object_store::path::Path::from(format!("upload/{id}"))
}

/// Where worker `name` puts job `id`'s copy of `variant` before it is moved
/// into place.
fn aside(id: &str, name: &str, variant: store::Variant) -> object_store::path::Path {
    object_store::path::Path::from(format!("work/{id}/{name}/{}", variant.name()))
}

/// How long a signed URL a worker is given lasts: longer than any one fetch
/// or put of a job's files takes.
const SIGNED: Duration = Duration::from_secs(3600);

/// A job, as a worker is given it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub id: String,
    pub video: bool,
    /// The upload's length.
    pub bytes: u64,
    /// A video to keep as recorded rather than convert.
    pub as_recorded: bool,
    /// The web process's limits, which a worker holds the job to.
    pub max_pixels: u64,
    pub max_seconds: u64,
    /// A signed URL to fetch the upload from the store directly; without
    /// one, the worker API serves it.
    #[serde(default)]
    pub source: Option<String>,
}

/// How a job ended, as its worker says.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Made, and its copies sent. Its original's length and SHA-256 are what
    /// the web process stored, not what the worker says.
    Made { made: Made },
    /// The file is at fault: failed at once, with a `FAILED_*` key and what
    /// to log.
    Refused { key: String, detail: String },
    /// Something on the worker's side failed: tried again later, up to a
    /// point.
    Failed { why: String },
    /// Stopped: deleted while it was made, or the claim was lost.
    Stopped,
}

/// What a worker can take now, when it asks for a job.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Want {
    pub pictures: bool,
    pub videos: bool,
    /// What it makes at all, whether or not it has room now.
    pub makes_pictures: bool,
    pub makes_videos: bool,
    /// The jobs it is making now, which it is not offered again.
    #[serde(default)]
    pub making: Vec<String>,
}

/// What became of a copy a worker said it put in the store.
#[derive(Debug, PartialEq, Eq)]
pub enum Placed {
    Yes,
    /// The job is not the worker's.
    NotYours,
    /// The store holds this many bytes of it (none at all with `None`), not
    /// what the worker said.
    Short(Option<u64>),
}

/// A job's progress, for the album page showing it.
#[derive(Debug, Clone)]
pub struct Tick {
    pub owner: Arc<str>,
    pub id: String,
    /// Thousandths.
    pub done: u16,
}

/// A worker as last heard from.
#[derive(Clone, Copy)]
struct Seen {
    at: Instant,
    pictures: bool,
    videos: bool,
}

/// The last word of a job's progress: how far along, when it came, and
/// whose job it is.
struct Report {
    done: u16,
    at: Instant,
    owner: Arc<str>,
}

/// The web process's side of the queue.
pub struct Hub {
    db: Db,
    store: Store,
    max_pixels: u64,
    max_seconds: u64,
    /// The last word of each job's progress.
    progress: parking_lot::Mutex<HashMap<String, Report>>,
    ticks: broadcast::Sender<Tick>,
    /// Each job as it ends: its id, and whether it was made.
    ended: broadcast::Sender<(String, bool)>,
    /// Workers waiting for a job hear one may be.
    waiting: Notify,
    /// Whether uploads an earlier version left are in the store yet: no job
    /// is handed out until they are.
    adopted: tokio::sync::watch::Sender<bool>,
    /// Each worker heard from: when, and what it makes.
    workers: parking_lot::Mutex<HashMap<String, Seen>>,
}

/// The words the API gives for a failure key, which the pages word through
/// gettext instead.
pub fn reason(key: &str) -> &'static str {
    match key {
        FAILED_UNREADABLE => "it can't be read: it may be damaged",
        FAILED_TOO_LONG => "it's longer than this server takes",
        FAILED_TOO_LARGE => "it's larger than this server takes",
        FAILED_UNSUPPORTED => "this server can't process videos",
        _ => "it couldn't be processed",
    }
}

/// A key a worker gave, held to the ones the pages word.
fn known(key: &str) -> &'static str {
    [
        FAILED_UNREADABLE,
        FAILED_TOO_LONG,
        FAILED_TOO_LARGE,
        FAILED_UNSUPPORTED,
    ]
    .into_iter()
    .find(|k| *k == key)
    .unwrap_or(FAILED)
}

fn now() -> i64 {
    crate::db::now()
}

fn until() -> i64 {
    now() + LEASE.as_millis() as i64
}

impl Hub {
    pub fn new(db: Db, store: Store, config: &crate::config::Config) -> std::io::Result<Self> {
        Ok(Self {
            db,
            store,
            max_pixels: config.max_pixels,
            max_seconds: config.video.max_seconds,
            progress: Default::default(),
            ticks: broadcast::channel(1024).0,
            ended: broadcast::channel(1024).0,
            waiting: Notify::new(),
            adopted: tokio::sync::watch::Sender::new(true),
            workers: Default::default(),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Worker `name` was heard from, saying what it makes when it asks for
    /// a job.
    fn heard(&self, name: &str, makes: Option<(bool, bool)>) {
        let mut workers = self.workers.lock();
        workers.retain(|_, seen| seen.at.elapsed() < ALIVE);
        // One not heard asking since this process started (busy since) is
        // counted once it asks, rather than as making nothing.
        let Some((pictures, videos)) =
            makes.or_else(|| workers.get(name).map(|s| (s.pictures, s.videos)))
        else {
            return;
        };
        workers.insert(
            name.to_string(),
            Seen {
                at: Instant::now(),
                pictures,
                videos,
            },
        );
    }

    fn alive(&self) -> Vec<Seen> {
        self.workers
            .lock()
            .values()
            .filter(|seen| seen.at.elapsed() < ALIVE)
            .copied()
            .collect()
    }

    /// Whether a worker that makes pictures is there to take them now.
    pub fn takes_pictures(&self) -> bool {
        self.alive().iter().any(|seen| seen.pictures)
    }

    /// Whether videos can be made: taken on the configuration's word until a
    /// worker is heard from, then whether any that is makes them.
    pub fn takes_videos(&self) -> bool {
        let alive = self.alive();
        alive.is_empty() || alive.iter().any(|seen| seen.videos)
    }

    /// Wait up to `limit` for job `id` to end: whether it was made, or `None`
    /// when it has not ended by then. Ask before the job can end, as
    /// [`ends`](Self::ends) does.
    pub async fn ended(
        mut ends: broadcast::Receiver<(String, bool)>,
        id: &str,
        limit: Duration,
    ) -> Option<bool> {
        let wait = async {
            loop {
                match ends.recv().await {
                    Ok((ended, made)) if ended == id => return Some(made),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        };
        tokio::time::timeout(limit, wait).await.ok().flatten()
    }

    /// Jobs' ends, from now.
    pub fn ends(&self) -> broadcast::Receiver<(String, bool)> {
        self.ended.subscribe()
    }

    /// A job is waiting.
    pub fn wake(&self) {
        self.waiting.notify_waiters();
    }

    /// A job for worker `name`, of a kind it wants, held up to `limit` for
    /// one to be waiting.
    pub async fn claim(
        &self,
        name: &str,
        want: Want,
        limit: Duration,
    ) -> crate::db::Result<Option<Job>> {
        self.heard(name, Some((want.makes_pictures, want.makes_videos)));
        let deadline = tokio::time::Instant::now() + limit;
        let mut adopted = self.adopted.subscribe();
        if tokio::time::timeout_at(deadline, adopted.wait_for(|done| *done))
            .await
            .is_err()
        {
            return Ok(None);
        }
        loop {
            // Listening before looking, so a job queued in between is heard.
            let waiting = self.waiting.notified();
            tokio::pin!(waiting);
            waiting.as_mut().enable();
            // A job failed here for having killed workers leaves the next
            // to be looked for at once.
            let mut again = false;
            for video in [false, true] {
                if !(if video { want.videos } else { want.pictures }) {
                    continue;
                }
                let Some(claimed) = self.db.claim(name, video, until(), &want.making).await? else {
                    continue;
                };
                let image = claimed.image;
                // A job that has killed workers before is not tried again.
                if image.attempts > ATTEMPTS {
                    again = true;
                    let detail = format!("claimed {} times without ending", image.attempts - 1);
                    self.end(
                        name,
                        &image.id,
                        Outcome::Refused {
                            key: FAILED.into(),
                            detail,
                        },
                        0,
                    )
                    .await?;
                    continue;
                }
                self.started(&claimed.owner, &image.id);
                // The upload's length is its object's: the row's becomes
                // the original's once a worker has stored one.
                let bytes = match self.store.size(&upload(&image.id)).await {
                    Ok(Some(size)) => size,
                    _ => image.bytes,
                };
                let source = self
                    .store
                    .sign(axum::http::Method::GET, &upload(&image.id), SIGNED)
                    .await
                    .map(String::from);
                return Ok(Some(Job {
                    source,
                    video: image.is_video(),
                    bytes,
                    as_recorded: image.as_recorded,
                    max_pixels: self.max_pixels,
                    max_seconds: self.max_seconds,
                    id: image.id,
                }));
            }
            if again {
                continue;
            }
            tokio::select! {
                () = waiting => {}
                () = tokio::time::sleep_until(deadline) => return Ok(None),
            }
            self.heard(name, None);
        }
    }

    /// Job `id` of `owner`'s is being made, from nothing so far.
    fn started(&self, owner: &str, id: &str) {
        let owner: Arc<str> = owner.into();
        self.progress.lock().insert(
            id.to_string(),
            Report {
                done: 0,
                at: Instant::now(),
                owner: owner.clone(),
            },
        );
        let _ = self.ticks.send(Tick {
            owner,
            id: id.to_string(),
            done: 0,
        });
    }

    /// Worker `name` still makes job `id`: its lease renewed, and `done`
    /// thousandths of it made, if it says. False when the job is no longer
    /// its own (deleted, or taken by another after its lease lapsed), and it
    /// should stop.
    pub async fn renew(&self, name: &str, id: &str, done: Option<u16>) -> crate::db::Result<bool> {
        self.heard(name, None);
        if !self.db.renew(name, id, until()).await? {
            return Ok(false);
        }
        if let Some(done) = done {
            // Whose it is was noted when it was claimed, unless the web
            // process has started again since.
            let known = self.progress.lock().get(id).map(|r| r.owner.clone());
            let owner = match known {
                Some(owner) => owner,
                None => match self.db.image(id).await? {
                    Some((_, album)) => album.owner.into(),
                    None => return Ok(false),
                },
            };
            {
                let mut progress = self.progress.lock();
                // What no word has come of for a while (a worker stopped
                // mid-job) goes.
                progress.retain(|_, r| r.at.elapsed() < STALE);
                progress.insert(
                    id.to_string(),
                    Report {
                        done,
                        at: Instant::now(),
                        owner: owner.clone(),
                    },
                );
            }
            let _ = self.ticks.send(Tick {
                owner,
                id: id.to_string(),
                done,
            });
        }
        Ok(true)
    }

    /// Job `id`'s copy of `variant`, from worker `name`, sent through the
    /// worker API: stored aside, then placed. False when the job is not the
    /// worker's.
    pub async fn put<E>(
        &self,
        name: &str,
        id: &str,
        variant: store::Variant,
        body: impl futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    ) -> anyhow::Result<bool>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        if !self.renew(name, id, None).await? {
            return Ok(false);
        }
        let aside = aside(id, name, variant);
        let (bytes, sha256) = self.store.put_stream(&aside, body).await?;
        // Already on this machine's disk, if it caches: written as it came.
        self.place(name, id, variant, bytes, sha256, false).await
    }

    /// A signed URL for worker `name` to put job `id`'s copy of `variant`
    /// straight into the store, aside: `Ok(None)` when the store does not
    /// sign, and `Err` when the job is not the worker's.
    pub async fn put_url(
        &self,
        name: &str,
        id: &str,
        variant: store::Variant,
    ) -> anyhow::Result<Result<Option<String>, ()>> {
        if !self.renew(name, id, None).await? {
            return Ok(Err(()));
        }
        let url = self
            .store
            .sign(axum::http::Method::PUT, &aside(id, name, variant), SIGNED)
            .await;
        Ok(Ok(url.map(String::from)))
    }

    /// Worker `name` put job `id`'s copy of `variant` straight into the
    /// store, `bytes` long with `sha256`: placed, once the store holds that
    /// length. The SHA-256 is the worker's word, as the copy is; the length
    /// is checked.
    pub async fn put_done(
        &self,
        name: &str,
        id: &str,
        variant: store::Variant,
        bytes: u64,
        sha256: [u8; 32],
    ) -> anyhow::Result<Placed> {
        if !self.renew(name, id, None).await? {
            return Ok(Placed::NotYours);
        }
        let held = self.store.size(&aside(id, name, variant)).await?;
        if held != Some(bytes) {
            return Ok(Placed::Short(held));
        }
        Ok(
            if self.place(name, id, variant, bytes, sha256, true).await? {
                Placed::Yes
            } else {
                Placed::NotYours
            },
        )
    }

    /// Move job `id`'s copy of `variant` from where worker `name` put it
    /// into place, only if the job is still the worker's once it has
    /// arrived, so a worker that lost the job cannot replace what the one
    /// that took it stored. An original's length and SHA-256 go on the
    /// job's row for when it is made.
    async fn place(
        &self,
        name: &str,
        id: &str,
        variant: store::Variant,
        bytes: u64,
        sha256: [u8; 32],
        warm: bool,
    ) -> anyhow::Result<bool> {
        let aside = aside(id, name, variant);
        let still = self.renew(name, id, None).await?
            && (variant != store::Variant::Original
                || self.db.record_original(name, id, bytes, sha256).await?);
        if !still {
            let _ = self.store.delete(&aside).await;
            return Ok(false);
        }
        let key = store::key(variant, id);
        self.store.rename(&aside, &key).await?;
        // On this machine's disk too, if it caches and the copy did not pass
        // through it: its owner looks next.
        if warm {
            self.store.warm(&key, bytes);
        }
        Ok(true)
    }

    /// Record how worker `name`'s job `id` ended, after `ms`: made, refused,
    /// put back for later, or not its to record. Its upload and, unless it
    /// was made, its copies go once it has ended.
    pub async fn end(
        &self,
        name: &str,
        id: &str,
        outcome: Outcome,
        ms: u64,
    ) -> crate::db::Result<()> {
        self.heard(name, None);
        let Some((image, album)) = self.db.image(id).await? else {
            // Deleted: whatever was stored of it goes.
            self.gone(id, false).await;
            return Ok(());
        };
        let owner = album.owner;
        let video = image.is_video();
        // The original's length and SHA-256 are what was stored, recorded
        // on the row as it arrived, not what the worker says.
        let outcome = match outcome {
            Outcome::Made { .. } if image.sha256.is_empty() => Outcome::Failed {
                why: "made, but its original never arrived".into(),
            },
            Outcome::Made { mut made } => {
                made.bytes = image.bytes;
                made.sha256 = image.sha256.clone();
                Outcome::Made { made }
            }
            outcome => outcome,
        };
        let (recorded, made, key) = match outcome {
            Outcome::Made { made } => {
                let bytes = made.bytes;
                let recorded = self.db.finish_job(&owner, name, id, made).await?;
                if recorded {
                    tracing::info!(job = %id, video, ms, "made");
                }
                (recorded, Some(bytes), "")
            }
            Outcome::Stopped => (false, None, ""),
            Outcome::Failed { why } if image.attempts < ATTEMPTS => {
                tracing::warn!(job = %id, worker = %name, attempt = image.attempts, error = %why, "a job failed; it will be tried again");
                self.later(name, id, &owner, image.attempts).await?;
                return Ok(());
            }
            Outcome::Failed { why } => {
                tracing::error!(job = %id, worker = %name, error = %why, "a job failed for good");
                (
                    self.db.fail_job(&owner, name, id, FAILED).await?,
                    None,
                    FAILED,
                )
            }
            Outcome::Refused { key, detail } => {
                let key = known(&key);
                tracing::info!(job = %id, worker = %name, reason = %detail, "a job's file was refused");
                (self.db.fail_job(&owner, name, id, key).await?, None, key)
            }
        };
        if recorded {
            self.progress.lock().remove(id);
            // A refusal is the file's; a failure on this side is not.
            crate::metrics::processed(
                made.is_some(),
                made.is_none() && key != FAILED,
                video,
                Duration::from_millis(ms),
                made.unwrap_or(0),
            );
            self.gone(id, made.is_some()).await;
            let _ = self.ended.send((id.to_string(), made.is_some()));
            return Ok(());
        }
        // Not this worker's to record. Deleted: everything goes. Taken by
        // another worker after this one's lease lapsed: everything stays,
        // the copies included, which are that worker's now. Stopped while
        // still this worker's: given back at once, for whichever worker asks
        // next.
        match self.db.image(id).await? {
            None => self.gone(id, false).await,
            Some(_) => {
                if self.db.release(name, id, 0).await? {
                    self.waits(id, &owner);
                }
            }
        }
        self.wake();
        Ok(())
    }

    /// Put job `id` back to be tried again, later with each attempt, and say
    /// it waits, so its album does not show it stuck part way.
    async fn later(
        &self,
        name: &str,
        id: &str,
        owner: &str,
        attempts: u32,
    ) -> crate::db::Result<()> {
        let wait = Duration::from_secs(30) * attempts.max(1);
        let after = now() + wait.as_millis() as i64;
        // Not this worker's to put back: another holds it, or it ended.
        if self.db.release(name, id, after).await? {
            self.waits(id, owner);
        }
        Ok(())
    }

    /// Job `id` of `owner`'s was put back: its album shows it waiting, not
    /// stuck part way.
    fn waits(&self, id: &str, owner: &str) {
        self.progress.lock().remove(id);
        let _ = self.ticks.send(Tick {
            owner: owner.into(),
            id: id.to_string(),
            done: 0,
        });
    }

    /// Job `id` has ended, or its image is gone: its upload goes, and its
    /// copies with it unless they were `kept`.
    async fn gone(&self, id: &str, kept: bool) {
        let _ = self.store.delete(&upload(id)).await;
        if !kept {
            store::discard(&self.store, &self.db, vec![id.to_string()]).await;
        }
    }

    /// Give back every claim worker `name` holds, for the next to take up at
    /// once: it is stopping.
    pub async fn release_all(&self, name: &str) -> crate::db::Result<()> {
        self.db.release_all(name).await?;
        self.workers.lock().remove(name);
        self.wake();
        Ok(())
    }

    /// Images `ids` were deleted: the files of any still waiting go. A worker
    /// making one finds out when it next renews its claim.
    pub async fn forget(&self, ids: &[String]) {
        use futures_util::StreamExt as _;
        for id in ids {
            self.progress.lock().remove(id);
        }
        // Only those still waiting have an upload in the store; trying the
        // rest costs a request each, so it is done behind the response, a
        // few at a time. The sweep removes whatever this misses.
        let (store, ids) = (self.store.clone(), ids.to_vec());
        tokio::spawn(async move {
            futures_util::stream::iter(ids)
                .for_each_concurrent(8, |id| {
                    let store = store.clone();
                    async move {
                        let _ = store.delete(&upload(&id)).await;
                    }
                })
                .await;
        });
    }

    /// Move uploads an earlier version left waiting in `state_dir`'s jobs
    /// directory into the store, where workers now fetch them, in the
    /// background, so the process serves (and answers its probes) at once;
    /// no job is handed out until they are moved. Entries with a dot in
    /// their names are a worker's scratch, its own to tidy.
    pub fn adopt(self: &Arc<Self>, state_dir: &Path) {
        self.adopted.send_replace(false);
        let (this, dir) = (self.clone(), dir(state_dir));
        tokio::spawn(async move {
            if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.contains('.') || !crate::id::valid(&name) {
                        continue;
                    }
                    let path = entry.path();
                    match this.store.put_file(&upload(&name), &path).await {
                        Ok(_) => {
                            let _ = tokio::fs::remove_file(&path).await;
                            tracing::info!(job = %name, "moved a waiting upload into the store");
                        }
                        Err(e) => {
                            tracing::warn!(job = %name, error = %e, "moving a waiting upload into the store failed")
                        }
                    }
                }
            }
            this.adopted.send_replace(true);
            this.wake();
        });
    }

    /// Thousandths done of job `id`, while a worker reports on it.
    pub fn progress(&self, id: &str) -> Option<u16> {
        self.progress
            .lock()
            .get(id)
            .filter(|r| r.at.elapsed() < STALE)
            .map(|r| r.done)
    }

    pub fn ticks(&self) -> broadcast::Receiver<Tick> {
        self.ticks.subscribe()
    }

    /// Remove what jobs left in the store a day ago and nothing is being
    /// made of: an upload whose job ended without tidying it (a crash in
    /// between, or a row deleted while it waited), and copies a worker put
    /// aside and never had placed.
    pub async fn sweep(&self) {
        let before = crate::db::now() - 24 * 3600 * 1000;
        if let Ok(uploads) = self.store.older("upload/", before).await {
            for key in uploads {
                let id = key.filename().unwrap_or_default().to_string();
                let waiting = match self.db.image(&id).await {
                    Ok(Some((image, _))) => image.status == crate::db::Status::Processing,
                    Ok(None) => false,
                    Err(_) => true,
                };
                if !waiting {
                    let _ = self.store.delete(&key).await;
                }
            }
        }
        if let Ok(aside) = self.store.older("work/", before).await {
            for key in aside {
                let _ = self.store.delete(&key).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_copy_put_straight_into_the_store_is_checked_and_placed() {
        use sha2::Digest as _;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::temp().await;
        let store = Store::memory();
        let hub = Hub::new(
            db.clone(),
            store.clone(),
            &crate::config::Config::test(dir.path().to_path_buf()),
        )
        .unwrap();
        let (picture, _) = crate::db::tests::queued(&db).await;
        let want = Want {
            pictures: true,
            videos: false,
            makes_pictures: true,
            makes_videos: false,
            making: vec![],
        };
        let job = hub
            .claim("w1", want, Duration::ZERO)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.id, picture);
        assert_eq!(job.source, None, "memory signs nothing");
        // Memory signs nothing, so a worker is told to send it through the API.
        assert_eq!(
            hub.put_url("w1", &picture, store::Variant::Original)
                .await
                .unwrap(),
            Ok(None)
        );
        let bytes = bytes::Bytes::from_static(b"cleaned picture");
        let sha256: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        store
            .put(
                &aside(&picture, "w1", store::Variant::Original),
                bytes.clone(),
            )
            .await
            .unwrap();
        // A length the store does not hold is refused.
        assert_eq!(
            hub.put_done("w1", &picture, store::Variant::Original, 3, sha256)
                .await
                .unwrap(),
            Placed::Short(Some(bytes.len() as u64))
        );
        assert_eq!(
            hub.put_done(
                "w1",
                &picture,
                store::Variant::Original,
                bytes.len() as u64,
                sha256
            )
            .await
            .unwrap(),
            Placed::Yes
        );
        assert_eq!(
            store
                .read(&store::key(store::Variant::Original, &picture))
                .await
                .unwrap()
                .as_deref(),
            Some(&bytes[..])
        );
        let (image, _) = db.image(&picture).await.unwrap().unwrap();
        assert_eq!(
            (image.bytes, image.sha256),
            (bytes.len() as u64, sha256.to_vec())
        );
        // Another worker's copy, whole, is not placed: the job is not its.
        store
            .put(&aside(&picture, "w2", store::Variant::Thumb), bytes.clone())
            .await
            .unwrap();
        assert_eq!(
            hub.put_done(
                "w2",
                &picture,
                store::Variant::Thumb,
                bytes.len() as u64,
                sha256
            )
            .await
            .unwrap(),
            Placed::NotYours
        );
        assert!(
            store
                .read(&store::key(store::Variant::Thumb, &picture))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn outcomes_are_tagged_json() {
        let refused = Outcome::Refused {
            key: FAILED_TOO_LONG.into(),
            detail: "31:00 long".into(),
        };
        let json = serde_json::to_string(&refused).unwrap();
        assert_eq!(
            json,
            r#"{"outcome":"refused","key":"too-long","detail":"31:00 long"}"#
        );
        assert_eq!(serde_json::from_str::<Outcome>(&json).unwrap(), refused);
        assert_eq!(
            serde_json::from_str::<Outcome>(r#"{"outcome":"stopped"}"#).unwrap(),
            Outcome::Stopped
        );
    }

    #[test]
    fn a_workers_key_is_held_to_the_known_ones() {
        assert_eq!(known(FAILED_TOO_LARGE), FAILED_TOO_LARGE);
        assert_eq!(known("<script>"), FAILED);
    }
}
