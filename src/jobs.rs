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

/// Where jobs' files wait, and are worked on.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("jobs")
}

/// Job `id`'s upload. Ids are checked by [`crate::id`] or made by it, so this
/// names nothing outside the directory.
pub fn source(state_dir: &Path, id: &str) -> PathBuf {
    dir(state_dir).join(id)
}

/// Where a worker works on job `id`.
pub fn work(state_dir: &Path, id: &str) -> PathBuf {
    dir(state_dir).join(format!("{id}.work"))
}

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
    state_dir: PathBuf,
    max_pixels: u64,
    max_seconds: u64,
    /// The last word of each job's progress.
    progress: parking_lot::Mutex<HashMap<String, Report>>,
    ticks: broadcast::Sender<Tick>,
    /// Each job as it ends: its id, and whether it was made.
    ended: broadcast::Sender<(String, bool)>,
    /// Workers waiting for a job hear one may be.
    waiting: Notify,
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
        std::fs::create_dir_all(dir(&config.state_dir))?;
        Ok(Self {
            db,
            store,
            state_dir: config.state_dir.clone(),
            max_pixels: config.max_pixels,
            max_seconds: config.video.max_seconds,
            progress: Default::default(),
            ticks: broadcast::channel(1024).0,
            ended: broadcast::channel(1024).0,
            waiting: Notify::new(),
            workers: Default::default(),
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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
                // The upload's length is its file's: the row's becomes the
                // original's once a worker has stored one.
                let bytes = tokio::fs::metadata(source(&self.state_dir, &image.id))
                    .await
                    .map_or(image.bytes, |m| m.len());
                return Ok(Some(Job {
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

    /// Job `id`'s copy of `variant`, from worker `name`: stored, and for its
    /// original, its length and SHA-256 recorded on its row for when it is
    /// made. False when the job is not the worker's.
    ///
    /// It is written aside and moved into place only if the job is still the
    /// worker's once it has arrived, so a worker that lost the job while
    /// sending cannot replace what the one that took it stored.
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
        let aside = object_store::path::Path::from(format!("work/{id}/{name}/{}", variant.name()));
        let (bytes, sha256) = self.store.put_stream(&aside, body).await?;
        let still = self.renew(name, id, None).await?
            && (variant != store::Variant::Original
                || self.db.record_original(name, id, bytes, sha256).await?);
        if !still {
            let _ = self.store.delete(&aside).await;
            return Ok(false);
        }
        self.store.rename(&aside, &store::key(variant, id)).await?;
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
        let _ = tokio::fs::remove_file(source(&self.state_dir, id)).await;
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
        for id in ids {
            self.progress.lock().remove(id);
            let _ = tokio::fs::remove_file(source(&self.state_dir, id)).await;
        }
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

    /// Remove what jobs left in the directory once nothing is being made of
    /// them: a crash between finishing and tidying, or a row deleted while
    /// its file waited. A day's grace, as uploads get.
    pub async fn sweep(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(dir(&self.state_dir)).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            // `<id>`, or `<id>.<worker>.work`.
            let id = name.split('.').next().unwrap_or(&name);
            let stale = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 3600));
            if !stale {
                continue;
            }
            let keep = match self.db.image(id).await {
                Ok(Some((image, _))) => image.status == crate::db::Status::Processing,
                Ok(None) => false,
                Err(_) => true,
            };
            if !keep {
                let path = entry.path();
                let _ = tokio::fs::remove_dir_all(&path).await;
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
