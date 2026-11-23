//! Videos: ffmpeg and ffprobe, and what a video is made into. The worker
//! (`worker.rs`) runs them.
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
//! ffmpeg parses untrusted files with a great deal of code, so it runs as a
//! process of its own with none of the environment, told which container it
//! reads rather than left to guess, limited to the file protocol and to the
//! decoders a phone or camera needs, on a fixed number of threads at a lower
//! priority, and with a time limit.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};
use tokio::sync::watch;

use crate::sanitize::Container;

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
/// The most pixels a video's frame may have: 8K. ffmpeg's memory is outside
/// the processing budget, and a frame larger than this would need more than
/// a worker has to decode with its references.
pub const MAX_PIXELS: u64 = 8192 * 4320;

/// Whether ffmpeg may decode `codec` here.
pub fn decodable(codec: &str) -> bool {
    DECODERS.split(',').any(|d| d == codec)
}

/// Whether MP4 holds a video of `codec`, so the original can be remuxed into
/// it as recorded. One that does not (VP8, Theora, ProRes) keeps only its
/// playback copy.
pub fn fits_mp4(codec: &str) -> bool {
    matches!(codec, "h264" | "hevc" | "av1" | "vp9" | "mpeg4")
}

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
        let mut cmd = command(program);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Serving comes first: the priority is lowered in the child before it
        // runs ffmpeg, so every thread ffmpeg starts has it too.
        // SAFETY: the closure runs between fork and exec, where only
        // async-signal-safe calls are allowed; setpriority is a system call
        // and allocates nothing.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setpriority_process(None, 10).map_err(std::io::Error::from)
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("starting {}: {e}", program.display()))?;
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
            () = stopped => Ok(Err("stopped".to_string())),
        };
        // Out of time or stopped, it is killed before its error output is
        // read, which ends only when it does.
        if !matches!(outcome, Ok(Ok(_))) {
            let _ = child.start_kill();
        }
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
        let mut probe = Probe::parse(&out)?;
        // Matroska from a browser's MediaRecorder states no length: the last
        // packet's time is it.
        if probe.duration <= 0.0 {
            probe.duration = self.last_packet(container, path).await?;
        }
        if probe.duration <= 0.0 {
            return Err("its length can't be read".into());
        }
        Ok(probe)
    }

    /// When the video's last packet ends, in seconds, read from every
    /// packet's time: slower than the container's word, but a file's whole
    /// is read without decoding it.
    async fn last_packet(&self, container: Container, path: &Path) -> Result<f64, String> {
        let mut args: Vec<OsString> = ["-hide_banner", "-v", "error"]
            .iter()
            .map(Into::into)
            .collect();
        args.extend(Self::input(container).iter().map(Into::into));
        args.extend(
            [
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time,duration_time",
                "-of",
                "csv=p=0",
            ]
            .iter()
            .map(Into::into),
        );
        args.push(file(path));
        let out = self
            .run(
                &self.ffprobe,
                args,
                Duration::from_secs(300),
                watch::channel(false).1,
                None,
            )
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let mut parts = l.split(',').map(|v| v.trim().parse::<f64>().ok());
                let pts = parts.next().flatten()?;
                Some(pts + parts.next().flatten().unwrap_or(0.0))
            })
            .fold(0.0, f64::max))
    }

    /// What every ffmpeg run starts with: no questions, errors only, and its
    /// filters held to its thread count, which is otherwise the machine's
    /// (a pod's CPU limit is not what ffmpeg counts).
    fn ffmpeg_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = ["-nostdin", "-hide_banner", "-v", "error", "-y"]
            .iter()
            .map(Into::into)
            .collect();
        args.extend(["-filter_threads".into(), self.threads.to_string().into()]);
        args
    }

    /// An input's decoding held to the thread count.
    fn threads(&self) -> [OsString; 2] {
        ["-threads".into(), self.threads.to_string().into()]
    }

    /// The original as MP4, without metadata: streams copied, or failing
    /// that (audio MP4 cannot hold, such as Vorbis) with the audio as AAC.
    /// The video's codec is one MP4 holds ([`fits_mp4`]); `size` is the
    /// file's, which the time limit allows for at a slow disk's pace.
    pub async fn remux(
        &self,
        container: Container,
        probe: &Probe,
        from: &Path,
        to: &Path,
        size: u64,
        stop: &watch::Receiver<bool>,
    ) -> Result<(), String> {
        let limit = Duration::from_secs(600) + Duration::from_secs(size / (2 << 20));
        let mut failure = String::new();
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
            args.extend(probe.length());
            args.extend(clean_output().iter().map(Into::into));
            args.push(file(to));
            match self
                .run(&self.ffmpeg, args, limit, stop.clone(), None)
                .await
            {
                Ok(_) => return Ok(()),
                // Out of time, another try would only run out again.
                Err(e) if e == "stopped" || e.starts_with("timed out") => return Err(e),
                Err(e) => {
                    tracing::info!(error = %e, audio_copy, "remuxing into MP4 failed");
                    failure = e;
                }
            }
        }
        Err(failure)
    }

    /// A copy every browser plays: H.264 at most [`PLAY_MAX`] long, 8-bit
    /// 4:2:0 SDR, at most 60 frames a second, with stereo AAC.
    pub async fn transcode(
        &self,
        container: Container,
        probe: &Probe,
        from: &Path,
        to: &Path,
        stop: &watch::Receiver<bool>,
        progress: &mut (dyn FnMut(f64) + Send),
    ) -> Result<(), String> {
        let mut args = self.ffmpeg_args();
        args.extend(self.threads());
        args.extend(Self::input(container).iter().map(Into::into));
        args.extend(["-i".into(), file(from)]);
        args.extend(probe.maps());
        let hdr = probe.hdr();
        let tonemap = hdr && self.zscale;
        // Without zimg, HDR is at least converted to BT.709's matrix and
        // labelled as SDR, rather than an 8-bit copy claiming to be HDR.
        let matrix = if hdr && !tonemap {
            ":out_color_matrix=bt709:out_range=tv"
        } else {
            ""
        };
        let mut chain = vec![format!(
            "scale=w='min({PLAY_MAX},iw)':h='min({PLAY_MAX},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2{matrix}"
        )];
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
        if hdr {
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
        args.extend(self.threads());
        args.extend(["-progress", "pipe:1", "-nostats"].iter().map(Into::into));
        args.extend(probe.length());
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
    pub async fn frame(
        &self,
        path: &Path,
        at: f64,
        stop: &watch::Receiver<bool>,
    ) -> Result<Vec<u8>, String> {
        let mut args = self.ffmpeg_args();
        args.extend(self.threads());
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

/// `program` to run with none of this process's environment but where to
/// find programs: the environment holds the sign-in secret and the bucket's
/// keys, and ffmpeg is what parses untrusted files.
fn command(program: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd
}

/// What `program` writes to standard output given `args`, for the checks at
/// startup.
async fn output(program: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = command(program);
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
        // The first sound ffmpeg may decode here: a track in a codec it may
        // not (spatial audio beside a stereo one) is passed over.
        let audio = raw.streams.iter().find(|s| {
            s.codec_type.as_deref() == Some("audio")
                && s.codec_name.as_deref().is_some_and(decodable)
        });
        let duration = number(&raw.format.duration)
            .or_else(|| number(&video.duration))
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or(0.0);
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

    /// Output held to the length the file declared, which the length limit
    /// and the time limit were judged by: a file whose index says ten
    /// seconds and whose samples run for hours is cut at ten.
    fn length(&self) -> [OsString; 2] {
        ["-t".into(), format!("{:.3}", self.duration + 1.0).into()]
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
    use std::time::Instant;

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
        tools
            .remux(Container::Mp4, &probe, &phone, &out, 0, &never())
            .await
            .unwrap();
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
        assert!(fits_mp4(&probe.video.codec));
        tools
            .remux(Container::Matroska, &probe, &webm, &original, 0, &never())
            .await
            .expect("VP9 and Opus fit in MP4");
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
                // Tagged in the stream itself: some builds' x265 ignores
                // ffmpeg's own colour options.
                "-x265-params",
                "log-level=error:colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc",
                "-pix_fmt",
                "yuv420p10le",
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
    async fn a_recording_that_states_no_length_is_measured_from_its_packets() {
        let Some(tools) = tools().await else { return };
        let dir = tempfile::tempdir().unwrap();
        // As a browser's MediaRecorder writes it: no duration in the header.
        let live = clip(
            dir.path(),
            "recorded.webm",
            3,
            "320x240",
            &[
                "-c:v",
                "libvpx",
                "-deadline",
                "realtime",
                "-c:a",
                "libopus",
                "-live",
                "1",
                "-f",
                "webm",
            ],
        );
        let probe = tools.probe(Container::Matroska, &live).await.unwrap();
        assert!((probe.duration - 3.0).abs() < 0.5, "{}", probe.duration);
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
        // No length stated: left for the packets to tell.
        let unstated = Probe::parse(br#"{"streams": [{"index": 0, "codec_type": "video", "codec_name": "vp9", "width": 8, "height": 8}], "format": {}}"#).unwrap();
        assert_eq!(unstated.duration, 0.0);
    }

    #[tokio::test]
    async fn ffmpeg_sees_none_of_the_environment() {
        // Cargo gives a test this; it stands in for the secrets a pod has.
        assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
        let out = command(Path::new("env")).output().await.unwrap();
        let seen = String::from_utf8_lossy(&out.stdout);
        assert!(!seen.contains("CARGO_PKG_NAME"), "{seen}");
        assert!(seen.contains("PATH="), "{seen}");
    }

    #[test]
    fn lengths_read_as_a_clock() {
        assert_eq!(clock(0.4), "0:00");
        assert_eq!(clock(42.0), "0:42");
        assert_eq!(clock(62.6), "1:03");
        assert_eq!(clock(3723.0), "1:02:03");
    }
}
