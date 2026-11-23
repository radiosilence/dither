//! `dither worker`: the process that makes uploads' copies. It runs beside
//! the web process (in Kubernetes, a second container in its pod), shares
//! the state directory and the object store, and holds none of its secrets
//! but the store's.
//!
//! It waits for the web process to listen before it opens the database: the
//! web process may first replace a database that is behind the store's
//! latest copy, and a worker holding the old file would write into one that
//! is gone. For the same reason it stops when the web process goes away, to
//! be started again with it, rather than carrying on with a file that may be
//! replaced.
//!
//! Jobs are claimed from the database with a lease, renewed while the job is
//! made and lost when the job is deleted, which stops it. Pictures are made
//! several at a time under the memory budget, sized from each one's header;
//! videos one at a time by default, since ffmpeg's memory is its own. A job
//! that fails in a way that may pass is put back for later, three times
//! before it is marked failed; one whose file is at fault fails at once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::{Notify, Semaphore, mpsc, watch};

use crate::config::Config;
use crate::db::{Claimed, Db, Made};
use crate::jobs::{self, FromWorker, ToWorker};
use crate::sanitize::Container;
use crate::store::{self, Store, Variant};
use crate::video::{self, Tools};

/// How long a claim lasts unrenewed, and how often it is renewed.
const LEASE: Duration = Duration::from_secs(60);
const RENEW: Duration = Duration::from_secs(20);
/// How often to look for jobs when nobody says one is waiting.
const IDLE: Duration = Duration::from_secs(30);
/// Claims before a job that keeps failing on this side is marked failed.
const ATTEMPTS: u32 = 3;
/// The processing budget's unit.
const MIB: u64 = 1 << 20;

/// Why a job was not made.
#[derive(Debug)]
enum Problem {
    /// The file is at fault: failed at once, with a key for why
    /// (`jobs::FAILED_*`) and what to log.
    Rejected(&'static str, String),
    /// Something on this side failed: tried again later.
    Failed(String),
    /// Deleted while it was made.
    Stopped,
}

pub struct Worker {
    db: Db,
    store: Store,
    tools: Option<Tools>,
    state_dir: PathBuf,
    /// Who holds a claim: this run of this process.
    name: String,
    budget: Arc<Semaphore>,
    budget_mib: u32,
    max_pixels: u64,
    max_seconds: u64,
    pictures: Arc<Semaphore>,
    videos: Arc<Semaphore>,
    events: mpsc::UnboundedSender<FromWorker>,
    wake: Notify,
    /// The jobs being made, and how to stop each.
    running: parking_lot::Mutex<HashMap<String, watch::Sender<bool>>>,
    /// What was last said of each job's progress, so a step is said once.
    said: parking_lot::Mutex<HashMap<String, u16>>,
}

/// Run until the web process goes away (an error, so the process exits and
/// is started again) or a signal to stop (released claims, and `Ok`).
pub async fn run(config: &Config, store: Store) -> anyhow::Result<()> {
    let path = jobs::socket(&config.state_dir);
    let stream = connect(&path).await;
    let db = Db::open(&config.state_dir.join("dither.db"))
        .await
        .context("opening the database")?;
    let tools = Tools::find(&config.video).await?;
    std::fs::create_dir_all(jobs::dir(&config.state_dir))?;
    let (events, mut said) = mpsc::unbounded_channel();
    let mib = (config.process_memory / MIB).clamp(1, u64::from(u32::MAX)) as u32;
    let worker = Arc::new(Worker {
        db,
        store,
        tools,
        state_dir: config.state_dir.clone(),
        name: crate::id::new(),
        budget: Arc::new(Semaphore::new(mib as usize)),
        budget_mib: mib,
        max_pixels: config.max_pixels,
        max_seconds: config.video.max_seconds,
        pictures: Arc::new(Semaphore::new(config.workers.pictures.max(1))),
        videos: Arc::new(Semaphore::new(config.workers.videos.max(1))),
        events,
        wake: Notify::new(),
        running: Default::default(),
        said: Default::default(),
    });
    tracing::info!(
        worker = %worker.name,
        videos = worker.tools.is_some(),
        "working"
    );
    worker.say(FromWorker::Hello {
        videos: worker.tools.is_some(),
    });
    let (read, mut write) = stream.into_split();
    let writer = async {
        while let Some(event) = said.recv().await {
            let mut line = serde_json::to_string(&event).expect("serialisable");
            line.push('\n');
            write.write_all(line.as_bytes()).await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let reader = {
        let worker = worker.clone();
        async move {
            let mut lines = tokio::io::BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await? {
                match serde_json::from_str(&line) {
                    Ok(ToWorker::Wake) => worker.wake.notify_one(),
                    Ok(ToWorker::Cancel { ids }) => worker.cancel(&ids),
                    Err(e) => {
                        tracing::warn!(error = %e, "the web process said something unreadable")
                    }
                }
            }
            Ok::<_, std::io::Error>(())
        }
    };
    let outcome = tokio::select! {
        r = writer => r.map_err(anyhow::Error::from).and(Err(anyhow::anyhow!("the web process went away"))),
        r = reader => r.map_err(anyhow::Error::from).and(Err(anyhow::anyhow!("the web process went away"))),
        () = worker.clone().claim_forever() => unreachable!("claims until stopped"),
        () = crate::signal() => Ok(()),
    };
    worker.let_go().await;
    outcome
}

/// The web process's socket, once it listens.
async fn connect(path: &Path) -> tokio::net::UnixStream {
    let mut said = false;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => return stream,
            Err(e) => {
                if !said {
                    tracing::info!(socket = %path.display(), error = %e, "waiting for the web process");
                    said = true;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

fn now() -> i64 {
    crate::db::now()
}

impl Worker {
    fn cancel(&self, ids: &[String]) {
        for id in ids {
            if let Some(stop) = self.running.lock().get(id) {
                let _ = stop.send(true);
            }
        }
    }

    /// Give back every claim, for the next worker to take up at once: those
    /// being made, and one a claim stopped part way still made.
    async fn let_go(&self) {
        if let Err(e) = self.db.release_all(&self.name).await {
            tracing::warn!(error = %e, "giving back claims failed; their leases will lapse");
        }
    }

    fn say(&self, event: FromWorker) {
        let _ = self.events.send(event);
    }

    /// Say job `id` is `done` thousandths made. Never backwards: ffmpeg's
    /// time out wavers as it interleaves sound and picture.
    fn progress(&self, owner: &str, id: &str, done: u16) {
        {
            let mut said = self.said.lock();
            let last = said.get(id).copied();
            if last.is_some_and(|last| done <= last) {
                return;
            }
            said.insert(id.to_string(), done);
        }
        self.say(FromWorker::Progress {
            owner: owner.to_string(),
            id: id.to_string(),
            done,
        });
    }

    async fn claim_forever(self: Arc<Self>) {
        loop {
            let mut claimed = false;
            for video in [false, true] {
                let slots = if video { &self.videos } else { &self.pictures };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let until = now() + LEASE.as_millis() as i64;
                match self.db.claim(&self.name, video, until).await {
                    Ok(Some(job)) => {
                        claimed = true;
                        let this = self.clone();
                        tokio::spawn(async move {
                            this.clone().make(job).await;
                            drop(slot);
                            this.wake.notify_one();
                        });
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "claiming a job failed"),
                }
            }
            if !claimed {
                tokio::select! {
                    () = self.wake.notified() => {}
                    () = tokio::time::sleep(IDLE) => {}
                }
            }
        }
    }

    /// Make a claimed job, and record how it ended.
    async fn make(self: Arc<Self>, job: Claimed) {
        let id = job.image.id.clone();
        let owner = job.owner.clone();
        let video = job.image.is_video();
        let started = Instant::now();
        // A job that has killed workers before is not tried again.
        let outcome = if job.image.attempts > ATTEMPTS {
            Err(Problem::Rejected(
                jobs::FAILED,
                format!("claimed {} times without ending", job.image.attempts - 1),
            ))
        } else if video && self.tools.is_none() {
            Err(Problem::Rejected(
                jobs::FAILED_UNSUPPORTED,
                "this worker has no ffmpeg".into(),
            ))
        } else {
            let (stop, stopped) = watch::channel(false);
            self.running.lock().insert(id.clone(), stop.clone());
            // The lease is renewed while the job is made; one that cannot be
            // was deleted, or lapsed and taken by another, and this stops.
            let heartbeat = {
                let this = self.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    let mut renewed = Instant::now();
                    loop {
                        tokio::time::sleep(RENEW).await;
                        let until = now() + LEASE.as_millis() as i64;
                        match this.db.renew(&this.name, &id, until).await {
                            Ok(true) => renewed = Instant::now(),
                            Ok(false) => break,
                            // Unrenewed for a lease's length, it may be
                            // another's by now.
                            Err(_) if renewed.elapsed() >= LEASE => break,
                            Err(e) => {
                                tracing::warn!(job = %id, error = %e, "renewing a lease failed")
                            }
                        }
                    }
                    let _ = stop.send(true);
                })
            };
            self.progress(&owner, &id, 0);
            let outcome = if video {
                self.video(&job, stopped).await
            } else {
                self.picture(&job, stopped).await
            };
            heartbeat.abort();
            self.running.lock().remove(&id);
            outcome
        };
        self.said.lock().remove(&id);
        let _ = tokio::fs::remove_dir_all(self.work(&id)).await;
        let ended = |made: bool, bytes: u64, why: &str| FromWorker::Ended {
            owner: owner.clone(),
            id: id.clone(),
            made,
            video,
            ms: started.elapsed().as_millis() as u64,
            bytes,
            failure: why.to_string(),
        };
        // Whether the copies are kept, should this worker record the end.
        let keep = outcome.is_ok();
        let recorded = match outcome {
            Ok(made) => {
                let bytes = made.bytes;
                let finished = self.db.finish_job(&owner, &self.name, &id, made).await;
                if let Ok(true) = finished {
                    tracing::info!(job = %id, video, ms = started.elapsed().as_millis() as u64, "made");
                    self.say(ended(true, bytes, ""));
                }
                finished
            }
            // Deleted, or no longer this worker's: what to tidy is decided below.
            Err(Problem::Stopped) => Ok(false),
            Err(Problem::Failed(why)) if job.image.attempts < ATTEMPTS => {
                tracing::warn!(job = %id, attempt = job.image.attempts, error = %why, "a job failed; it will be tried again");
                self.later(&job).await;
                return;
            }
            Err(problem) => {
                let why = match problem {
                    Problem::Rejected(key, detail) => {
                        tracing::info!(job = %id, reason = %detail, "a job's file was refused");
                        key
                    }
                    Problem::Failed(why) => {
                        tracing::error!(job = %id, error = %why, "a job failed for good");
                        jobs::FAILED
                    }
                    Problem::Stopped => unreachable!("matched above"),
                };
                let failed = self.db.fail_job(&owner, &self.name, &id, why).await;
                if let Ok(true) = failed {
                    self.say(ended(false, 0, why));
                }
                failed
            }
        };
        match recorded {
            // Ended here: the upload goes, and a failure's copies with it.
            Ok(true) => {
                let _ = tokio::fs::remove_file(self.source(&id)).await;
                if !keep {
                    store::discard(&self.store, &self.db, vec![id.clone()]).await;
                }
            }
            // Not this worker's to record. Deleted: everything goes. Taken by
            // another worker after this one's lease lapsed: everything stays,
            // the copies included, which are that worker's now.
            Ok(false) => match self.db.image(&id).await {
                Ok(None) => {
                    let _ = tokio::fs::remove_file(self.source(&id)).await;
                    store::discard(&self.store, &self.db, vec![id.clone()]).await;
                }
                // Stopped while still this worker's (its lease could not be
                // renewed): given back, since a worker does not take up a
                // job it holds.
                _ => {
                    let _ = self.db.release(&self.name, &id, 0).await;
                }
            },
            Err(e) => {
                tracing::error!(job = %id, error = %e, "recording how a job ended failed");
                self.later(&job).await;
            }
        }
    }

    /// Put a job back to be tried again, later with each attempt, and say
    /// it waits, so its album does not show it stuck part way.
    async fn later(&self, job: &Claimed) {
        let wait = Duration::from_secs(30) * job.image.attempts.max(1);
        let after = now() + wait.as_millis() as i64;
        if let Err(e) = self.db.release(&self.name, &job.image.id, after).await {
            tracing::warn!(job = %job.image.id, error = %e, "putting a job back failed; its lease will lapse");
        }
        self.say(FromWorker::Waiting {
            owner: job.owner.clone(),
            id: job.image.id.clone(),
        });
    }

    fn source(&self, id: &str) -> PathBuf {
        jobs::source(&self.state_dir, id)
    }

    /// Where this worker works on job `id`: its own, so a worker that lost
    /// the job and one that took it never share one.
    fn work(&self, id: &str) -> PathBuf {
        jobs::work(&self.state_dir, &format!("{id}.{}", self.name))
    }

    /// `cost` bytes of the processing budget, until what is returned drops.
    async fn hold(&self, cost: u64) -> Result<tokio::sync::SemaphorePermit<'_>, Problem> {
        let permits = u32::try_from(cost.div_ceil(MIB)).unwrap_or(u32::MAX).max(1);
        if permits > self.budget_mib {
            return Err(Problem::Rejected(
                jobs::FAILED_TOO_LARGE,
                format!("it needs {cost} bytes to process"),
            ));
        }
        self.budget
            .acquire_many(permits)
            .await
            .map_err(|_| Problem::Failed("stopping".into()))
    }

    /// The upload's file: its first `limit` bytes, or all of it.
    async fn upload(&self, id: &str, limit: Option<u64>) -> Result<Vec<u8>, Problem> {
        let lost = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Problem::Rejected(
                    jobs::FAILED,
                    "its upload was lost before it was made".into(),
                )
            } else {
                Problem::Failed(e.to_string())
            }
        };
        let path = self.source(id);
        match limit {
            None => tokio::fs::read(&path).await.map_err(lost),
            Some(n) => crate::media::read_head(&path, n).await.map_err(lost),
        }
    }

    /// A picture: cleaned and resized under the memory budget, as sized
    /// from its header.
    async fn picture(&self, job: &Claimed, stop: watch::Receiver<bool>) -> Result<Made, Problem> {
        use crate::media::{self, Reject};
        let id = job.image.id.as_str();
        let size = job.image.bytes;
        let refused = |r: Reject| match r {
            Reject::TooBig(why) => Problem::Rejected(jobs::FAILED_TOO_LARGE, why),
            Reject::Unsupported(why) => Problem::Rejected(jobs::FAILED_UNREADABLE, why.into()),
            Reject::Broken(why) => Problem::Rejected(jobs::FAILED_UNREADABLE, why),
        };
        let cost = match media::cost_of(&self.source(id), size, self.max_pixels).await {
            Ok(cost) => cost,
            Err(media::Unsized::Refused(r)) => return Err(refused(r)),
            Err(media::Unsized::Unread(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Problem::Rejected(
                    jobs::FAILED,
                    "its upload was lost before it was made".into(),
                ));
            }
            Err(media::Unsized::Unread(e)) => return Err(Problem::Failed(e.to_string())),
        };
        let _held = self.hold(cost).await?;
        if *stop.borrow() {
            return Err(Problem::Stopped);
        }
        let bytes = self.upload(id, None).await?;
        let max = self.max_pixels;
        let processed = tokio::task::spawn_blocking(move || media::process(&bytes, max))
            .await
            .map_err(|e| Problem::Failed(format!("processing panicked: {e}")))?
            .map_err(refused)?;
        if *stop.borrow() {
            return Err(Problem::Stopped);
        }
        let made = Made {
            width: processed.width,
            height: processed.height,
            bytes: processed.original.len() as u64,
            animated: processed.animated,
            display: processed.display.is_some(),
            sha256: processed.sha256.to_vec(),
            duration_ms: None,
            audio: false,
        };
        let mut puts = vec![
            (Variant::Original, Bytes::from(processed.original)),
            (Variant::Thumb, Bytes::from(processed.thumb)),
            (Variant::Preview, Bytes::from(processed.preview)),
        ];
        if let Some(d) = processed.display {
            puts.push((Variant::Display, Bytes::from(d)));
        }
        self.db
            .add_stray(id)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        for (variant, bytes) in puts {
            self.store
                .put(&store::key(variant, id), bytes)
                .await
                .map_err(|e| Problem::Failed(format!("storing: {e}")))?;
        }
        Ok(made)
    }

    /// A video: probed, remuxed without metadata, given a playback copy when
    /// browsers would not play it, and a frame for its pictures.
    async fn video(&self, job: &Claimed, stop: watch::Receiver<bool>) -> Result<Made, Problem> {
        // ffmpeg failing on a file it probed, or running past a limit
        // generous to the slowest machine, is the file's doing: retrying
        // would hold the video slot for hours to the same end. Not starting
        // at all is this side's.
        let failed = |what: &'static str| {
            move |e: String| {
                if e == "stopped" {
                    Problem::Stopped
                } else if e.starts_with(video::LOCAL) {
                    Problem::Failed(format!("{what}: {e}"))
                } else {
                    Problem::Rejected(jobs::FAILED_UNREADABLE, format!("{what}: {e}"))
                }
            }
        };
        let tools = self
            .tools
            .as_ref()
            .ok_or_else(|| Problem::Failed("this worker has no ffmpeg".into()))?;
        let id = job.image.id.as_str();
        let owner = job.owner.as_str();
        let source = self.source(id);
        let head = self.upload(id, Some(16)).await?;
        let container = crate::sanitize::sniff_video(&head).ok_or_else(|| {
            Problem::Rejected(jobs::FAILED_UNREADABLE, "not a video after all".into())
        })?;
        let probe = tools.probe(container, &source).await.map_err(|e| {
            // Out of time, or trouble running ffprobe: this side's.
            if e.starts_with("timed out") || e.starts_with(video::LOCAL) {
                Problem::Failed(format!("probing: {e}"))
            } else {
                Problem::Rejected(jobs::FAILED_UNREADABLE, e)
            }
        })?;
        if probe.duration > self.max_seconds as f64 {
            return Err(Problem::Rejected(
                jobs::FAILED_TOO_LONG,
                format!("{} long", video::clock(probe.duration)),
            ));
        }
        let (width, height) = probe.shown();
        let pixels = u64::from(width) * u64::from(height);
        if pixels == 0 || pixels > video::MAX_PIXELS.min(self.max_pixels) {
            return Err(Problem::Rejected(
                jobs::FAILED_TOO_LARGE,
                format!("{width}×{height}"),
            ));
        }
        if !video::decodable(&probe.video.codec) {
            return Err(Problem::Rejected(
                jobs::FAILED_UNREADABLE,
                format!("its video is {}", probe.video.codec),
            ));
        }
        let size = tokio::fs::metadata(&source)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let work = self.work(id);
        let _ = tokio::fs::remove_dir_all(&work).await;
        tokio::fs::create_dir_all(&work)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;

        // Kept as recorded unless MP4 cannot hold its video at all.
        let fits = video::fits_mp4(&probe.video.codec);
        let playable = probe.playable();
        let original = work.join("original.mp4");
        if fits {
            tools
                .remux(container, &probe, &source, &original, size, &stop)
                .await
                .map_err(failed("remuxing"))?;
        }
        let transcoded = !playable || !fits;
        self.progress(owner, id, if transcoded { 100 } else { 900 });

        let play = work.join("play.mp4");
        if transcoded {
            let duration = probe.duration;
            let mut report = |secs: f64| {
                let share = (secs / duration).clamp(0.0, 1.0);
                self.progress(owner, id, (100.0 + share * 850.0) as u16);
            };
            tools
                .transcode(container, &probe, &source, &play, &stop, &mut report)
                .await
                .map_err(failed("transcoding"))?;
        }
        // MP4 cannot hold the original's video: the playback copy is the
        // only copy.
        let (original, display) = match (fits, transcoded) {
            (true, true) => (original, Some(play)),
            (true, false) => (original, None),
            (false, _) => (play, None),
        };
        let playback = display.as_deref().unwrap_or(&original);

        let at = (probe.duration / 3.0).min(1.0);
        let png = tools
            .frame(playback, at, &stop)
            .await
            .map_err(failed("taking a frame"))?;
        let cost = crate::media::cost(&png, png.len() as u64, self.max_pixels)
            .map_err(|e| Problem::Failed(format!("the frame: {e}")))?;
        let still = {
            let _held = self.hold(cost).await?;
            let max = self.max_pixels;
            tokio::task::spawn_blocking(move || crate::media::stills(&png, max))
                .await
                .map_err(|e| Problem::Failed(e.to_string()))?
                .map_err(|e| Problem::Failed(format!("the frame: {e}")))?
        };
        self.progress(owner, id, 960);

        let stored = async {
            self.db.add_stray(id).await?;
            let fingerprint = self
                .store
                .put_file(&store::key(Variant::Original, id), &original)
                .await?;
            if let Some(display) = &display {
                self.store
                    .put_file(&store::key(Variant::Display, id), display)
                    .await?;
            }
            self.store
                .put(&store::key(Variant::Thumb, id), Bytes::from(still.thumb))
                .await?;
            self.store
                .put(
                    &store::key(Variant::Preview, id),
                    Bytes::from(still.preview),
                )
                .await?;
            Ok::<_, anyhow::Error>(fingerprint)
        };
        let (bytes, sha256) = stored
            .await
            .map_err(|e| Problem::Failed(format!("storing: {e:#}")))?;
        self.progress(owner, id, 1000);
        // The playback copy standing in for the original has its own size.
        let (width, height) = if fits {
            (width, height)
        } else {
            tools
                .probe(Container::Mp4, &original)
                .await
                .map_or((width, height), |p| p.shown())
        };
        Ok(Made {
            width,
            height,
            bytes,
            animated: false,
            display: display.is_some(),
            sha256: sha256.to_vec(),
            duration_ms: Some((probe.duration * 1000.0).round() as u64),
            audio: probe.audio.is_some(),
        })
    }
}
