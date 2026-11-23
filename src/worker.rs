//! `dither worker`: the process that makes uploads' copies. It runs wherever
//! it reaches the web process's worker API (`web::work`), as many as there
//! are machines to spare. It holds none of the web process's secrets, and
//! neither the database nor the store: it asks for a job, fetches the upload,
//! makes it, sends what it made and says how it ended.
//!
//! One sharing the web process's state directory, as `just dev` runs it,
//! reads an upload where it waits rather than fetching a copy.
//!
//! A job is held with a lease, renewed while the job is made; a renewal that
//! is refused means the job was deleted or taken by another worker, and this
//! one stops. Pictures are made several at a time under the memory budget,
//! sized from each one's header; videos one at a time by default, since
//! ffmpeg's memory is its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{Semaphore, watch};

use crate::config::Config;
use crate::db::Made;
use crate::jobs::{self, Job, LEASE, Outcome, Want};
use crate::sanitize::Container;
use crate::store::Variant;
use crate::video::{self, Tools};

/// How often a job's claim is renewed when there is no progress to report.
const RENEW: Duration = Duration::from_secs(20);
/// How often progress is reported, at most.
const REPORT: Duration = Duration::from_secs(1);
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
    /// Deleted while it was made, or no longer this worker's.
    Stopped,
}

pub struct Worker {
    http: reqwest::Client,
    /// The web process's worker API.
    queue: String,
    token: Option<String>,
    tools: Option<Tools>,
    state_dir: PathBuf,
    /// Who holds a claim: this run of this process.
    name: String,
    budget: Arc<Semaphore>,
    budget_mib: u32,
    pictures: Arc<Semaphore>,
    videos: Arc<Semaphore>,
    /// Whether it makes pictures: it has slots for them.
    makes_pictures: bool,
    /// Whether it makes videos: it has ffmpeg, and slots for them.
    makes_videos: bool,
    /// The jobs being made now.
    making: parking_lot::Mutex<std::collections::HashSet<String>>,
}

/// A job being made: whether to stop, and how far along it is.
struct Making {
    stop: watch::Receiver<bool>,
    done: watch::Sender<u16>,
}

impl Making {
    /// `done` thousandths made. Never backwards: ffmpeg's time out wavers as
    /// it interleaves sound and picture.
    fn report(&self, done: u16) {
        self.done.send_if_modified(|d| {
            let ahead = done > *d;
            if ahead {
                *d = done;
            }
            ahead
        });
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }
}

/// Run until a signal to stop, when the claims held are given back.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    let tools = Tools::find(&config.video).await?;
    std::fs::create_dir_all(jobs::dir(&config.state_dir))?;
    let mib = (config.process_memory / MIB).clamp(1, u64::from(u32::MAX)) as u32;
    let worker = Arc::new(Worker {
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Longer than a claim is held waiting, so only a connection gone
            // quiet mid-transfer trips it.
            .read_timeout(jobs::POLL + Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?,
        queue: config.queue_url.trim_end_matches('/').to_string(),
        token: config.worker_token.clone(),
        state_dir: config.state_dir.clone(),
        name: crate::id::new(),
        budget: Arc::new(Semaphore::new(mib as usize)),
        budget_mib: mib,
        pictures: Arc::new(Semaphore::new(config.workers.pictures)),
        videos: Arc::new(Semaphore::new(config.workers.videos)),
        makes_pictures: config.workers.pictures > 0,
        makes_videos: tools.is_some() && config.workers.videos > 0,
        tools,
        making: Default::default(),
    });
    worker.tidy().await;
    tracing::info!(
        worker = %worker.name,
        queue = %worker.queue,
        pictures = config.workers.pictures,
        videos = if worker.makes_videos { config.workers.videos } else { 0 },
        "working"
    );
    let videos = async {
        if worker.makes_videos {
            worker.clone().claim_forever(true).await;
        }
        std::future::pending::<()>().await;
    };
    tokio::select! {
        () = worker.clone().claim_forever(false) => unreachable!("claims until stopped"),
        () = videos => unreachable!("claims until stopped"),
        () = crate::signal() => {}
    }
    worker.let_go().await;
    Ok(())
}

impl Worker {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self
            .http
            .request(method, format!("{}{path}", self.queue))
            .header("x-worker", &self.name);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Give back every claim, for the next worker to take up at once.
    async fn let_go(&self) {
        let given = self
            .request(reqwest::Method::POST, "/work/release")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        if let Err(e) = given {
            tracing::warn!(error = %e, "giving back claims failed; their leases will lapse");
        }
    }

    /// Ask for jobs of one kind, `video` or pictures, whenever there is a
    /// slot for one, and make each.
    async fn claim_forever(self: Arc<Self>, video: bool) {
        let slots = if video { &self.videos } else { &self.pictures };
        let mut unreachable = false;
        loop {
            let Ok(slot) = slots.clone().acquire_owned().await else {
                return;
            };
            let want = Want {
                pictures: !video,
                videos: video,
                makes_pictures: self.makes_pictures,
                makes_videos: self.makes_videos,
                making: self.making.lock().iter().cloned().collect(),
            };
            match self.ask(&want).await {
                Ok(Some(job)) => {
                    unreachable = false;
                    let this = self.clone();
                    tokio::spawn(async move {
                        this.make(job).await;
                        drop(slot);
                    });
                }
                Ok(None) => unreachable = false,
                Err(e) => {
                    if !unreachable {
                        tracing::warn!(queue = %self.queue, error = %e, "the queue cannot be reached; trying again");
                        unreachable = true;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    async fn ask(&self, want: &Want) -> anyhow::Result<Option<Job>> {
        let response = self
            .request(reqwest::Method::POST, "/work/claim")
            .json(want)
            .timeout(jobs::POLL + Duration::from_secs(15))
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::OK => Ok(Some(response.json().await?)),
            reqwest::StatusCode::NO_CONTENT => Ok(None),
            status => anyhow::bail!("the queue answered {status}"),
        }
    }

    /// Renew job `id`'s claim, saying it is `done` thousandths made: whether
    /// it is still this worker's.
    async fn renew(&self, id: &str, done: Option<u16>) -> anyhow::Result<bool> {
        let response = self
            .request(reqwest::Method::POST, &format!("/work/{id}/renew"))
            .json(&serde_json::json!({ "done": done }))
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(true),
            reqwest::StatusCode::GONE => Ok(false),
            status => anyhow::bail!("the queue answered {status}"),
        }
    }

    /// Send job `id`'s copy of `variant`.
    async fn put(&self, id: &str, variant: Variant, body: reqwest::Body) -> Result<(), Problem> {
        let name = variant.name();
        let response = self
            .request(reqwest::Method::PUT, &format!("/work/{id}/files/{name}"))
            .body(body)
            .send()
            .await
            .map_err(|e| Problem::Failed(format!("sending its {name}: {e}")))?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(()),
            reqwest::StatusCode::GONE => Err(Problem::Stopped),
            status => Err(Problem::Failed(format!(
                "sending its {name}: the queue answered {status}"
            ))),
        }
    }

    /// Say how job `id` ended, until the web process hears it: it may be
    /// restarting. Unheard, the job's lease lapses and another worker makes
    /// it again.
    async fn end(&self, id: &str, outcome: Outcome, ms: u64) {
        let mut body = serde_json::to_value(&outcome).expect("serialisable");
        body["ms"] = ms.into();
        for attempt in 1..=5u32 {
            let said = self
                .request(reqwest::Method::POST, &format!("/work/{id}/end"))
                .json(&body)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status);
            match said {
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!(job = %id, attempt, error = %e, "saying how a job ended failed");
                    tokio::time::sleep(Duration::from_secs(2) * attempt).await;
                }
            }
        }
    }

    /// Make a claimed job, and say how it ended.
    async fn make(self: Arc<Self>, job: Job) {
        let id = job.id.clone();
        self.making.lock().insert(id.clone());
        let started = Instant::now();
        let (stop, stopped) = watch::channel(false);
        let (done, mut reported) = watch::channel(0u16);
        // The claim is kept while the job is made: progress is reported as
        // it moves, and the lease renewed in between. One that cannot be
        // was deleted, or lapsed and taken by another, and this stops.
        let keeper = {
            let this = self.clone();
            let id = id.clone();
            tokio::spawn(async move {
                let mut renewed = Instant::now();
                loop {
                    tokio::time::sleep(REPORT).await;
                    let moved = reported.has_changed().unwrap_or(false);
                    if !moved && renewed.elapsed() < RENEW {
                        continue;
                    }
                    let done = moved.then(|| *reported.borrow_and_update());
                    match this.renew(&id, done).await {
                        Ok(true) => renewed = Instant::now(),
                        Ok(false) => break,
                        // Unrenewed for a lease's length, it may be
                        // another's by now.
                        Err(_) if renewed.elapsed() >= LEASE => break,
                        Err(e) => {
                            tracing::warn!(job = %id, error = %e, "renewing a claim failed")
                        }
                    }
                }
                let _ = stop.send(true);
            })
        };
        let making = Making {
            stop: stopped,
            done,
        };
        let outcome = match self.fetch(&job).await {
            Ok(source) if job.video => self.video(&job, &source, &making).await,
            Ok(source) => self.picture(&job, &source, &making).await,
            Err(problem) => Err(problem),
        };
        keeper.abort();
        let _ = tokio::fs::remove_dir_all(self.work(&id)).await;
        let outcome = match outcome {
            Ok(made) => Outcome::Made { made },
            Err(Problem::Rejected(key, detail)) => Outcome::Refused {
                key: key.into(),
                detail,
            },
            Err(Problem::Failed(why)) => Outcome::Failed { why },
            Err(Problem::Stopped) => Outcome::Stopped,
        };
        self.end(&id, outcome, started.elapsed().as_millis() as u64)
            .await;
        self.making.lock().remove(&id);
    }

    /// Remove what a run before this one left in its scratch: it stopped
    /// mid-job, killed or out of memory, and its name died with it. Only
    /// what nothing has written to in a while, in case another worker shares
    /// the directory.
    async fn tidy(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(jobs::dir(&self.state_dir)).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "work") {
                continue;
            }
            let mut newest = entry.metadata().await.ok().and_then(|m| m.modified().ok());
            if let Ok(mut inside) = tokio::fs::read_dir(&path).await {
                while let Ok(Some(file)) = inside.next_entry().await {
                    let modified = file.metadata().await.ok().and_then(|m| m.modified().ok());
                    newest = newest.max(modified);
                }
            }
            let idle = newest
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(3600));
            if idle {
                let _ = tokio::fs::remove_dir_all(&path).await;
            }
        }
    }

    /// Job `id`'s upload: where it waits, when this worker shares the web
    /// process's state directory, or else fetched, carrying on from where a
    /// fetch cut short stopped.
    async fn fetch(&self, job: &Job) -> Result<PathBuf, Problem> {
        use tokio::io::AsyncWriteExt as _;
        let local = jobs::source(&self.state_dir, &job.id);
        if tokio::fs::metadata(&local)
            .await
            .is_ok_and(|m| m.len() == job.bytes)
        {
            return Ok(local);
        }
        let work = self.work(&job.id);
        let _ = tokio::fs::remove_dir_all(&work).await;
        tokio::fs::create_dir_all(&work)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        let path = work.join("source");
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        let mut have = 0u64;
        let mut last = String::new();
        for _ in 0..5 {
            let response = self
                .request(reqwest::Method::GET, &format!("/work/{}/source", job.id))
                .header(reqwest::header::RANGE, format!("bytes={have}-"))
                .send()
                .await;
            let mut response = match response {
                Ok(r) if r.status() == reqwest::StatusCode::GONE => return Err(Problem::Stopped),
                Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                    return Err(Problem::Rejected(
                        jobs::FAILED,
                        "its upload was lost before it was made".into(),
                    ));
                }
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    last = format!("the queue answered {}", r.status());
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                Err(e) => {
                    last = e.to_string();
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        file.write_all(&chunk)
                            .await
                            .map_err(|e| Problem::Failed(format!("keeping its upload: {e}")))?;
                        have += chunk.len() as u64;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        last = e.to_string();
                        break;
                    }
                }
            }
            if have >= job.bytes {
                break;
            }
        }
        file.flush()
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        if have != job.bytes {
            return Err(Problem::Failed(format!(
                "fetched {have} of its {} bytes: {last}",
                job.bytes
            )));
        }
        Ok(path)
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

    /// A picture: cleaned and resized under the memory budget, as sized
    /// from its header.
    async fn picture(&self, job: &Job, source: &Path, making: &Making) -> Result<Made, Problem> {
        use crate::media::{self, Reject};
        let id = job.id.as_str();
        let size = job.bytes;
        let refused = |r: Reject| match r {
            Reject::TooBig(why) => Problem::Rejected(jobs::FAILED_TOO_LARGE, why),
            Reject::Unsupported(why) => Problem::Rejected(jobs::FAILED_UNREADABLE, why.into()),
            Reject::Broken(why) => Problem::Rejected(jobs::FAILED_UNREADABLE, why),
        };
        let cost = match media::cost_of(source, size, job.max_pixels).await {
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
        if making.stopped() {
            return Err(Problem::Stopped);
        }
        let bytes = tokio::fs::read(source)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        let max = job.max_pixels;
        let processed = tokio::task::spawn_blocking(move || media::process(&bytes, max))
            .await
            .map_err(|e| Problem::Failed(format!("processing panicked: {e}")))?
            .map_err(refused)?;
        if making.stopped() {
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
        for (variant, bytes) in puts {
            self.put(id, variant, bytes.into()).await?;
        }
        Ok(made)
    }

    /// A video: probed, kept as recorded or converted to H.264 (either way
    /// without metadata), and a frame of it taken for its pictures.
    async fn video(&self, job: &Job, source: &Path, making: &Making) -> Result<Made, Problem> {
        // ffmpeg failing on a file it probed, or running past a limit
        // generous to the slowest machine, is the file's doing: retrying
        // would hold the video slot for hours to the same end. Not starting
        // at all is this side's.
        let failed = |what: &'static str| {
            move |e: String| {
                if e == "stopped" {
                    Problem::Stopped
                } else if e.starts_with(video::LOCAL) || e.contains("No space left on device") {
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
        let id = job.id.as_str();
        let stop = &making.stop;
        let head = crate::media::read_head(source, 16)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        let container = crate::sanitize::sniff_video(&head).ok_or_else(|| {
            Problem::Rejected(jobs::FAILED_UNREADABLE, "not a video after all".into())
        })?;
        let probe = tools.probe(container, source).await.map_err(|e| {
            // Out of time, or trouble running ffprobe: this side's.
            if e.starts_with("timed out") || e.starts_with(video::LOCAL) {
                Problem::Failed(format!("probing: {e}"))
            } else {
                Problem::Rejected(jobs::FAILED_UNREADABLE, e)
            }
        })?;
        if probe.duration > job.max_seconds as f64 {
            return Err(Problem::Rejected(
                jobs::FAILED_TOO_LONG,
                format!("{} long", video::clock(probe.duration)),
            ));
        }
        let (width, height) = probe.shown();
        let pixels = u64::from(width) * u64::from(height);
        if pixels == 0 || pixels > video::MAX_PIXELS.min(job.max_pixels) {
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
        let size = tokio::fs::metadata(source)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let work = self.work(id);
        tokio::fs::create_dir_all(&work)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;

        // One copy is kept: as recorded, when asked for and MP4 holds it, or
        // H.264 at its own size, which plays everywhere. A video already
        // H.264, 8-bit and SDR is that as it stands, and is not re-encoded.
        let fits = video::fits_mp4(&probe.video.codec);
        let kept = fits && (job.as_recorded || probe.compatible());
        let made = work.join("video.mp4");
        if kept {
            tools
                .remux(container, &probe, source, &made, size, stop)
                .await
                .map_err(failed("remuxing"))?;
            making.report(900);
        } else {
            let duration = probe.duration;
            let mut report = |secs: f64| {
                let share = (secs / duration).clamp(0.0, 1.0);
                making.report((share * 950.0) as u16);
            };
            tools
                .transcode(container, &probe, source, &made, stop, &mut report)
                .await
                .map_err(failed("transcoding"))?;
        }
        // What was made is what is stored, plays and downloads: its own size
        // and length, read back, or failing that the upload's, scaled as a
        // conversion scales it.
        let stored = match tools.probe(Container::Mp4, &made).await {
            Ok(stored) => stored,
            Err(e) => {
                tracing::warn!(id, error = %e, "reading back what was made");
                let mut guess = probe.clone();
                if !kept {
                    let (w, h) = (guess.video.width, guess.video.height);
                    let f = video::shrink(w, h, guess.video.fps.clamp(1.0, 60.0));
                    let even = |n: u32| ((f64::from(n) * f / 2.0).trunc() * 2.0) as u32;
                    (guess.video.width, guess.video.height) = (even(w), even(h));
                }
                guess
            }
        };
        // Kept in HDR, its frame is tone-mapped where the build can; should
        // that fail, a frame as it is beats no picture at all.
        let hdr = kept && stored.hdr();
        let at = (stored.duration / 3.0).min(1.0);
        let png = match tools.frame(&made, at, hdr, stop).await {
            Err(e) if hdr && !making.stopped() => {
                tracing::warn!(id, error = %e, "tone-mapping a frame");
                tools.frame(&made, at, false, stop).await
            }
            done => done,
        }
        .map_err(failed("taking a frame"))?;
        let cost = crate::media::cost(&png, png.len() as u64, job.max_pixels)
            .map_err(|e| Problem::Failed(format!("the frame: {e}")))?;
        let still = {
            let _held = self.hold(cost).await?;
            let max = job.max_pixels;
            tokio::task::spawn_blocking(move || crate::media::stills(&png, max))
                .await
                .map_err(|e| Problem::Failed(e.to_string()))?
                .map_err(|e| Problem::Failed(format!("the frame: {e}")))?
        };
        making.report(960);

        let video = tokio::fs::File::open(&made)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        self.put(id, Variant::Original, video.into()).await?;
        self.put(id, Variant::Thumb, Bytes::from(still.thumb).into())
            .await?;
        self.put(id, Variant::Preview, Bytes::from(still.preview).into())
            .await?;
        making.report(1000);
        let (width, height) = stored.shown();
        Ok(Made {
            width,
            height,
            // What the web process stored stands for these.
            bytes: 0,
            animated: false,
            display: false,
            sha256: Vec::new(),
            duration_ms: Some((stored.duration * 1000.0).round() as u64),
            audio: stored.audio.is_some(),
        })
    }
}
