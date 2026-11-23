//! The processing queue, as the web process sees it, and what it and a
//! worker say to each other.
//!
//! An upload that finishes is a job: its image's row, added as processing,
//! and its file moved to `STATE_DIR/jobs/<id>`. A worker (`dither worker`,
//! `worker.rs`) claims jobs from the database with a lease it renews while it
//! works, makes their copies, and marks them ready or failed. The database is
//! the record: whatever either process misses, the rows still say what is
//! waiting, being made, made or failed.
//!
//! What happens is said over a Unix socket in the state directory, one JSON
//! object a line each way. The web process listens; a worker connects. The
//! web process says a job is waiting, so a worker looks without polling, and
//! that jobs were deleted, so a worker making one stops. A worker says how
//! far along a job is, which the web process passes to the album pages
//! showing it, and that a job ended, which is a write the web process did
//! not make: it redraws its owner's pages and tells the snapshotter, which
//! would otherwise miss it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use crate::db::Db;

/// Why a job failed, as its row records it: keys the pages word.
pub const FAILED_UNREADABLE: &str = "unreadable";
pub const FAILED_TOO_LONG: &str = "too-long";
pub const FAILED_TOO_LARGE: &str = "too-large";
/// A video, and no worker that can make videos.
pub const FAILED_UNSUPPORTED: &str = "unsupported";
pub const FAILED: &str = "failed";

/// How long a job's progress stands with no word of it: longer than any step
/// of a transcode between reports.
const STALE: std::time::Duration = std::time::Duration::from_secs(300);

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

pub fn socket(state_dir: &Path) -> PathBuf {
    state_dir.join("worker.sock")
}

/// What the web process tells a worker.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ToWorker {
    /// A job is waiting.
    Wake,
    /// These were deleted: stop making any of them.
    Cancel { ids: Vec<String> },
}

/// What a worker tells the web process.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum FromWorker {
    /// Job `id` of `owner`'s is `done` thousandths made.
    Progress {
        owner: String,
        id: String,
        done: u16,
    },
    /// Job `id` of `owner`'s was put back, to be made from the start later.
    Waiting { owner: String, id: String },
    /// Job `id` of `owner`'s ended: made, or not, after `ms`, its original
    /// `bytes` long.
    Ended {
        owner: String,
        id: String,
        made: bool,
        video: bool,
        ms: u64,
        bytes: u64,
    },
}

/// A job's progress, for the album page showing it.
#[derive(Debug, Clone)]
pub struct Tick {
    pub owner: Arc<str>,
    pub id: String,
    /// Thousandths.
    pub done: u16,
}

/// The web process's side of the queue.
pub struct Hub {
    db: Db,
    state_dir: PathBuf,
    /// The last word of each job's progress, and when it came.
    progress: parking_lot::Mutex<HashMap<String, (u16, std::time::Instant)>>,
    ticks: broadcast::Sender<Tick>,
    orders: broadcast::Sender<ToWorker>,
}

impl Hub {
    pub fn new(db: Db, state_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir(state_dir))?;
        Ok(Self {
            db,
            state_dir: state_dir.to_path_buf(),
            progress: Default::default(),
            ticks: broadcast::channel(1024).0,
            orders: broadcast::channel(256).0,
        })
    }

    /// Listen for workers. A socket a stopped process left is replaced.
    pub fn listen(self: &Arc<Self>) -> anyhow::Result<()> {
        use anyhow::Context as _;
        let path = socket(&self.state_dir);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("listening on {} for workers", path.display()))?;
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let this = this.clone();
                        tokio::spawn(async move { this.serve(stream).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accepting a worker failed");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });
        Ok(())
    }

    async fn serve(&self, stream: UnixStream) {
        tracing::info!("a worker connected");
        let (read, mut write) = stream.into_split();
        let mut orders = self.orders.subscribe();
        // One may have been waiting since before it connected.
        let mut pending = vec![ToWorker::Wake];
        let mut lines = tokio::io::BufReader::new(read).lines();
        loop {
            for order in pending.drain(..) {
                let mut line = serde_json::to_string(&order).expect("serialisable");
                line.push('\n');
                if write.write_all(line.as_bytes()).await.is_err() {
                    tracing::info!("a worker went away");
                    return;
                }
            }
            tokio::select! {
                order = orders.recv() => match order {
                    Ok(order) => pending.push(order),
                    // Missed some: whatever was waiting is still waiting.
                    Err(broadcast::error::RecvError::Lagged(_)) => pending.push(ToWorker::Wake),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                line = lines.next_line() => match line {
                    Ok(Some(line)) => match serde_json::from_str(&line) {
                        Ok(event) => self.heard(event),
                        Err(e) => tracing::warn!(error = %e, "a worker said something unreadable"),
                    },
                    _ => {
                        tracing::info!("a worker went away");
                        return;
                    }
                },
            }
        }
    }

    fn heard(&self, event: FromWorker) {
        match event {
            FromWorker::Progress { owner, id, done } => {
                let mut progress = self.progress.lock();
                // What no word has come of for a while (a worker stopped
                // mid-job, or a word after the job was deleted) goes.
                progress.retain(|_, (_, at)| at.elapsed() < STALE);
                progress.insert(id.clone(), (done, std::time::Instant::now()));
                drop(progress);
                let _ = self.ticks.send(Tick {
                    owner: owner.into(),
                    id,
                    done,
                });
            }
            FromWorker::Waiting { owner, id } => {
                self.progress.lock().remove(&id);
                let _ = self.ticks.send(Tick {
                    owner: owner.into(),
                    id,
                    done: 0,
                });
            }
            FromWorker::Ended {
                owner,
                id,
                made,
                video,
                ms,
                bytes,
            } => {
                self.progress.lock().remove(&id);
                crate::metrics::processed(made, video, std::time::Duration::from_millis(ms), bytes);
                self.db.announce(&owner);
            }
        }
    }

    /// A job is waiting.
    pub fn wake(&self) {
        let _ = self.orders.send(ToWorker::Wake);
    }

    /// Images `ids` were deleted: a worker making one stops, and the files of
    /// any still waiting go.
    pub async fn forget(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let _ = self.orders.send(ToWorker::Cancel { ids: ids.to_vec() });
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
            .filter(|(_, at)| at.elapsed() < STALE)
            .map(|(done, _)| *done)
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
                .is_some_and(|age| age > std::time::Duration::from_secs(24 * 3600));
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
    fn events_are_one_json_object_a_line() {
        let wake = serde_json::to_string(&ToWorker::Wake).unwrap();
        assert_eq!(wake, r#"{"event":"wake"}"#);
        let cancel = ToWorker::Cancel {
            ids: vec!["a".into()],
        };
        assert_eq!(
            serde_json::from_str::<ToWorker>(&serde_json::to_string(&cancel).unwrap()).unwrap(),
            cancel
        );
        let progress: FromWorker =
            serde_json::from_str(r#"{"event":"progress","owner":"ann","id":"x","done":5}"#)
                .unwrap();
        assert_eq!(
            progress,
            FromWorker::Progress {
                owner: "ann".into(),
                id: "x".into(),
                done: 5
            }
        );
    }
}
