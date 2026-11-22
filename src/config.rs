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
    /// Megapixels being decoded at once, across every upload: what bounds
    /// the memory processing can take.
    pub process_megapixels: u32,
    pub cache_bytes: u64,
    /// Uploads are refused while the disk under a file store has less than
    /// this free, so a full image store never fills the disk the rest of the
    /// machine depends on.
    pub min_free_bytes: u64,
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
    pub fn from_env() -> Result<Self> {
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
            (None, None, None) if var("INSECURE_NO_AUTH").as_deref() == Some("1") => None,
            (None, None, None) => bail!(
                "OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET are required (or INSECURE_NO_AUTH=1 for local development)"
            ),
            _ => bail!("OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET must be set together"),
        };
        let bind_addr = or("BIND_ADDR", "127.0.0.1:8080");
        if oidc.is_none()
            && !bind_addr.starts_with("127.0.0.1:")
            && !bind_addr.starts_with("[::1]:")
        {
            bail!(
                "INSECURE_NO_AUTH serves the app with no sign-in, so BIND_ADDR must be loopback, not {bind_addr}"
            );
        }
        let app_url = origin("APP_URL", &or("APP_URL", &format!("http://{bind_addr}")))?;
        let public_url = origin("PUBLIC_URL", &or("PUBLIC_URL", &app_url))?;
        let state_dir = PathBuf::from(or("STATE_DIR", "data/state"));
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
            process_megapixels: num("PROCESS_MEGAPIXELS", 100)?.max(1),
            cache_bytes: num("CACHE_BYTES", 256 << 20)?,
            min_free_bytes: num("MIN_FREE_BYTES", 0)?,
        })
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
            process_megapixels: 100,
            cache_bytes: 16 << 20,
            min_free_bytes: 0,
        }
    }
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
