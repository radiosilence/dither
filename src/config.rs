//! Everything read from the environment, once, at boot. A missing or
//! malformed value fails startup rather than the request that first needs it.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub struct Oidc {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Where codes are exchanged. The issuer's own endpoint unless a pod
    /// cannot reach the issuer's public address (Cilium drops a pod's traffic
    /// to its own node's), when it is the provider's in-cluster service.
    pub token_url: String,
}

/// Whether videos are taken, and with what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    /// When ffmpeg and ffprobe are found.
    Auto,
    /// Never.
    Off,
    /// Always: startup fails without them.
    On,
}

pub struct Video {
    pub mode: VideoMode,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub max_bytes: u64,
    pub max_seconds: u64,
    /// Threads a transcode uses: the pod's CPU limit is shared with serving.
    pub threads: u32,
}

/// How many jobs a worker makes at once.
pub struct Workers {
    /// Pictures, each also held to the memory budget.
    pub pictures: usize,
    /// Videos, each an ffmpeg of a few hundred megabytes on `Video::threads`.
    pub videos: usize,
}

pub struct Config {
    pub bind_addr: String,
    /// `/metrics`, on a port of its own so the scraper is let in without
    /// reaching the app. Unset, there is none.
    pub metrics_addr: Option<String>,
    /// Where the app is used and signed in to.
    pub app_url: String,
    /// What share links name. The same as `app_url` until the images get a
    /// domain of their own; then that domain serves the public pages and
    /// images only, with no sign-in and no cookies.
    pub public_url: String,
    /// The SQLite database and uploads in progress. A local disk: SQLite over
    /// a network filesystem loses its locking.
    pub state_dir: PathBuf,
    /// The object store, by URL: `file:///abs/dir` or `s3://bucket[/prefix]`.
    pub storage: String,
    /// `None` only with `INSECURE_NO_AUTH=1`, for local development.
    pub oidc: Option<Oidc>,
    pub max_upload_bytes: u64,
    pub max_pixels: u64,
    /// Memory processing may take at once, across every upload. An image
    /// that needs more than this alone is refused.
    pub process_memory: u64,
    pub cache_bytes: u64,
    /// Uploads are refused while a disk they write to has less than this
    /// free, so a full image store never fills the disk the rest of the
    /// machine depends on. Unset, each disk keeps 15% of its size free.
    pub min_free_bytes: Option<u64>,
    pub video: Video,
    pub workers: Workers,
    /// Where the web process serves the worker API. Loopback unless workers
    /// run on other machines, when `worker_token` is required.
    pub worker_addr: String,
    /// What a worker presents to the worker API, and what the API asks for.
    pub worker_token: Option<String>,
    /// Where a worker reaches the worker API.
    pub queue_url: String,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn or(key: &str, default: &str) -> String {
    var(key).unwrap_or_else(|| default.to_string())
}

fn num<T: std::str::FromStr>(key: &str, default: T) -> Result<T> {
    match var(key) {
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("{key} must be a number")),
        None => Ok(default),
    }
}

/// `scheme://host[:port]`, from a URL that may carry a path or a trailing slash.
fn origin(key: &str, value: &str) -> Result<String> {
    let url = url::Url::parse(value).with_context(|| format!("{key} must be a URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("{key} must be an http(s) URL");
    }
    Ok(url.origin().ascii_serialization())
}

impl Config {
    /// The web process's configuration: sign-in is required.
    pub fn from_env() -> Result<Self> {
        Self::read(false)
    }

    /// A worker's: it serves nothing and signs nobody in, so needs neither
    /// the provider nor its secret.
    pub fn for_worker() -> Result<Self> {
        Self::read(true)
    }

    fn read(worker: bool) -> Result<Self> {
        let oidc = match (
            var("OIDC_ISSUER"),
            var("OIDC_CLIENT_ID"),
            var("OIDC_CLIENT_SECRET"),
        ) {
            (Some(issuer), Some(client_id), Some(client_secret)) => {
                let issuer = issuer.trim_end_matches('/').to_string();
                Some(Oidc {
                    token_url: or("OIDC_TOKEN_URL", &format!("{issuer}/oauth2/token")),
                    issuer,
                    client_id,
                    client_secret,
                })
            }
            (None, None, None) if worker || var("INSECURE_NO_AUTH").as_deref() == Some("1") => None,
            (None, None, None) => bail!(
                "OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET are required (or INSECURE_NO_AUTH=1 for local development)"
            ),
            _ => bail!("OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET must be set together"),
        };
        let bind_addr = or("BIND_ADDR", "127.0.0.1:8080");
        if oidc.is_none() && !worker && !loopback(&bind_addr) {
            bail!(
                "INSECURE_NO_AUTH serves the app with no sign-in, so BIND_ADDR must be loopback, not {bind_addr}"
            );
        }
        let app_url = origin("APP_URL", &or("APP_URL", &format!("http://{bind_addr}")))?;
        let public_url = origin("PUBLIC_URL", &or("PUBLIC_URL", &app_url))?;
        let state_dir = PathBuf::from(or("STATE_DIR", "data/state"));
        let worker_addr = or("WORKER_ADDR", "127.0.0.1:8081");
        let worker_token = var("WORKER_TOKEN");
        if !worker && worker_token.is_none() && !loopback(&worker_addr) {
            bail!(
                "WORKER_ADDR {worker_addr} is reachable from other machines, so WORKER_TOKEN is required"
            );
        }
        let queue_url = or("QUEUE_URL", "http://127.0.0.1:8081");
        if !url::Url::parse(&queue_url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
            bail!("QUEUE_URL must be an http(s) URL");
        }
        let queue_url = queue_url.trim_end_matches('/').to_string();
        let storage = match var("STORAGE") {
            Some(s) => s,
            None => {
                let dir = std::path::absolute("data/objects").context("resolving data/objects")?;
                url::Url::from_directory_path(&dir)
                    .map_err(|()| anyhow::anyhow!("{} is not an absolute path", dir.display()))?
                    .to_string()
            }
        };
        Ok(Self {
            bind_addr,
            metrics_addr: var("METRICS_ADDR"),
            app_url,
            public_url,
            state_dir,
            storage,
            oidc,
            max_upload_bytes: num("MAX_UPLOAD_BYTES", 64_000_000)?,
            max_pixels: num("MAX_PIXELS", 100_000_000)?,
            process_memory: num("PROCESS_MEMORY_BYTES", 1 << 30)?,
            cache_bytes: num("CACHE_BYTES", 256 << 20)?,
            min_free_bytes: var("MIN_FREE_BYTES")
                .map(|v| {
                    v.trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("MIN_FREE_BYTES must be a number"))
                })
                .transpose()?,
            video: Video {
                mode: match or("VIDEO", "auto").as_str() {
                    "auto" => VideoMode::Auto,
                    "off" => VideoMode::Off,
                    "on" => VideoMode::On,
                    other => bail!("VIDEO must be auto, on or off, not {other}"),
                },
                ffmpeg: PathBuf::from(or("FFMPEG", "ffmpeg")),
                ffprobe: PathBuf::from(or("FFPROBE", "ffprobe")),
                max_bytes: num("MAX_VIDEO_BYTES", 1_000_000_000)?,
                max_seconds: num("MAX_VIDEO_SECONDS", 1800)?,
                threads: num::<u32>("VIDEO_THREADS", 2)?.max(1),
            },
            workers: Workers {
                pictures: num("PICTURE_WORKERS", 4)?,
                videos: num("VIDEO_WORKERS", 1)?,
            },
            worker_addr,
            worker_token,
            queue_url,
        })
    }

    /// The largest video taken; `None` when none are.
    pub fn max_video_bytes(&self) -> Option<u64> {
        (self.video.mode != VideoMode::Off).then_some(self.video.max_bytes)
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.app_url)
    }

    /// The `Host` header the app answers to, and the one public pages do.
    pub fn app_host(&self) -> &str {
        host_of(&self.app_url)
    }

    pub fn public_host(&self) -> &str {
        host_of(&self.public_url)
    }

    /// Whether the public pages have a domain of their own.
    pub fn split_hosts(&self) -> bool {
        self.app_host() != self.public_host()
    }

    pub fn secure(&self) -> bool {
        self.app_url.starts_with("https://")
    }

    /// A configuration for tests: no sign-in, a store in memory.
    #[cfg(test)]
    pub fn test(state_dir: PathBuf) -> Self {
        Self {
            bind_addr: "127.0.0.1:0".into(),
            metrics_addr: None,
            app_url: "http://app.test".into(),
            public_url: "http://app.test".into(),
            state_dir,
            storage: "memory:///".into(),
            oidc: None,
            max_upload_bytes: 8 << 20,
            max_pixels: 50_000_000,
            process_memory: 1 << 30,
            cache_bytes: 16 << 20,
            min_free_bytes: Some(0),
            video: Video {
                mode: VideoMode::Auto,
                ffmpeg: PathBuf::from("ffmpeg"),
                ffprobe: PathBuf::from("ffprobe"),
                max_bytes: 64 << 20,
                max_seconds: 600,
                threads: 2,
            },
            workers: Workers {
                pictures: 2,
                videos: 1,
            },
            worker_addr: "127.0.0.1:0".into(),
            worker_token: None,
            queue_url: "http://127.0.0.1:0".into(),
        }
    }
}

fn loopback(addr: &str) -> bool {
    addr.starts_with("127.0.0.1:") || addr.starts_with("[::1]:") || addr.starts_with("localhost:")
}

fn host_of(origin: &str) -> &str {
    origin.split_once("://").map_or(origin, |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_drops_its_path_and_keeps_its_port() {
        assert_eq!(
            origin("X", "https://dither.example/").unwrap(),
            "https://dither.example"
        );
        assert_eq!(
            origin("X", "http://127.0.0.1:8080/x").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert!(origin("X", "ftp://x").is_err());
        assert!(origin("X", "dither.example").is_err());
    }

    #[test]
    fn hosts_come_from_the_origins() {
        let mut c = Config::test(PathBuf::new());
        assert!(!c.split_hosts());
        c.public_url = "https://pics.example".into();
        assert_eq!(c.public_host(), "pics.example");
        assert_eq!(c.app_host(), "app.test");
        assert!(c.split_hosts());
    }
}
