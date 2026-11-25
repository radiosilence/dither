//! Everything read from the environment, once, at boot. A missing or
//! malformed value fails startup rather than the request that first needs it.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// The Ed25519 key invites are signed with, as PEM.
pub struct SigningKey {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
    /// Made for this run, with none configured: invites it signs stop working
    /// when the process does.
    pub generated: bool,
}

impl SigningKey {
    fn generate() -> Result<Self> {
        let (private, public) = koan_auth::keys::generate_pem()?;
        Ok(Self {
            private: private.into_bytes(),
            public: public.into_bytes(),
            generated: true,
        })
    }
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
    /// The object store, by URL: `s3://bucket[/prefix]`, or `memory:///` for
    /// development, which forgets everything when the process stops.
    pub storage: String,
    /// What invites are signed with. A database copy in the store must not
    /// be enough to forge one, so it is never kept there.
    pub signing_key: SigningKey,
    pub max_upload_bytes: u64,
    pub max_pixels: u64,
    /// Memory processing may take at once, across every upload. An image
    /// that needs more than this alone is refused.
    pub process_memory: u64,
    pub cache_bytes: u64,
    /// A cache on this machine's disk for a store on another, in bytes; 0
    /// for none. Unused with a store on this disk.
    pub disk_cache_bytes: u64,
    /// Uploads are refused while a disk they write to has less than this
    /// free, so a full image store never fills the disk the rest of the
    /// machine depends on. Unset, each disk keeps 15% of its size free.
    pub min_free_bytes: Option<u64>,
    /// Proxies in front of the one that reaches dither, such as a CDN's
    /// edges: `X-Forwarded-For` entries in these ranges are passed over when
    /// finding the client's address.
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// A header a trusted proxy sets to the client's address, such as
    /// Cloudflare's `CF-Connecting-IP`, for a proxy whose `X-Forwarded-For`
    /// the one in front does not keep. Read only when the request came from
    /// a trusted proxy, since anyone else could send it.
    pub address_header: Option<axum::http::HeaderName>,
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
    /// The web process's configuration.
    pub fn from_env() -> Result<Self> {
        Self::read(false)
    }

    /// A worker's: it serves nothing and signs nobody in, so needs no key.
    pub fn for_worker() -> Result<Self> {
        Self::read(true)
    }

    fn read(worker: bool) -> Result<Self> {
        let bind_addr = or("BIND_ADDR", "127.0.0.1:8080");
        let signing_key = match var("SIGNING_KEY") {
            Some(key) => {
                let private = signing_pem(&key)?;
                let public = koan_auth::keys::public_from_private(private.as_bytes())
                    .map_err(|e| anyhow::anyhow!("SIGNING_KEY: {e}"))?;
                SigningKey {
                    private: private.into_bytes(),
                    public: public.into_bytes(),
                    generated: false,
                }
            }
            // Only this machine reaches it, so a key for the run will do.
            None if worker || loopback(&bind_addr) => SigningKey::generate()?,
            None => bail!(
                "SIGNING_KEY is required: the Ed25519 key invites are signed with, as 32 random bytes in base64 or as PKCS#8 PEM"
            ),
        };
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
            // A worker touches no store but through the worker API.
            None if worker => "memory:///".into(),
            None => bail!("STORAGE is required: s3://bucket, or memory:/// for development"),
        };
        let trusted_proxies: Vec<ipnet::IpNet> = var("TRUSTED_PROXIES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<ipnet::IpNet>()
                    .or_else(|_| v.parse::<std::net::IpAddr>().map(Into::into))
                    .map_err(|_| {
                        anyhow::anyhow!("TRUSTED_PROXIES must be addresses or CIDR ranges, not {v}")
                    })
            })
            .collect::<Result<_>>()?;
        let address_header = var("ADDRESS_HEADER")
            .map(|v| {
                axum::http::HeaderName::from_bytes(v.trim().as_bytes())
                    .map_err(|_| anyhow::anyhow!("ADDRESS_HEADER must be a header name"))
            })
            .transpose()?;
        if address_header.is_some() && trusted_proxies.is_empty() {
            bail!("ADDRESS_HEADER is read only from TRUSTED_PROXIES, so it needs them");
        }
        Ok(Self {
            bind_addr,
            metrics_addr: var("METRICS_ADDR"),
            app_url,
            public_url,
            state_dir,
            storage,
            signing_key,
            max_upload_bytes: num("MAX_UPLOAD_BYTES", 64_000_000)?,
            max_pixels: num("MAX_PIXELS", 100_000_000)?,
            process_memory: num("PROCESS_MEMORY_BYTES", 1 << 30)?,
            cache_bytes: num("CACHE_BYTES", 256 << 20)?,
            disk_cache_bytes: num("DISK_CACHE_BYTES", 0)?,
            min_free_bytes: var("MIN_FREE_BYTES")
                .map(|v| {
                    v.trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("MIN_FREE_BYTES must be a number"))
                })
                .transpose()?,
            trusted_proxies,
            address_header,
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

    /// A configuration for tests: a store in memory, and a key of its own.
    #[cfg(test)]
    pub fn test(state_dir: PathBuf) -> Self {
        Self {
            bind_addr: "127.0.0.1:0".into(),
            metrics_addr: None,
            app_url: "http://app.test".into(),
            public_url: "http://app.test".into(),
            state_dir,
            storage: "memory:///".into(),
            signing_key: SigningKey::generate().expect("a test signing key"),
            max_upload_bytes: 8 << 20,
            max_pixels: 50_000_000,
            process_memory: 1 << 30,
            cache_bytes: 16 << 20,
            disk_cache_bytes: 0,
            min_free_bytes: Some(0),
            trusted_proxies: Vec::new(),
            address_header: None,
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

/// `SIGNING_KEY` as PKCS#8 PEM: given so, or as the key's 32-byte seed in
/// base64 (`openssl rand -base64 32`), which is what a secret generator makes.
fn signing_pem(value: &str) -> Result<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    let value = value.trim();
    if value.starts_with("-----BEGIN") {
        return Ok(value.to_string());
    }
    let seed = STANDARD
        .decode(value)
        .map_err(|_| anyhow::anyhow!("SIGNING_KEY must be 32 bytes in base64, or PKCS#8 PEM"))?;
    if seed.len() != 32 {
        bail!("SIGNING_KEY's seed must be 32 bytes, not {}", seed.len());
    }
    // PKCS#8 for Ed25519 is a fixed prefix and the seed (RFC 8410).
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&seed);
    Ok(format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        STANDARD.encode(der)
    ))
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
    fn a_signing_key_is_a_seed_or_a_pem() {
        use base64::Engine as _;
        let seed = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let from_seed = signing_pem(&seed).unwrap();
        let public = koan_auth::keys::public_from_private(from_seed.as_bytes()).unwrap();
        // The same key, given as the PEM it makes.
        assert_eq!(signing_pem(&from_seed).unwrap(), from_seed.trim());
        assert_eq!(
            koan_auth::keys::public_from_private(signing_pem(&from_seed).unwrap().as_bytes())
                .unwrap(),
            public
        );
        assert!(signing_pem("c2hvcnQ=").is_err());
        assert!(signing_pem("not base64!").is_err());
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
