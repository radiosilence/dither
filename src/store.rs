//! Where images live: one object store, a directory on disk or an S3 bucket
//! (Cloudflare R2, Hetzner, MinIO), chosen by `STORAGE`'s scheme.
//!
//! The database and every image go through the same store, so moving to a
//! bucket is a copy of one tree (`dither copy-store`) and a changed setting.
//! Keys are sharded by the id's first two characters, so a directory on disk
//! never holds more than a few thousand entries.
//!
//! An image's copies are its original, a thumbnail, a display copy when it
//! is large, and a link preview. A video's display copy is the one browsers
//! play, when the original is not; its thumbnail and preview are of a frame.
//! Videos are written from files and read in ranges, never held whole.
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
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload};

/// Objects larger than this are streamed rather than cached.
pub const CACHE_ITEM_MAX: u64 = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The upload, cleaned.
    Original,
    Thumb,
    Display,
    /// A link preview: a small JPEG.
    Preview,
}

impl Variant {
    /// What the worker API calls it.
    pub fn name(self) -> &'static str {
        match self {
            Variant::Original => "original",
            Variant::Thumb => "thumb",
            Variant::Display => "display",
            Variant::Preview => "preview",
        }
    }

    pub fn named(name: &str) -> Option<Self> {
        [
            Variant::Original,
            Variant::Thumb,
            Variant::Display,
            Variant::Preview,
        ]
        .into_iter()
        .find(|v| v.name() == name)
    }

    fn prefix(self) -> &'static str {
        match self {
            Variant::Original => "o",
            Variant::Thumb => "t",
            Variant::Display => "d",
            Variant::Preview => "p",
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
        // Thumbnails and display copies are what pages ask for next. An
        // original just stored is not, and a batch of them would push those
        // out.
        let original = key.prefix_matches(&Path::from(Variant::Original.prefix()));
        if !original && (bytes.len() as u64) <= CACHE_ITEM_MAX {
            self.cache.insert(key.clone(), bytes);
        } else {
            self.cache.remove(key);
        }
        Ok(())
    }

    /// Store what `body` yields under `key`, streamed rather than held whole
    /// (a video can be larger than the memory processing may take), and say
    /// its length and SHA-256, taken in the same pass.
    pub async fn put_stream<E>(
        &self,
        key: &Path,
        mut body: impl futures_util::Stream<Item = std::result::Result<Bytes, E>> + Unpin,
    ) -> Result<(u64, [u8; 32])>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        use sha2::Digest as _;
        use tokio::io::AsyncWriteExt as _;
        self.cache.remove(key);
        let mut writer = object_store::buffered::BufWriter::new(self.inner.clone(), key.clone());
        let mut hash = sha2::Sha256::new();
        let mut len = 0u64;
        let copied = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(anyhow::Error::from)?;
                hash.update(&chunk);
                len += chunk.len() as u64;
                writer.write_all(&chunk).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = copied {
            let _ = writer.abort().await;
            return Err(e).with_context(|| format!("storing {key}"));
        }
        writer
            .shutdown()
            .await
            .with_context(|| format!("storing {key}"))?;
        Ok((len, hash.finalize().into()))
    }

    /// Part of the object, as `range` asks: what of it there is, or that
    /// none of it is in the object; `None` when there is no object. One read
    /// when the range is in the object, as nearly every request's is.
    pub async fn get_range(
        &self,
        key: &Path,
        range: GetRange,
    ) -> object_store::Result<Option<Part>> {
        if let Some(hit) = self.cache.get(key) {
            crate::metrics::cache(true);
            let total = hit.len() as u64;
            return Ok(Some(match bounds(&range, total) {
                Some(r) => Part::Within {
                    body: Body::Bytes(hit.slice(r.start as usize..r.end as usize)),
                    range: r,
                    total,
                },
                None => Part::Outside { total },
            }));
        }
        let options = GetOptions {
            range: Some(range.clone()),
            ..GetOptions::default()
        };
        match self.inner.get_opts(key, options).await {
            Ok(result) => {
                let (r, total) = (result.range.clone(), result.meta.size);
                Ok(Some(Part::Within {
                    body: Body::Stream {
                        size: r.end - r.start,
                        stream: result.into_stream(),
                    },
                    range: r,
                    total,
                }))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            // Each store words a range outside the object differently, so
            // its length decides whether that is what this was.
            Err(e) => match self.inner.head(key).await {
                Ok(meta) if bounds(&range, meta.size).is_none() => {
                    Ok(Some(Part::Outside { total: meta.size }))
                }
                _ => Err(e),
            },
        }
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

    /// Every variant of image `id`. True when all are gone.
    /// Move what is stored at `from` to `to`, replacing it.
    pub async fn rename(&self, from: &Path, to: &Path) -> object_store::Result<()> {
        self.cache.remove(to);
        self.inner.rename(from, to).await
    }

    pub async fn delete_image(&self, id: &str) -> bool {
        let mut gone = true;
        for variant in [
            Variant::Original,
            Variant::Thumb,
            Variant::Display,
            Variant::Preview,
        ] {
            if let Err(e) = self.delete(&key(variant, id)).await {
                tracing::warn!(image = id, error = %e, "could not delete a stored copy; the sweep will retry");
                gone = false;
            }
        }
        gone
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

/// What a ranged read found.
pub enum Part {
    Within {
        body: Body,
        range: std::ops::Range<u64>,
        total: u64,
    },
    /// None of the range is in an object of `total` bytes.
    Outside { total: u64 },
}

/// The bytes `range` names in an object of `len`, or `None` when it names
/// none of them.
pub fn bounds(range: &GetRange, len: u64) -> Option<std::ops::Range<u64>> {
    let r = match range {
        GetRange::Bounded(r) => r.start..r.end.min(len),
        GetRange::Offset(start) => *start..len,
        GetRange::Suffix(n) => len.saturating_sub(*n)..len,
    };
    (r.start < r.end).then_some(r)
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

/// Delete the copies of images `ids`, recorded as strays, and forget the
/// record of each one the store confirms gone; the sweep retries the rest.
pub async fn discard(store: &Store, db: &crate::db::Db, ids: Vec<String>) {
    let mut gone = Vec::with_capacity(ids.len());
    for id in ids {
        if store.delete_image(&id).await {
            gone.push(id);
        }
    }
    if let Err(e) = db.settle_strays(gone).await {
        tracing::warn!(error = %e, "forgetting deleted copies failed; the sweep will find them gone");
    }
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
        assert_eq!(key(Variant::Preview, "q").as_ref(), "p/q/q");
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
        assert_eq!(store.read(&k).await.unwrap().as_deref(), Some(&b"jpeg"[..]));
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

    #[tokio::test]
    async fn a_file_is_stored_whole_and_read_back_in_ranges() {
        let bytes: Vec<u8> = (0..(3 << 20)).map(|i| (i % 251) as u8).collect();
        let chunks = bytes
            .chunks(700_000)
            .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
            .collect::<Vec<_>>();
        let store = Store::memory();
        let k = key(Variant::Original, "abcdefghij");
        let (len, sha) = store
            .put_stream(&k, futures_util::stream::iter(chunks))
            .await
            .unwrap();
        assert_eq!(len, bytes.len() as u64);
        assert_eq!(
            sha,
            <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
        assert_eq!(store.read(&k).await.unwrap().as_deref(), Some(&bytes[..]));
        let read = |body: Body| async {
            match body {
                Body::Bytes(b) => b.to_vec(),
                Body::Stream { stream, .. } => stream
                    .map(|c| c.unwrap().to_vec())
                    .collect::<Vec<_>>()
                    .await
                    .concat(),
            }
        };
        let within = |part: Option<Part>| match part {
            Some(Part::Within { body, range, total }) => (body, range, total),
            _ => panic!("expected part of the object"),
        };
        let (body, range, total) = within(
            store
                .get_range(&k, GetRange::Bounded(1000..2000))
                .await
                .unwrap(),
        );
        assert_eq!((range, total), (1000..2000, bytes.len() as u64));
        assert_eq!(read(body).await, &bytes[1000..2000]);
        let (body, range, _) = within(store.get_range(&k, GetRange::Suffix(10)).await.unwrap());
        assert_eq!(range.start, bytes.len() as u64 - 10);
        assert_eq!(read(body).await, &bytes[bytes.len() - 10..]);
        let past = bytes.len() as u64 + 5;
        assert!(matches!(
            store.get_range(&k, GetRange::Offset(past)).await.unwrap(),
            Some(Part::Outside { total }) if total == bytes.len() as u64
        ));
        // A cached object is sliced from memory.
        let t = key(Variant::Thumb, "abcdefghij");
        store
            .put(&t, Bytes::from_static(b"0123456789"))
            .await
            .unwrap();
        let (body, range, total) =
            within(store.get_range(&t, GetRange::Bounded(2..5)).await.unwrap());
        assert_eq!(
            (read(body).await, range, total),
            (b"234".to_vec(), 2..5, 10)
        );
        assert!(matches!(
            store.get_range(&t, GetRange::Offset(10)).await.unwrap(),
            Some(Part::Outside { total: 10 })
        ));
        assert!(
            store
                .get_range(&key(Variant::Original, "nothing"), GetRange::Offset(0))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ranges_fall_within_the_object_or_outside_it() {
        assert_eq!(bounds(&GetRange::Bounded(0..100), 50), Some(0..50));
        assert_eq!(bounds(&GetRange::Offset(10), 50), Some(10..50));
        assert_eq!(bounds(&GetRange::Offset(50), 50), None);
        assert_eq!(bounds(&GetRange::Suffix(500), 50), Some(0..50));
        assert_eq!(bounds(&GetRange::Suffix(0), 50), None);
        assert_eq!(bounds(&GetRange::Bounded(49..1000), 50), Some(49..50));
    }

    #[test]
    fn a_password_never_reaches_the_label() {
        let url = url::Url::parse("s3://key:secret@bucket/prefix").unwrap();
        assert!(!redact(&url).contains("secret"));
    }
}
