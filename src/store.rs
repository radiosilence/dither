//! Where images live: one object store, a directory on disk or an S3 bucket
//! (Cloudflare R2, Hetzner, MinIO), chosen by `STORAGE`'s scheme.
//!
//! The database and every image go through the same store, so moving to a
//! bucket is a copy of one tree (`dither copy-store`) and a changed setting.
//! Keys are sharded by the id's first two characters, so a directory on disk
//! never holds more than a few thousand entries.
//!
//! Reads go through an in-memory cache weighted by size. Thumbnails are what a
//! grid asks for a hundred at a time; originals are cached up to
//! [`CACHE_ITEM_MAX`] and streamed past it, so one popular large GIF cannot
//! take the memory every reader shares.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

/// Objects larger than this are streamed rather than cached.
pub const CACHE_ITEM_MAX: u64 = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The upload, cleaned.
    Original,
    Thumb,
    Display,
}

impl Variant {
    fn prefix(self) -> &'static str {
        match self {
            Variant::Original => "o",
            Variant::Thumb => "t",
            Variant::Display => "d",
        }
    }
}

/// Where a variant of image `id` is kept.
pub fn key(variant: Variant, id: &str) -> Path {
    let shard = id.get(..2).unwrap_or(id);
    Path::from(format!("{}/{shard}/{id}", variant.prefix()))
}

#[derive(Clone)]
struct Weigh;

impl quick_cache::Weighter<Path, Bytes> for Weigh {
    fn weight(&self, _: &Path, value: &Bytes) -> u64 {
        value.len() as u64
    }
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    cache: Arc<quick_cache::sync::Cache<Path, Bytes, Weigh>>,
    /// The directory, when the store is one: what free space is measured on.
    dir: Option<PathBuf>,
    /// What the store is, for logs, with no credential in it.
    label: String,
}

/// What a read returns: whole bytes, or a stream for an object too large to
/// hold.
pub enum Body {
    Bytes(Bytes),
    Stream {
        size: u64,
        stream: BoxStream<'static, object_store::Result<Bytes>>,
    },
}

impl Store {
    /// `file:///abs/path` or `s3://bucket[/prefix]`. An S3 store reads its
    /// endpoint, region and keys from the `AWS_*` variables (`AWS_ENDPOINT`,
    /// `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`).
    pub fn open(url: &str, cache_bytes: u64) -> Result<Self> {
        let parsed =
            url::Url::parse(url).with_context(|| format!("STORAGE is not a URL: {url}"))?;
        let (inner, dir): (Arc<dyn ObjectStore>, Option<PathBuf>) = match parsed.scheme() {
            "file" => {
                let dir = parsed
                    .to_file_path()
                    .map_err(|()| anyhow::anyhow!("STORAGE must be an absolute file:// path"))?;
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                let fs = object_store::local::LocalFileSystem::new_with_prefix(&dir)?
                    .with_automatic_cleanup(true);
                (Arc::new(fs), Some(dir))
            }
            "s3" => {
                let s3 = object_store::aws::AmazonS3Builder::from_env()
                    .with_url(url)
                    .build()
                    .context("configuring the S3 store")?;
                // The builder takes the bucket from the URL and ignores its
                // path, which is how one bucket holds more than one dither.
                let prefix = parsed.path().trim_matches('/');
                let s3: Arc<dyn ObjectStore> = if prefix.is_empty() {
                    Arc::new(s3)
                } else {
                    Arc::new(object_store::prefix::PrefixStore::new(s3, prefix))
                };
                (s3, None)
            }
            "memory" => (Arc::new(object_store::memory::InMemory::new()), None),
            other => anyhow::bail!("STORAGE must be file://, s3:// or memory://, not {other}://"),
        };
        Ok(Self::with(inner, dir, cache_bytes, redact(&parsed)))
    }

    fn with(
        inner: Arc<dyn ObjectStore>,
        dir: Option<PathBuf>,
        cache_bytes: u64,
        label: String,
    ) -> Self {
        // quick_cache wants an estimate of how many entries; thumbnails are
        // the common case at around 40 KB.
        let estimate = (cache_bytes / 40_000).clamp(64, 1 << 20) as usize;
        Self {
            inner,
            cache: Arc::new(quick_cache::sync::Cache::with_weighter(
                estimate,
                cache_bytes,
                Weigh,
            )),
            dir,
            label,
        }
    }

    /// An empty store in memory, for tests.
    #[cfg(test)]
    pub fn memory() -> Self {
        Self::with(
            Arc::new(object_store::memory::InMemory::new()),
            None,
            64 << 20,
            "memory://".into(),
        )
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn inner(&self) -> &Arc<dyn ObjectStore> {
        &self.inner
    }

    pub async fn put(&self, key: &Path, bytes: impl Into<Bytes>) -> object_store::Result<()> {
        let bytes: Bytes = bytes.into();
        self.inner.put(key, PutPayload::from(bytes.clone())).await?;
        if (bytes.len() as u64) <= CACHE_ITEM_MAX {
            self.cache.insert(key.clone(), bytes);
        } else {
            self.cache.remove(key);
        }
        Ok(())
    }

    /// The object, or `None` when there is none.
    pub async fn get(&self, key: &Path) -> object_store::Result<Option<Body>> {
        if let Some(hit) = self.cache.get(key) {
            crate::metrics::cache(true);
            return Ok(Some(Body::Bytes(hit)));
        }
        crate::metrics::cache(false);
        let result = match self.inner.get(key).await {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let size = result.meta.size;
        if size <= CACHE_ITEM_MAX {
            let bytes = result.bytes().await?;
            self.cache.insert(key.clone(), bytes.clone());
            Ok(Some(Body::Bytes(bytes)))
        } else {
            Ok(Some(Body::Stream {
                size,
                stream: result.into_stream(),
            }))
        }
    }

    /// The whole object, however large: for the snapshot and for copying.
    pub async fn read(&self, key: &Path) -> object_store::Result<Option<Bytes>> {
        match self.inner.get(key).await {
            Ok(r) => r.bytes().await.map(Some),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Gone, whether or not it was there.
    pub async fn delete(&self, key: &Path) -> object_store::Result<()> {
        self.cache.remove(key);
        match self.inner.delete(key).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Every variant of image `id`.
    pub async fn delete_image(&self, id: &str) {
        for variant in [Variant::Original, Variant::Thumb, Variant::Display] {
            if let Err(e) = self.delete(&key(variant, id)).await {
                tracing::warn!(image = id, error = %e, "could not delete a stored copy; the sweep will retry");
            }
        }
    }

    /// Ids of originals in the store, with when each was written (ms since the
    /// epoch).
    pub fn originals(&self) -> BoxStream<'static, object_store::Result<(String, i64)>> {
        self.inner
            .list(Some(&Path::from(Variant::Original.prefix())))
            .map(|meta| {
                meta.map(|m| {
                    let id = m.location.filename().unwrap_or_default().to_string();
                    (id, m.last_modified.timestamp_millis())
                })
            })
            .boxed()
    }

    /// Bytes free on the disk under a file store; `None` for a bucket.
    pub fn free_bytes(&self) -> Option<u64> {
        disk(self.dir.as_deref()?).map(|(free, _)| free)
    }

    /// The directory, for a file store.
    pub fn dir(&self) -> Option<&FsPath> {
        self.dir.as_deref()
    }
}

/// Bytes free to an unprivileged writer, and the size, of the filesystem
/// holding `dir`.
pub fn disk(dir: &FsPath) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(dir).ok()?;
    Some((
        stat.f_bavail.saturating_mul(stat.f_frsize),
        stat.f_blocks.saturating_mul(stat.f_frsize),
    ))
}

/// `url` with any password removed, for logs.
fn redact(url: &url::Url) -> String {
    let mut url = url.clone();
    let _ = url.set_password(None);
    url.to_string()
}

/// Copy every object from one store to another: how a file store moves to a
/// bucket. Objects already at the destination with the same size are skipped,
/// so an interrupted copy resumes.
pub async fn copy(from: &Store, to: &Store) -> Result<(u64, u64)> {
    let mut listing = from.inner.list(None);
    let (mut copied, mut skipped) = (0u64, 0u64);
    while let Some(meta) = listing.next().await {
        let meta = meta?;
        if let Ok(existing) = to.inner.head(&meta.location).await
            && existing.size == meta.size
        {
            skipped += 1;
            continue;
        }
        let bytes = from
            .read(&meta.location)
            .await?
            .with_context(|| format!("{} vanished while copying", meta.location))?;
        to.inner
            .put(&meta.location, PutPayload::from(bytes))
            .await?;
        copied += 1;
        if copied.is_multiple_of(500) {
            tracing::info!(copied, skipped, "copying");
        }
    }
    Ok((copied, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_sharded_by_the_id() {
        assert_eq!(
            key(Variant::Original, "abcdefghij").as_ref(),
            "o/ab/abcdefghij"
        );
        assert_eq!(key(Variant::Thumb, "Zx9").as_ref(), "t/Zx/Zx9");
        assert_eq!(key(Variant::Display, "q").as_ref(), "d/q/q");
    }

    #[tokio::test]
    async fn a_put_object_reads_back_and_a_deleted_one_does_not() {
        let store = Store::memory();
        let k = key(Variant::Thumb, "abcdefghij");
        store.put(&k, Bytes::from_static(b"webp")).await.unwrap();
        let Some(Body::Bytes(b)) = store.get(&k).await.unwrap() else {
            panic!("expected bytes");
        };
        assert_eq!(&b[..], b"webp");
        store.delete(&k).await.unwrap();
        assert!(store.get(&k).await.unwrap().is_none());
        // Deleting what is not there is not an error.
        store.delete(&k).await.unwrap();
    }

    #[tokio::test]
    async fn a_large_object_streams_rather_than_filling_the_cache() {
        let store = Store::memory();
        let k = key(Variant::Original, "abcdefghij");
        let big = vec![7u8; (CACHE_ITEM_MAX + 1) as usize];
        store.put(&k, big.clone()).await.unwrap();
        match store.get(&k).await.unwrap() {
            Some(Body::Stream { size, stream }) => {
                assert_eq!(size, CACHE_ITEM_MAX + 1);
                let got: Vec<u8> = stream
                    .map(|c| c.unwrap().to_vec())
                    .collect::<Vec<_>>()
                    .await
                    .concat();
                assert_eq!(got, big);
            }
            _ => panic!("expected a stream"),
        }
    }

    #[tokio::test]
    async fn a_file_store_works_on_disk_and_reports_free_space() {
        let dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(dir.path()).unwrap();
        let store = Store::open(url.as_str(), 1 << 20).unwrap();
        let k = key(Variant::Original, "abcdefghij");
        store.put(&k, Bytes::from_static(b"jpeg")).await.unwrap();
        assert!(dir.path().join("o/ab/abcdefghij").is_file());
        assert!(store.free_bytes().unwrap() > 0);
        let ids: Vec<_> = store.originals().map(|r| r.unwrap().0).collect().await;
        assert_eq!(ids, vec!["abcdefghij".to_string()]);
    }

    #[tokio::test]
    async fn copy_moves_every_object_and_resumes() {
        let (a, b) = (Store::memory(), Store::memory());
        for id in ["aaaaaaaaaa", "bbbbbbbbbb"] {
            a.put(&key(Variant::Original, id), Bytes::from_static(b"x"))
                .await
                .unwrap();
        }
        assert_eq!(copy(&a, &b).await.unwrap(), (2, 0));
        assert_eq!(copy(&a, &b).await.unwrap(), (0, 2));
        assert!(
            b.read(&key(Variant::Original, "bbbbbbbbbb"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_password_never_reaches_the_label() {
        let url = url::Url::parse("s3://key:secret@bucket/prefix").unwrap();
        assert!(!redact(&url).contains("secret"));
    }
}
