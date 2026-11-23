//! Videos: what ffprobe says of one, and the worker that makes its copies with
//! ffmpeg.
//!
//! A video's original is kept as recorded but remuxed into MP4 with only its
//! first video and audio streams and none of the container's or streams'
//! metadata: a phone writes where a video was taken there, as it does in a
//! photo's EXIF, and in timed metadata tracks beside the picture. Nothing is
//! re-encoded, so the original loses nothing, and the rotation a phone
//! records, which is not metadata but part of the video track, survives.
//!
//! Browsers do not all play what phones record (HEVC, 10-bit, HDR), and a
//! camera's bitrate is too much for a phone on a poor signal, so a video that
//! is not already H.264, 8-bit, SDR, at most 1920 pixels long and of a modest
//! bitrate gets a playback copy that is, tone-mapped from HDR where the
//! ffmpeg build can. A frame from the playback copy becomes the thumbnail and
//! the link preview through the same code as an image's.
//!
//! Transcoding takes minutes, so finishing an upload only probes the file and
//! adds the video as processing, and one worker makes copies a video at a
//! time, reporting its progress to the album. A video interrupted by a
//! restart is still processing, with its upload kept, and is made again.
//!
//! ffmpeg parses untrusted files with a great deal of code, so it runs as a
//! process of its own, told which container it reads rather than left to
//! guess, limited to the file protocol and to the decoders a phone or camera
//! needs, at a lower priority than serving, with a time limit, and holding a
//! share of the processing memory budget while it works.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};
use tokio::sync::{Semaphore, broadcast, mpsc, watch};

use crate::db::{Db, MadeVideo, NewVideo, Status};
use crate::sanitize::Container;
use crate::store::{self, Store, Variant};

/// A playback copy's longer side, and the longest a video plays as uploaded.
pub const PLAY_MAX: u32 = 1920;
/// The highest bitrate a video plays at as uploaded, in bits a second: well
/// within a phone's connection, and above what phones record H.264 at.
const PLAY_BITRATE: u64 = 12_000_000;
/// The x264 quality of a playback copy: visually close to the source at
/// 1080p, at a few megabits a second.
const CRF: &str = "23";
/// Decoders ffmpeg may open: what phones, cameras and screen recorders write.
const DECODERS: &str = "h264,hevc,vp8,vp9,av1,libdav1d,prores,mpeg4,aac,mp3,mp3float,opus,libopus,\
     vorbis,flac,alac,pcm_s16le,pcm_s16be,pcm_s24le,pcm_s24be,pcm_f32le,ac3,eac3";
/// Tone mapping from PQ or HLG to SDR, before the playback copy's own format:
/// to linear light, to BT.709 primaries, Hable's curve, back to BT.709.
const TONEMAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
     tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv";

/// ffmpeg and ffprobe, found and checked at startup.
#[derive(Debug, Clone)]
pub struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    /// Whether the build has zimg's `zscale`, which tone mapping needs.
    zscale: bool,
    threads: u32,
}

impl Tools {
    /// The tools `video` names, if they run and can make a playback copy.
    /// `None` with videos off, or not found under `auto`; an error when they
    /// are required and missing.
    pub async fn find(video: &crate::config::Video) -> anyhow::Result<Option<Self>> {
        use crate::config::VideoMode;
        if video.mode == VideoMode::Off {
            return Ok(None);
        }
        match Self::check(video).await {
            Ok(tools) => {
                tracing::info!(
                    ffmpeg = %tools.ffmpeg.display(),
                    tonemap = tools.zscale,
                    "videos on"
                );
                Ok(Some(tools))
            }
            Err(e) if video.mode == VideoMode::Auto => {
                tracing::info!(reason = %e, "videos off");
                Ok(None)
            }
            Err(e) => Err(e.context("VIDEO=on, but ffmpeg cannot be used")),
        }
    }

    async fn check(video: &crate::config::Video) -> anyhow::Result<Self> {
        output(&video.ffprobe, &["-hide_banner", "-version"]).await?;
        let encoders = output(&video.ffmpeg, &["-hide_banner", "-encoders"]).await?;
        for needed in ["libx264", "aac"] {
            anyhow::ensure!(
                encoders.split_whitespace().any(|w| w == needed),
                "ffmpeg has no {needed} encoder"
            );
        }
        let filters = output(&video.ffmpeg, &["-hide_banner", "-filters"]).await?;
        let has = |name: &str| {
            filters
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
        };
        Ok(Self {
            ffmpeg: video.ffmpeg.clone(),
            ffprobe: video.ffprobe.clone(),
            zscale: has("zscale") && has("tonemap"),
            threads: video.threads,
        })
    }

    /// What every run reading `container` is limited to: the file protocol,
    /// that container's demuxer, the decoders in [`DECODERS`], and ffmpeg
    /// told the format rather than probing for one.
    fn input(container: Container) -> [&'static str; 8] {
        let format = match container {
            Container::Mp4 => "mov",
            Container::Matroska => "matroska",
        };
        [
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            format,
            "-codec_whitelist",
            DECODERS,
            "-f",
            format,
        ]
    }

    /// Run `program` and return what it wrote, or why it failed. With
    /// `progress`, its standard output is ffmpeg's `-progress` report, read
    /// as it comes and passed on as seconds of output made. Stopped at
    /// `limit`, or when `stop` turns true.
    async fn run(
        &self,
        program: &Path,
        args: Vec<OsString>,
        limit: Duration,
        mut stop: watch::Receiver<bool>,
        mut progress: Option<&mut (dyn FnMut(f64) + Send)>,
    ) -> Result<Vec<u8>, String> {
        let mut child = tokio::process::Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("starting {}: {e}", program.display()))?;
        // Serving comes first. Threads ffmpeg starts later take this from
        // the one that starts them.
        if let Some(pid) = child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32))
        {
            let _ = rustix::process::setpriority_process(Some(pid), 10);
        }
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let errors = tokio::spawn(async move {
            let mut all = Vec::new();
            let _ = (&mut stderr).take(64 << 10).read_to_end(&mut all).await;
            // Whatever is left is read and dropped, so ffmpeg never blocks on it.
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            String::from_utf8_lossy(&all).trim().to_string()
        });
        let work = async {
            let mut out = Vec::new();
            match progress.as_deref_mut() {
                Some(report) => {
                    let mut lines = tokio::io::BufReader::new(&mut stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Some(us) = line
                            .strip_prefix("out_time_us=")
                            .and_then(|v| v.trim().parse::<i64>().ok())
                        {
                            report(us.max(0) as f64 / 1e6);
                        }
                    }
                }
                // A poster frame or ffprobe's JSON: small, and capped.
                None => {
                    (&mut stdout)
                        .take(64 << 20)
                        .read_to_end(&mut out)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            child
                .wait()
                .await
                .map(|status| (status, out))
                .map_err(|e| e.to_string())
        };
        // A sender dropped without asking for a stop never will.
        let stopped = async {
            if stop.wait_for(|s| *s).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let outcome = tokio::select! {
            r = tokio::time::timeout(limit, work) => r,
            () = stopped => return Err("stopped".into()),
        };
        let errors = errors.await.unwrap_or_default();
        match outcome {
            Err(_) => Err(format!("timed out after {}s", limit.as_secs())),
            Ok(Err(e)) => Err(e),
            Ok(Ok((status, _))) if !status.success() => Err(if errors.is_empty() {
                status.to_string()
            } else {
                errors
            }),
            Ok(Ok((_, out))) => Ok(out),
        }
    }

    /// What `path`, a `container`, holds.
    pub async fn probe(&self, container: Container, path: &Path) -> Result<Probe, String> {
        let mut args: Vec<OsString> = ["-hide_banner", "-v", "error"]
            .iter()
            .map(Into::into)
            .collect();
        args.extend(Self::input(container).iter().map(Into::into));
        args.extend(
            ["-show_format", "-show_streams", "-of", "json"]
                .iter()
                .map(Into::into),
        );
        args.push(file(path));
        let out = self
            .run(
                &self.ffprobe,
                args,
                Duration::from_secs(60),
                watch::channel(false).1,
                None,
            )
            .await?;
        Probe::parse(&out)
    }

    fn ffmpeg_args(&self) -> Vec<OsString> {
        ["-nostdin", "-hide_banner", "-v", "error", "-y"]
            .iter()
            .map(Into::into)
            .collect()
    }

    /// The original as MP4, without metadata: streams copied, or failing
    /// that (audio MP4 cannot hold, such as Vorbis) with the audio as AAC.
    /// False when MP4 cannot hold the video itself.
    async fn remux(
        &self,
        container: Container,
        probe: &Probe,
        from: &Path,
        to: &Path,
        stop: &watch::Receiver<bool>,
    ) -> Result<bool, String> {
        let limit = Duration::from_secs(600);
        for audio_copy in [true, false] {
            if !audio_copy && probe.audio.is_none() {
                break;
            }
            let mut args = self.ffmpeg_args();
            args.extend(Self::input(container).iter().map(Into::into));
            args.extend(["-i".into(), file(from)]);
            args.extend(probe.maps());
            args.extend(["-c:v", "copy"].iter().map(Into::into));
            if probe.audio.is_some() {
                let audio: &[&str] = if audio_copy {
                    &["-c:a", "copy"]
                } else {
                    &["-c:a", "aac", "-b:a", "192k"]
                };
                args.extend(audio.iter().map(Into::into));
            }
            // Safari plays HEVC in MP4 only under this tag.
            if probe.video.codec == "hevc" {
                args.extend(["-tag:v", "hvc1"].iter().map(Into::into));
            }
            args.extend(clean_output().iter().map(Into::into));
            args.push(file(to));
            match self
                .run(&self.ffmpeg, args, limit, stop.clone(), None)
                .await
            {
                Ok(_) => return Ok(true),
                Err(e) if e == "stopped" => return Err(e),
                Err(e) => tracing::info!(error = %e, audio_copy, "remuxing into MP4 failed"),
            }
        }
        Ok(false)
    }

    /// A copy every browser plays: H.264 at most [`PLAY_MAX`] long, 8-bit
    /// 4:2:0 SDR, at most 60 frames a second, with stereo AAC.
    async fn transcode(
        &self,
        container: Container,
        probe: &Probe,
        from: &Path,
        to: &Path,
        stop: &watch::Receiver<bool>,
        progress: &mut (dyn FnMut(f64) + Send),
    ) -> Result<(), String> {
        let threads = self.threads.to_string();
        let mut args = self.ffmpeg_args();
        args.extend(["-threads".into(), threads.clone().into()]);
        args.extend(Self::input(container).iter().map(Into::into));
        args.extend(["-i".into(), file(from)]);
        args.extend(probe.maps());
        let mut chain = vec![format!(
            "scale=w='min({PLAY_MAX},iw)':h='min({PLAY_MAX},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2"
        )];
        let tonemap = probe.hdr() && self.zscale;
        if tonemap {
            chain.push(TONEMAP.into());
        }
        chain.push("format=yuv420p".into());
        args.extend(["-vf".into(), chain.join(",").into()]);
        args.extend(
            [
                "-c:v",
                "libx264",
                "-preset",
                "medium",
                "-crf",
                CRF,
                "-profile:v",
                "high",
                "-fpsmax",
                "60",
            ]
            .iter()
            .map(Into::into),
        );
        if tonemap {
            args.extend(
                [
                    "-colorspace",
                    "bt709",
                    "-color_primaries",
                    "bt709",
                    "-color_trc",
                    "bt709",
                ]
                .iter()
                .map(Into::into),
            );
        }
        if let Some(audio) = &probe.audio {
            let copy = audio.codec == "aac" && audio.channels <= 2;
            let a: &[&str] = if copy {
                &["-c:a", "copy"]
            } else {
                &["-c:a", "aac", "-b:a", "160k", "-ac", "2"]
            };
            args.extend(a.iter().map(Into::into));
        }
        args.extend(["-threads".into(), threads.into()]);
        args.extend(["-progress", "pipe:1", "-nostats"].iter().map(Into::into));
        args.extend(clean_output().iter().map(Into::into));
        args.push(file(to));
        // Generous: a slow machine transcoding 4K HEVC runs well below real
        // time.
        let limit = Duration::from_secs(600) + Duration::from_secs_f64(probe.duration * 20.0);
        self.run(&self.ffmpeg, args, limit, stop.clone(), Some(progress))
            .await
            .map(drop)
    }

    /// A frame of `path`, an MP4 this made, as PNG: upright, since ffmpeg
    /// applies a video's rotation as it decodes.
    async fn frame(
        &self,
        path: &Path,
        at: f64,
        stop: &watch::Receiver<bool>,
    ) -> Result<Vec<u8>, String> {
        let mut args = self.ffmpeg_args();
        args.extend(["-ss".into(), format!("{at:.3}").into()]);
        args.extend(Self::input(Container::Mp4).iter().map(Into::into));
        args.extend(["-i".into(), file(path)]);
        args.extend(
            [
                "-frames:v",
                "1",
                "-an",
                "-sn",
                "-dn",
                "-f",
                "image2pipe",
                "-c:v",
                "png",
                "pipe:1",
            ]
            .iter()
            .map(Into::into),
        );
        let png = self
            .run(
                &self.ffmpeg,
                args,
                Duration::from_secs(120),
                stop.clone(),
                None,
            )
            .await?;
        if png.is_empty() {
            return Err("no frame came out".into());
        }
        Ok(png)
    }
}

/// What `program` writes to standard output given `args`, for the checks at
/// startup.
async fn output(program: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .context("timed out")?
        .with_context(|| format!("running {}", program.display()))?;
    anyhow::ensure!(out.status.success(), "{} failed", program.display());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `path` as ffmpeg's file protocol names it, so nothing in it reads as
/// another protocol.
fn file(path: &Path) -> OsString {
    let mut s = OsString::from("file:");
    s.push(path);
    s
}

/// An MP4 output without the source's metadata or chapters, its index at the
/// front so playback starts before the whole file arrives, and the same bytes
/// for the same input.
fn clean_output() -> [&'static str; 12] {
    [
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
        "-sn",
        "-dn",
        "-fflags",
        "+bitexact",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
    ]
}

/// A stream, as ffprobe reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub index: u32,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Degrees the picture is turned for display, from the track's matrix.
    pub rotation: i32,
    pub pix_fmt: String,
    pub transfer: String,
    pub fps: f64,
    pub bitrate: u64,
    pub channels: u32,
}

/// What a video file holds: the streams dither keeps.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub duration: f64,
    pub bitrate: u64,
    pub video: Stream,
    pub audio: Option<Stream>,
}

#[derive(serde::Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    #[serde(default)]
    format: RawFormat,
}

#[derive(serde::Deserialize, Default)]
struct RawFormat {
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawStream {
    index: u32,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    color_transfer: Option<String>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    bit_rate: Option<String>,
    channels: Option<u32>,
    duration: Option<String>,
    #[serde(default)]
    disposition: HashMap<String, i64>,
    #[serde(default)]
    side_data_list: Vec<HashMap<String, serde_json::Value>>,
}

/// `30000/1001` as a number; zero when it is not one.
fn rate(r: Option<&str>) -> f64 {
    let Some((n, d)) = r.and_then(|r| r.split_once('/')) else {
        return 0.0;
    };
    match (n.parse::<f64>(), d.parse::<f64>()) {
        (Ok(n), Ok(d)) if d > 0.0 => n / d,
        _ => 0.0,
    }
}

impl Probe {
    fn parse(json: &[u8]) -> Result<Self, String> {
        let raw: RawProbe =
            serde_json::from_slice(json).map_err(|e| format!("reading ffprobe's report: {e}"))?;
        let number = |s: &Option<String>| s.as_deref().and_then(|v| v.parse::<f64>().ok());
        let stream = |s: &RawStream| Stream {
            index: s.index,
            codec: s.codec_name.clone().unwrap_or_default(),
            width: s.width.unwrap_or_default(),
            height: s.height.unwrap_or_default(),
            rotation: s
                .side_data_list
                .iter()
                .find_map(|d| d.get("rotation").and_then(serde_json::Value::as_f64))
                .map_or(0, |r| r.round() as i32),
            pix_fmt: s.pix_fmt.clone().unwrap_or_default(),
            transfer: s.color_transfer.clone().unwrap_or_default(),
            fps: match rate(s.avg_frame_rate.as_deref()) {
                0.0 => rate(s.r_frame_rate.as_deref()),
                fps => fps,
            },
            bitrate: number(&s.bit_rate).unwrap_or_default() as u64,
            channels: s.channels.unwrap_or_default(),
        };
        // A cover picture in an MP4 is a video stream of one frame.
        let video = raw
            .streams
            .iter()
            .find(|s| {
                s.codec_type.as_deref() == Some("video")
                    && s.codec_name.is_some()
                    && s.disposition.get("attached_pic").copied().unwrap_or(0) == 0
                    && s.width.unwrap_or(0) > 0
            })
            .ok_or("it has no video in it")?;
        let audio = raw
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("audio") && s.codec_name.is_some());
        let duration = number(&raw.format.duration)
            .or_else(|| number(&video.duration))
            .filter(|d| d.is_finite() && *d > 0.0)
            .ok_or("its length can't be read")?;
        Ok(Self {
            duration,
            bitrate: number(&raw.format.bit_rate).unwrap_or_default() as u64,
            video: stream(video),
            audio: audio.map(stream),
        })
    }

    /// Width and height as shown, once the rotation is applied.
    pub fn shown(&self) -> (u32, u32) {
        let v = &self.video;
        if v.rotation.rem_euclid(180) == 90 {
            (v.height, v.width)
        } else {
            (v.width, v.height)
        }
    }

    /// PQ (HDR10, Dolby Vision's base layer) or HLG, as iPhones record.
    pub fn hdr(&self) -> bool {
        matches!(self.video.transfer.as_str(), "smpte2084" | "arib-std-b67")
    }

    /// Whether browsers play it as uploaded, at a bitrate a phone keeps up
    /// with.
    pub fn playable(&self) -> bool {
        let v = &self.video;
        let (w, h) = self.shown();
        let bitrate = if v.bitrate > 0 {
            v.bitrate
        } else {
            self.bitrate
        };
        v.codec == "h264"
            && matches!(v.pix_fmt.as_str(), "yuv420p" | "yuvj420p")
            && !self.hdr()
            && w.max(h) <= PLAY_MAX
            && v.fps <= 61.0
            && bitrate <= PLAY_BITRATE
            && self
                .audio
                .as_ref()
                .is_none_or(|a| matches!(a.codec.as_str(), "aac" | "mp3") && a.channels <= 2)
    }

    /// The `-map`s for the streams kept.
    fn maps(&self) -> Vec<OsString> {
        let mut maps = vec!["-map".into(), format!("0:{}", self.video.index).into()];
        if let Some(a) = &self.audio {
            maps.extend(["-map".into(), format!("0:{}", a.index).into()]);
        }
        maps
    }
}

/// Why a video was not made: the file's fault, said to its owner, or this
/// side's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Rejected(String),
    Failed(String),
    /// Deleted while it was made.
    Stopped,
}

/// What [`Videos`] needs beyond the database and the store.
pub struct Settings {
    pub tools: Option<Tools>,
    pub max_bytes: u64,
    pub max_seconds: u64,
    pub max_pixels: u64,
    /// Bytes of the processing budget a video holds while it is made.
    pub memory: u64,
}

impl Settings {
    /// No videos: for tests of everything else.
    #[cfg(test)]
    pub fn off() -> Self {
        Self {
            tools: None,
            max_bytes: 0,
            max_seconds: 0,
            max_pixels: 0,
            memory: 0,
        }
    }
}

/// How far along a video is, for the album showing it.
#[derive(Debug, Clone)]
pub struct Tick {
    pub owner: Arc<str>,
    pub id: String,
    /// Thousandths.
    pub done: u16,
}

pub struct Videos {
    settings: Settings,
    db: Db,
    store: Store,
    /// Uploads being made into videos, by the video's id.
    dir: PathBuf,
    budget: Arc<Semaphore>,
    /// The budget's size, in MiB: no video asks for more.
    budget_mib: u32,
    queue: mpsc::UnboundedSender<String>,
    waiting: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    progress: parking_lot::Mutex<HashMap<String, u16>>,
    ticks: broadcast::Sender<Tick>,
    /// The video being made, and how to stop it.
    current: parking_lot::Mutex<Option<(String, watch::Sender<bool>)>>,
}

impl Videos {
    pub fn new(
        db: Db,
        store: Store,
        state_dir: &Path,
        budget: Arc<Semaphore>,
        budget_mib: u32,
        settings: Settings,
    ) -> std::io::Result<Self> {
        let dir = state_dir.join("videos");
        std::fs::create_dir_all(&dir)?;
        let (queue, waiting) = mpsc::unbounded_channel();
        Ok(Self {
            settings,
            db,
            store,
            dir,
            budget,
            budget_mib,
            queue,
            waiting: parking_lot::Mutex::new(Some(waiting)),
            progress: Default::default(),
            ticks: broadcast::channel(256).0,
            current: Default::default(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.settings.tools.is_some()
    }

    pub fn max_bytes(&self) -> u64 {
        self.settings.max_bytes
    }

    /// Thousandths done of video `id`, while it is being made.
    pub fn progress(&self, id: &str) -> Option<u16> {
        self.progress.lock().get(id).copied()
    }

    pub fn ticks(&self) -> broadcast::Receiver<Tick> {
        self.ticks.subscribe()
    }

    /// Where an upload waits to be made into video `id`. Ids are checked by
    /// [`crate::id`], so this names nothing outside the directory.
    fn source(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    fn work(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.work"))
    }

    /// Probe an upload that is a `container`, and say what video it makes,
    /// or why it makes none. Nothing changes.
    pub async fn check(
        &self,
        container: Container,
        path: &Path,
        size: u64,
    ) -> Result<NewVideo, Problem> {
        let tools = self
            .settings
            .tools
            .as_ref()
            .ok_or_else(|| Problem::Rejected("this server doesn't take videos".into()))?;
        if size > self.settings.max_bytes {
            return Err(Problem::Rejected(format!(
                "{} is over the {} limit for a video",
                crate::web::human(size),
                crate::web::human(self.settings.max_bytes)
            )));
        }
        let probe = tools.probe(container, path).await.map_err(|e| {
            tracing::info!(error = %e, "ffprobe could not read an upload");
            Problem::Rejected("the video can't be read: it may be damaged".into())
        })?;
        if probe.duration > self.settings.max_seconds as f64 {
            return Err(Problem::Rejected(format!(
                "it runs {}; the limit is {}",
                clock(probe.duration),
                clock(self.settings.max_seconds as f64)
            )));
        }
        let (width, height) = probe.shown();
        let pixels = u64::from(width) * u64::from(height);
        if pixels == 0 || pixels > self.settings.max_pixels {
            return Err(Problem::Rejected(format!(
                "{width}×{height} is larger than this server takes"
            )));
        }
        if !DECODERS.split(',').any(|d| d == probe.video.codec) {
            return Err(Problem::Rejected(format!(
                "its video is {}, which dither can't read",
                probe.video.codec
            )));
        }
        Ok(NewVideo {
            id: crate::id::new(),
            width,
            height,
            duration_ms: (probe.duration * 1000.0).round() as u64,
            audio: probe.audio.is_some(),
            bytes: size,
        })
    }

    /// Move an upload's file to where video `id` is made from.
    pub async fn take(&self, from: &Path, id: &str) -> std::io::Result<()> {
        tokio::fs::rename(from, self.source(id)).await
    }

    /// Put it back, when the video's row could not be added.
    pub async fn give_back(&self, id: &str, to: &Path) {
        if let Err(e) = tokio::fs::rename(self.source(id), to).await {
            tracing::warn!(video = id, error = %e, "returning an upload's file failed");
        }
    }

    /// Make video `id` when the worker reaches it.
    pub fn enqueue(&self, id: String) {
        let _ = self.queue.send(id);
    }

    /// Stop making any of `ids`: they were deleted.
    pub fn cancel(&self, ids: &[String]) {
        if let Some((id, stop)) = self.current.lock().as_ref()
            && ids.contains(id)
        {
            let _ = stop.send(true);
        }
    }

    /// Start the worker, first with the videos a restart interrupted.
    pub fn start(self: &Arc<Self>) {
        let Some(mut waiting) = self.waiting.lock().take() else {
            return;
        };
        let this = self.clone();
        tokio::spawn(async move {
            match this.db.processing().await {
                Ok(videos) => {
                    if !videos.is_empty() {
                        tracing::info!(count = videos.len(), "resuming videos");
                    }
                    for (image, _) in videos {
                        this.enqueue(image.id);
                    }
                }
                Err(e) => tracing::error!(error = %e, "listing videos to resume failed"),
            }
            while let Some(id) = waiting.recv().await {
                this.make(&id).await;
            }
        });
    }

    fn tick(&self, owner: &Arc<str>, id: &str, done: u16) {
        let mut progress = self.progress.lock();
        if progress.get(id) == Some(&done) {
            return;
        }
        progress.insert(id.to_string(), done);
        let _ = self.ticks.send(Tick {
            owner: owner.clone(),
            id: id.to_string(),
            done,
        });
    }

    /// Make video `id`'s copies and mark it ready, or failed with why.
    async fn make(&self, id: &str) {
        let Ok(Some((image, album))) = self.db.image(id).await else {
            return;
        };
        if image.status != Status::Processing {
            return;
        }
        let owner: Arc<str> = album.owner.as_str().into();
        let started = Instant::now();
        let (stop, stopped) = watch::channel(false);
        *self.current.lock() = Some((id.to_string(), stop));
        self.tick(&owner, id, 0);
        let outcome = self.copies(&image, &owner, stopped).await;
        *self.current.lock() = None;
        let _ = tokio::fs::remove_dir_all(self.work(id)).await;
        let ok = match outcome {
            Ok(made) => match self.db.finish_video(&owner, id, made).await {
                Ok(true) => {
                    let _ = tokio::fs::remove_file(self.source(id)).await;
                    tracing::info!(video = id, secs = started.elapsed().as_secs(), "video made");
                    true
                }
                // Deleted while it was made.
                Ok(false) => {
                    let _ = tokio::fs::remove_file(self.source(id)).await;
                    store::discard(&self.store, &self.db, vec![id.to_string()]).await;
                    false
                }
                Err(e) => {
                    // Still processing, with its upload kept: made again on
                    // the next start.
                    tracing::error!(video = id, error = %e, "recording a video failed");
                    false
                }
            },
            Err(Problem::Stopped) => {
                let _ = tokio::fs::remove_file(self.source(id)).await;
                store::discard(&self.store, &self.db, vec![id.to_string()]).await;
                false
            }
            Err(problem) => {
                let why = match problem {
                    Problem::Rejected(why) => why,
                    Problem::Stopped => unreachable!("matched above"),
                    Problem::Failed(why) => {
                        tracing::error!(video = id, error = %why, "making a video failed");
                        "it couldn't be processed; delete it and upload it again".into()
                    }
                };
                let _ = tokio::fs::remove_file(self.source(id)).await;
                store::discard(&self.store, &self.db, vec![id.to_string()]).await;
                if let Err(e) = self.db.fail_video(&owner, id, &why).await {
                    tracing::warn!(video = id, error = %e, "recording a failed video failed");
                }
                false
            }
        };
        crate::metrics::video(ok, started.elapsed());
        self.progress.lock().remove(id);
    }

    async fn copies(
        &self,
        image: &crate::db::Image,
        owner: &Arc<str>,
        stop: watch::Receiver<bool>,
    ) -> Result<MadeVideo, Problem> {
        let failed = |what: &str| {
            let what = what.to_string();
            move |e: String| {
                if e == "stopped" {
                    Problem::Stopped
                } else {
                    Problem::Failed(format!("{what}: {e}"))
                }
            }
        };
        let tools = self
            .settings
            .tools
            .as_ref()
            .ok_or_else(|| Problem::Failed("videos are off".into()))?;
        let id = image.id.as_str();
        let source = self.source(id);
        if !tokio::fs::try_exists(&source).await.unwrap_or(false) {
            return Err(Problem::Failed(
                "its upload was lost before it was processed".into(),
            ));
        }
        let work = self.work(id);
        let _ = tokio::fs::remove_dir_all(&work).await;
        tokio::fs::create_dir_all(&work)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        let mut head = Vec::new();
        tokio::fs::File::open(&source)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?
            .take(16)
            .read_to_end(&mut head)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        let container = crate::sanitize::sniff_video(&head)
            .ok_or_else(|| Problem::Failed("its upload is no longer a video".into()))?;

        let mib = u32::try_from(self.settings.memory.div_ceil(1 << 20))
            .unwrap_or(u32::MAX)
            .clamp(1, self.budget_mib.max(1));
        let _held = self
            .budget
            .acquire_many(mib)
            .await
            .map_err(|_| Problem::Failed("shutting down".into()))?;

        let probe = tools
            .probe(container, &source)
            .await
            .map_err(|_| Problem::Rejected("the video can't be read: it may be damaged".into()))?;
        let playable = probe.playable();
        let original = work.join("original.mp4");
        let remuxed = tools
            .remux(container, &probe, &source, &original, &stop)
            .await
            .map_err(failed("remuxing"))?;
        let span = if playable && remuxed { 900.0 } else { 100.0 };
        self.tick(owner, id, span as u16);

        let play = work.join("play.mp4");
        let transcoded = !playable || !remuxed;
        if transcoded {
            let duration = probe.duration;
            let mut report = |secs: f64| {
                let share = (secs / duration).clamp(0.0, 1.0);
                self.tick(owner, id, (100.0 + share * 850.0) as u16);
            };
            tools
                .transcode(container, &probe, &source, &play, &stop, &mut report)
                .await
                .map_err(failed("transcoding"))?;
        }
        // MP4 could not hold the original's video: the playback copy is the
        // only copy.
        let (original, display) = match (remuxed, transcoded) {
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
        let max = self.settings.max_pixels;
        let still = tokio::task::spawn_blocking(move || crate::media::process(&png, max))
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?
            .map_err(|e| Problem::Failed(format!("the frame: {e}")))?;
        self.tick(owner, id, 960);

        let (bytes, sha256) = digest(&original)
            .await
            .map_err(|e| Problem::Failed(e.to_string()))?;
        let stored = async {
            self.db.add_stray(id).await?;
            self.store
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
            Ok::<_, anyhow::Error>(())
        };
        stored
            .await
            .map_err(|e| Problem::Failed(format!("storing: {e:#}")))?;
        self.tick(owner, id, 1000);
        let (width, height) = probe.shown();
        Ok(MadeVideo {
            width,
            height,
            duration_ms: (probe.duration * 1000.0).round() as u64,
            audio: probe.audio.is_some(),
            bytes,
            display: display.is_some(),
            sha256,
        })
    }
}

impl Videos {
    /// Remove what a video left in the directory once it is no longer being
    /// made: a crash between finishing and tidying, or a row deleted while
    /// its file waited. A day's grace, as uploads get.
    pub async fn sweep(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".work").unwrap_or(&name);
            let stale = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 3600));
            let making = self.current.lock().as_ref().is_some_and(|(c, _)| c == id);
            if !stale || making {
                continue;
            }
            let keep = match self.db.image(id).await {
                Ok(Some((image, _))) => image.status == Status::Processing,
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

/// The size and SHA-256 of the file at `path`, read in pieces.
async fn digest(path: &Path) -> std::io::Result<(u64, Vec<u8>)> {
    use sha2::Digest as _;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let mut file = std::fs::File::open(path)?;
        let mut hash = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut n = 0u64;
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            hash.update(&buf[..read]);
            n += read as u64;
        }
        Ok((n, hash.finalize().to_vec()))
    })
    .await?
}

/// Seconds as a clock reads them: `0:42`, `1:02:03`.
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The tools, or `None` where ffmpeg is not installed, and the test
    /// returns early. CI sets `DITHER_REQUIRE_FFMPEG`, where a missing
    /// ffmpeg fails instead.
    pub async fn tools() -> Option<Tools> {
        let video = crate::config::Config::test(PathBuf::new()).video;
        let found = Tools::find(&video).await.expect("auto never fails");
        let required = std::env::var_os("DITHER_REQUIRE_FFMPEG").is_some();
        match &found {
            None => {
                assert!(!required, "ffmpeg and ffprobe are required here");
                eprintln!("skipped: no ffmpeg");
            }
            // The build the image ships must tone-map, or HDR plays grey.
            Some(tools) => assert!(
                !required || tools.zscale,
                "the shipped ffmpeg has no zscale"
            ),
        }
        found
    }

    /// A clip ffmpeg makes from its test sources: `args` between the inputs
    /// and the output.
    pub fn clip(dir: &Path, name: &str, seconds: u32, size: &str, args: &[&str]) -> PathBuf {
        let out = dir.join(name);
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size}:rate=30:duration={seconds}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}"))
            .args(args)
            .arg(&out)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "making {name}");
        out
    }

    /// What ffprobe reports of `path`, whole: every tag, wherever it is.
    fn report(path: &Path) -> String {
        let out = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_format",
                "-show_streams",
                "-of",
                "json",
            ])
            .arg(path)
            .output()
            .expect("ffprobe runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// An HEVC MOV as an iPhone records one: turned for portrait, with sound,
    /// a timecode track, and where it was taken in the container's metadata.
    pub fn phone_clip(dir: &Path) -> PathBuf {
        let raw = clip(
            dir,
            "raw.mov",
            2,
            "640x360",
            &[
                "-c:v",
                "libx265",
                "-x265-params",
                "log-level=error",
                "-tag:v",
                "hvc1",
                "-c:a",
                "aac",
            ],
        );
        let out = dir.join("phone.mov");
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-display_rotation:v:0", "90", "-i"])
            .arg(&raw)
            .args([
                "-c",
                "copy",
                "-metadata",
                "location=+51.5072-000.1276/",
                "-metadata",
                "com.apple.quicktime.location.ISO6709=+51.5072-000.1276+011.000/",
                "-metadata",
                "com.apple.quicktime.make=Apple",
                "-movflags",
                "use_metadata_tags",
                "-timecode",
                "01:00:00:00",
            ])
            .arg(&out)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success());
        out
    }

    fn never() -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    #[tokio::test]
    async fn a_probe_reads_the_shown_size_the_length_and_the_sound() {
        let Some(tools) = tools().await else { return };
        let dir = tempfile::tempdir().unwrap();
        let phone = phone_clip(dir.path());
        let probe = tools.probe(Container::Mp4, &phone).await.unwrap();
        assert_eq!(probe.video.codec, "hevc");
        assert_eq!(probe.video.rotation.rem_euclid(180), 90);
        assert_eq!(probe.shown(), (360, 640));
        assert!((probe.duration - 2.0).abs() < 0.2, "{}", probe.duration);
        assert_eq!(probe.audio.as_ref().map(|a| a.codec.as_str()), Some("aac"));
        assert!(!probe.playable(), "HEVC plays in too few browsers");

        let plain = clip(
            dir.path(),
            "plain.mp4",
            1,
            "640x360",
            &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"],
        );
        let probe = tools.probe(Container::Mp4, &plain).await.unwrap();
        assert!(probe.playable(), "{probe:?}");

        // Told it is Matroska, ffmpeg does not go looking for what it is.
        assert!(tools.probe(Container::Matroska, &plain).await.is_err());
    }

    #[tokio::test]
    async fn the_original_keeps_its_streams_and_rotation_and_loses_where_it_was_taken() {
        let Some(tools) = tools().await else { return };
        let dir = tempfile::tempdir().unwrap();
        let phone = phone_clip(dir.path());
        assert!(
            report(&phone).contains("51.5072"),
            "the test clip carries a location"
        );
        let probe = tools.probe(Container::Mp4, &phone).await.unwrap();
        let out = dir.path().join("original.mp4");
        assert!(
            tools
                .remux(Container::Mp4, &probe, &phone, &out, &never())
                .await
                .unwrap()
        );
        let after = report(&out);
        for gone in ["51.5072", "ISO6709", "Apple", "timecode", "tmcd"] {
            assert!(!after.contains(gone), "{gone} survived: {after}");
        }
        let kept = tools.probe(Container::Mp4, &out).await.unwrap();
        assert_eq!(kept.shown(), (360, 640));
        assert_eq!(kept.video.codec, "hevc");
        assert!(kept.audio.is_some());
        // Safari plays HEVC in MP4 only tagged hvc1.
        assert!(after.contains("\"codec_tag_string\": \"hvc1\""), "{after}");
    }

    #[tokio::test]
    async fn a_playback_copy_is_upright_h264_within_bounds() {
        let Some(tools) = tools().await else { return };
        let dir = tempfile::tempdir().unwrap();
        let phone = phone_clip(dir.path());
        let probe = tools.probe(Container::Mp4, &phone).await.unwrap();
        let out = dir.path().join("play.mp4");
        let mut seen = Vec::new();
        let mut report = |secs: f64| seen.push(secs);
        tools
            .transcode(Container::Mp4, &probe, &phone, &out, &never(), &mut report)
            .await
            .unwrap();
        assert!(seen.iter().any(|s| *s > 1.0), "{seen:?}");
        let play = tools.probe(Container::Mp4, &out).await.unwrap();
        assert_eq!(play.video.codec, "h264");
        assert_eq!(play.video.pix_fmt, "yuv420p");
        // Turned as it was made, so it needs no turning to show.
        assert_eq!((play.video.width, play.video.height), (360, 640));
        assert_eq!(play.video.rotation, 0);
        assert!(play.playable(), "{play:?}");
        let png = tools.frame(&out, 0.5, &never()).await.unwrap();
        let frame = image::load_from_memory(&png).unwrap();
        assert_eq!((frame.width(), frame.height()), (360, 640));
    }

    #[tokio::test]
    async fn a_large_or_webm_video_is_scaled_and_transcoded() {
        let Some(tools) = tools().await else { return };
        let dir = tempfile::tempdir().unwrap();
        let webm = clip(
            dir.path(),
            "big.webm",
            1,
            "2560x1440",
            &[
                "-c:v",
                "libvpx-vp9",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-c:a",
                "libopus",
            ],
        );
        let probe = tools.probe(Container::Matroska, &webm).await.unwrap();
        assert!(!probe.playable());
        let original = dir.path().join("original.mp4");
        assert!(
            tools
                .remux(Container::Matroska, &probe, &webm, &original, &never())
                .await
                .unwrap(),
            "VP9 and Opus fit in MP4"
        );
        let out = dir.path().join("play.mp4");
        tools
            .transcode(
                Container::Matroska,
                &probe,
                &webm,
                &out,
                &never(),
                &mut |_| {},
            )
            .await
            .unwrap();
        let play = tools.probe(Container::Mp4, &out).await.unwrap();
        assert_eq!((play.video.width, play.video.height), (1920, 1080));
        assert_eq!(play.audio.as_ref().unwrap().codec, "aac");
    }

    #[tokio::test]
    async fn hdr_is_tone_mapped_to_sdr_where_the_build_can() {
        let Some(tools) = tools().await else { return };
        if !tools.zscale {
            eprintln!("skipped: this ffmpeg has no zscale");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // HLG in BT.2020, 10-bit HEVC, as an iPhone records HDR.
        let hlg = clip(
            dir.path(),
            "hlg.mov",
            1,
            "640x360",
            &[
                "-c:v",
                "libx265",
                "-x265-params",
                "log-level=error",
                "-pix_fmt",
                "yuv420p10le",
                "-color_primaries",
                "bt2020",
                "-color_trc",
                "arib-std-b67",
                "-colorspace",
                "bt2020nc",
                "-c:a",
                "aac",
            ],
        );
        let probe = tools.probe(Container::Mp4, &hlg).await.unwrap();
        assert!(probe.hdr(), "{probe:?}");
        let out = dir.path().join("play.mp4");
        tools
            .transcode(Container::Mp4, &probe, &hlg, &out, &never(), &mut |_| {})
            .await
            .unwrap();
        let play = tools.probe(Container::Mp4, &out).await.unwrap();
        assert_eq!(play.video.pix_fmt, "yuv420p");
        assert_eq!(play.video.transfer, "bt709");
        assert!(play.playable(), "{play:?}");
    }

    #[tokio::test]
    async fn a_stop_ends_ffmpeg() {
        let Some(tools) = tools().await else { return };
        let dir = tempfile::tempdir().unwrap();
        let long = clip(
            dir.path(),
            "long.mp4",
            30,
            "1920x1080",
            &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"],
        );
        let probe = tools.probe(Container::Mp4, &long).await.unwrap();
        let (stop, stopped) = watch::channel(false);
        let out = dir.path().join("play.mp4");
        let started = Instant::now();
        let mut report = |_| {
            let _ = stop.send(true);
        };
        let result = tools
            .transcode(Container::Mp4, &probe, &long, &out, &stopped, &mut report)
            .await;
        assert_eq!(result, Err("stopped".to_string()));
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn ffprobe_reports_parse_into_what_is_kept() {
        let json = br#"{
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "mjpeg", "width": 300, "height": 300,
                 "disposition": {"attached_pic": 1}},
                {"index": 1, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080,
                 "pix_fmt": "yuv420p", "avg_frame_rate": "30000/1001", "bit_rate": "8000000",
                 "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]},
                {"index": 2, "codec_type": "data", "codec_name": "bin_data"},
                {"index": 3, "codec_type": "audio", "codec_name": "aac", "channels": 2}
            ],
            "format": {"duration": "12.5", "bit_rate": "8200000"}
        }"#;
        let p = Probe::parse(json).unwrap();
        assert_eq!(p.video.index, 1, "a cover picture is not the video");
        assert_eq!(p.shown(), (1080, 1920));
        assert!((p.video.fps - 29.97).abs() < 0.01);
        assert_eq!(p.audio.as_ref().unwrap().index, 3);
        assert!(p.playable());
        let hdr = Probe {
            video: Stream {
                transfer: "arib-std-b67".into(),
                ..p.video.clone()
            },
            ..p.clone()
        };
        assert!(hdr.hdr() && !hdr.playable());
        let fast = Probe {
            video: Stream {
                bitrate: 40_000_000,
                ..p.video.clone()
            },
            ..p.clone()
        };
        assert!(
            !fast.playable(),
            "a camera's bitrate is too much for a phone"
        );
        assert_eq!(
            Probe::parse(br#"{"streams": [{"index": 0, "codec_type": "audio", "codec_name": "aac"}], "format": {"duration": "3"}}"#),
            Err("it has no video in it".into())
        );
    }

    #[test]
    fn lengths_read_as_a_clock() {
        assert_eq!(clock(0.4), "0:00");
        assert_eq!(clock(42.0), "0:42");
        assert_eq!(clock(62.6), "1:03");
        assert_eq!(clock(3723.0), "1:02:03");
    }
}
