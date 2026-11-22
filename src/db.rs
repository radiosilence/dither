//! SQLite: users, sessions, albums, images and uploads in progress.
//!
//! One file in the state directory, reached through a small pool of rusqlite
//! connections as slsk-mcp reaches its own. rusqlite blocks, so every
//! statement runs on tokio's blocking pool through [`Db::run`] and never holds
//! an async worker. WAL lets reads run beside a write; a transaction that
//! writes opens with `BEGIN IMMEDIATE`, so it waits for the lock up front
//! rather than failing when it upgrades a read.
//!
//! Every statement that changes something takes the owner and checks it, so
//! a handler cannot forget to: a request naming someone else's album changes
//! nothing and reads as not found.
//!
//! Timestamps are milliseconds since the epoch. Each write bumps a counter
//! ([`Db::writes`]) that the snapshotter watches, and a write to someone's
//! albums names them to the live pages showing those ([`Db::changes`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use tokio::sync::{broadcast, watch};

use crate::sanitize::Kind;

pub use rusqlite::Result;

const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

/// Idle connections kept: more than the service's usual concurrency, so the
/// steady state never opens one.
const KEEP_IDLE: usize = 8;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[derive(Clone)]
pub struct Db(Arc<Pool>);

struct Pool {
    path: PathBuf,
    idle: parking_lot::Mutex<Vec<Connection>>,
    writes: watch::Sender<u64>,
    changes: broadcast::Sender<Arc<str>>,
}

/// Who hears that a call changed something.
enum Announce {
    /// Nobody: upload progress, and the database's own bookkeeping.
    Quiet,
    /// The snapshotter.
    Writes,
    /// The snapshotter, and the pages showing this person's albums.
    Owner(Arc<str>),
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let applied = conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as usize;
    for (n, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (n + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

impl Db {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Self> {
            let mut conn = Connection::open(&path)?;
            configure(&conn)?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            migrate(&mut conn)?;
            Ok(Self(Arc::new(Pool {
                path,
                idle: parking_lot::Mutex::new(vec![conn]),
                writes: watch::channel(0).0,
                changes: broadcast::channel(1024).0,
            })))
        })
        .await?
    }

    /// A fresh database in a temporary directory, for tests: a file rather
    /// than `:memory:` so every pooled connection sees the same data.
    #[cfg(test)]
    pub async fn temp() -> Self {
        let dir = tempfile::tempdir().expect("temp dir").keep();
        Self::open(&dir.join("dither.db"))
            .await
            .expect("test database")
    }

    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Bumped after every call that changed a row.
    pub fn writes(&self) -> watch::Receiver<u64> {
        self.0.writes.subscribe()
    }

    /// The owner of each album or image a call changed, once per call, for
    /// the pages showing them. A receiver that falls behind is told it
    /// lagged, and redraws.
    pub fn changes(&self) -> broadcast::Receiver<Arc<str>> {
        self.0.changes.subscribe()
    }

    /// Run `f` on a pooled connection, on the blocking pool.
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.exec(Announce::Writes, f).await
    }

    /// [`run`](Self::run) for a write nothing watches: an upload's progress,
    /// which arrives a chunk at a time and which no page draws and no snapshot
    /// needs (a restored database has none of the partial files it counts).
    pub async fn run_quiet<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.exec(Announce::Quiet, f).await
    }

    /// [`run`](Self::run) for a change to `owner`'s albums or images.
    async fn run_as<T, F>(&self, owner: &str, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.exec(Announce::Owner(owner.into()), f).await
    }

    /// [`run_as`](Self::run_as) in a transaction that takes the write lock up
    /// front.
    async fn write_as<T, F>(&self, owner: &str, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Transaction) -> Result<T> + Send + 'static,
    {
        self.run_as(owner, move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
        .await
    }

    async fn exec<T, F>(&self, announce: Announce, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let pool = self.0.clone();
        match tokio::task::spawn_blocking(move || {
            let mut conn = match pool.idle.lock().pop() {
                Some(c) => c,
                None => {
                    let c = Connection::open(&pool.path)?;
                    configure(&c)?;
                    c
                }
            };
            let before = conn.total_changes();
            let out = f(&mut conn);
            if conn.total_changes() != before {
                match announce {
                    Announce::Quiet => {}
                    Announce::Writes => pool.writes.send_modify(|n| *n += 1),
                    Announce::Owner(owner) => {
                        pool.writes.send_modify(|n| *n += 1);
                        // No page listening is not an error.
                        let _ = pool.changes.send(owner);
                    }
                }
            }
            let mut idle = pool.idle.lock();
            if idle.len() < KEEP_IDLE {
                idle.push(conn);
            }
            out
        })
        .await
        {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    pub id: String,
    pub owner: String,
    pub title: String,
    pub description: String,
    pub cover: Option<String>,
    pub private: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Album {
    fn from_row(r: &Row) -> Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            owner: r.get("owner")?,
            title: r.get("title")?,
            description: r.get("description")?,
            cover: r.get("cover")?,
            private: r.get("private")?,
            created_at: r.get("created_at")?,
            updated_at: r.get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub id: String,
    pub album: String,
    pub position: i64,
    pub caption: String,
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub animated: bool,
    pub display: bool,
    pub sha256: Vec<u8>,
    pub name: String,
    pub created_at: i64,
}

impl Image {
    fn from_row(r: &Row) -> Result<Self> {
        let kind: String = r.get("kind")?;
        Ok(Self {
            id: r.get("id")?,
            album: r.get("album")?,
            position: r.get("position")?,
            caption: r.get("caption")?,
            kind: Kind::parse(&kind).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    format!("unknown kind {kind}").into(),
                )
            })?,
            width: r.get("width")?,
            height: r.get("height")?,
            bytes: r.get::<_, i64>("bytes")? as u64,
            animated: r.get("animated")?,
            display: r.get("display")?,
            sha256: r.get("sha256")?,
            name: r.get("name")?,
            created_at: r.get("created_at")?,
        })
    }

    /// The entity tag every copy of it is served under, with the variant's
    /// letter so the copies differ.
    pub fn etag(&self, variant: char) -> String {
        let hex: String = self
            .sha256
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("\"{hex}{variant}\"")
    }
}

/// An album as its owner's list shows it.
#[derive(Debug, Clone)]
pub struct Card {
    pub album: Album,
    pub count: u64,
    pub bytes: u64,
    pub cover: Option<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub id: String,
    pub owner: String,
    pub album: String,
    pub position: i64,
    pub name: String,
    pub size: u64,
    pub received: u64,
    pub image: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
}

impl Upload {
    fn from_row(r: &Row) -> Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            owner: r.get("owner")?,
            album: r.get("album")?,
            position: r.get("position")?,
            name: r.get("name")?,
            size: r.get::<_, i64>("size")? as u64,
            received: r.get::<_, i64>("received")? as u64,
            image: r.get("image")?,
            error: r.get("error")?,
            created_at: r.get("created_at")?,
        })
    }
}

/// What a finished upload adds.
#[derive(Debug, Clone)]
pub struct NewImage {
    pub id: String,
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub animated: bool,
    pub display: bool,
    pub sha256: Vec<u8>,
}

/// Fields of an album a person can change, each only when given.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AlbumEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub private: Option<bool>,
}

/// Record copies of images `ids` as no row's, for the sweep, should the store
/// not confirm them gone.
fn strays(tx: &rusqlite::Transaction, ids: &[String]) -> Result<()> {
    let mut insert =
        tx.prepare_cached("INSERT OR REPLACE INTO strays (id, since) VALUES (?1, ?2)")?;
    for id in ids {
        insert.execute(params![id, now()])?;
    }
    Ok(())
}

/// Positions reserved at the end of an album, or `None` if `owner` does not
/// own it.
fn reserve(tx: &rusqlite::Transaction, owner: &str, album: &str, n: usize) -> Result<Option<i64>> {
    tx.query_row(
        "UPDATE albums SET next_position = next_position + ?1, updated_at = ?2 \
         WHERE id = ?3 AND owner = ?4 RETURNING next_position - ?1",
        params![n as i64, now(), album, owner],
        |r| r.get(0),
    )
    .optional()
}

/// `?`-list placeholders for an `IN` over `ids`, numbered from `from`.
fn placeholders(from: usize, n: usize) -> String {
    (from..from + n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

impl Db {
    pub async fn upsert_user(&self, sub: &str, login: &str) -> Result<()> {
        let (sub, login) = (sub.to_string(), login.to_string());
        self.run(move |c| {
            c.execute(
                "INSERT INTO users (sub, login, created_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (sub) DO UPDATE SET login = excluded.login",
                params![sub, login, now()],
            )
            .map(drop)
        })
        .await
    }

    pub async fn login_of(&self, sub: &str) -> Result<Option<String>> {
        let sub = sub.to_string();
        self.run(move |c| {
            c.query_row("SELECT login FROM users WHERE sub = ?1", [sub], |r| {
                r.get(0)
            })
            .optional()
        })
        .await
    }

    pub async fn create_album(&self, owner: &str, title: &str) -> Result<Album> {
        let album = Album {
            id: crate::id::new(),
            owner: owner.to_string(),
            title: title.trim().to_string(),
            description: String::new(),
            cover: None,
            private: false,
            created_at: now(),
            updated_at: now(),
        };
        let a = album.clone();
        self.run_as(owner, move |c| {
            c.execute(
                "INSERT INTO albums (id, owner, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![a.id, a.owner, a.title, a.created_at],
            )
        })
        .await?;
        Ok(album)
    }

    /// `owner`'s albums, newest first, each with its count, size and cover.
    pub async fn cards(&self, owner: &str) -> Result<Vec<Card>> {
        let owner = owner.to_string();
        self.run(move |c| {
            let mut cards = c
                .prepare_cached(
                    "SELECT a.*, \
                       (SELECT COUNT(*) FROM images WHERE album = a.id) AS count, \
                       (SELECT COALESCE(SUM(bytes), 0) FROM images WHERE album = a.id) AS total, \
                       COALESCE(a.cover, (SELECT id FROM images WHERE album = a.id ORDER BY position, created_at LIMIT 1)) AS cover_id \
                     FROM albums a WHERE a.owner = ?1 ORDER BY a.created_at DESC",
                )?
                .query_map([&owner], |r| {
                    Ok((
                        Album::from_row(r)?,
                        r.get::<_, i64>("count")? as u64,
                        r.get::<_, i64>("total")? as u64,
                        r.get::<_, Option<String>>("cover_id")?,
                    ))
                })?
                .collect::<Result<Vec<_>>>()?;
            let ids: Vec<String> = cards.iter().filter_map(|c| c.3.clone()).collect();
            let covers = images_by_id(c, &ids)?;
            Ok(cards
                .drain(..)
                .map(|(album, count, bytes, cover)| Card {
                    album,
                    count,
                    bytes,
                    cover: cover.and_then(|id| covers.iter().find(|i| i.id == id).cloned()),
                })
                .collect())
        })
        .await
    }

    pub async fn album(&self, id: &str) -> Result<Option<Album>> {
        let id = id.to_string();
        self.run(move |c| {
            c.prepare_cached("SELECT * FROM albums WHERE id = ?1")?
                .query_row([id], Album::from_row)
                .optional()
        })
        .await
    }

    /// An album's images in order.
    pub async fn images(&self, album: &str) -> Result<Vec<Image>> {
        let album = album.to_string();
        self.run(move |c| {
            c.prepare_cached("SELECT * FROM images WHERE album = ?1 ORDER BY position, created_at")?
                .query_map([album], Image::from_row)?
                .collect()
        })
        .await
    }

    /// An image and the album it is in: everything serving it needs.
    pub async fn image(&self, id: &str) -> Result<Option<(Image, Album)>> {
        let id = id.to_string();
        self.run(move |c| {
            let Some(image) = c
                .prepare_cached("SELECT * FROM images WHERE id = ?1")?
                .query_row([&id], Image::from_row)
                .optional()?
            else {
                return Ok(None);
            };
            let album = c
                .prepare_cached("SELECT * FROM albums WHERE id = ?1")?
                .query_row([&image.album], Album::from_row)?;
            Ok(Some((image, album)))
        })
        .await
    }

    /// Whether anything changed: false when `owner` does not own the album.
    pub async fn edit_album(&self, owner: &str, id: &str, edit: AlbumEdit) -> Result<bool> {
        let (o, id) = (owner.to_string(), id.to_string());
        self.run_as(owner, move |c| {
            c.execute(
                "UPDATE albums SET title = COALESCE(?1, title), \
                   description = COALESCE(?2, description), \
                   private = COALESCE(?3, private), updated_at = ?4 \
                 WHERE id = ?5 AND owner = ?6",
                params![
                    edit.title.as_deref().map(str::trim),
                    edit.description.as_deref().map(str::trim_end),
                    edit.private,
                    now(),
                    id,
                    o
                ],
            )
            .map(|n| n == 1)
        })
        .await
    }

    /// The ids of the images that went with it, for their files to follow;
    /// `None` when `owner` does not own it. Their copies are recorded as
    /// strays until the store confirms them gone.
    pub async fn delete_album(&self, owner: &str, id: &str) -> Result<Option<Vec<String>>> {
        let (o, id) = (owner.to_string(), id.to_string());
        self.write_as(owner, move |tx| {
            let images = tx
                .prepare(
                    "SELECT i.id FROM images i JOIN albums a ON a.id = i.album \
                     WHERE a.id = ?1 AND a.owner = ?2",
                )?
                .query_map(params![id, o], |r| r.get(0))?
                .collect::<Result<Vec<String>>>()?;
            if tx.execute(
                "DELETE FROM albums WHERE id = ?1 AND owner = ?2",
                params![id, o],
            )? == 0
            {
                return Ok(None);
            }
            strays(tx, &images)?;
            Ok(Some(images))
        })
        .await
    }

    /// `image` as the album's cover, or the first image again with `None`.
    pub async fn set_cover(&self, owner: &str, album: &str, image: Option<&str>) -> Result<bool> {
        let (o, album, image) = (
            owner.to_string(),
            album.to_string(),
            image.map(str::to_string),
        );
        self.run_as(owner, move |c| {
            c.execute(
                "UPDATE albums SET cover = ?1, updated_at = ?2 WHERE id = ?3 AND owner = ?4 \
                 AND (?1 IS NULL OR EXISTS (SELECT 1 FROM images WHERE id = ?1 AND album = ?3))",
                params![image, now(), album, o],
            )
            .map(|n| n == 1)
        })
        .await
    }

    pub async fn set_caption(&self, owner: &str, image: &str, caption: &str) -> Result<bool> {
        let (o, image, caption) = (
            owner.to_string(),
            image.to_string(),
            caption.trim_end().to_string(),
        );
        self.run_as(owner, move |c| {
            c.execute(
                "UPDATE images SET caption = ?1 WHERE id = ?2 \
                 AND album IN (SELECT id FROM albums WHERE owner = ?3)",
                params![caption, image, o],
            )
            .map(|n| n == 1)
        })
        .await
    }

    /// Put `ids` first, in that order, and the album's other images after
    /// them as they were. Ids not in the album are ignored, so a list drawn
    /// before an upload landed still applies.
    pub async fn reorder(&self, owner: &str, album: &str, ids: Vec<String>) -> Result<bool> {
        let (o, album) = (owner.to_string(), album.to_string());
        self.write_as(owner, move |tx| {
            if tx.execute(
                "UPDATE albums SET updated_at = ?1 WHERE id = ?2 AND owner = ?3",
                params![now(), album, o],
            )? == 0
            {
                return Ok(false);
            }
            let current = tx
                .prepare("SELECT id FROM images WHERE album = ?1 ORDER BY position, created_at")?
                .query_map([&album], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>>>()?;
            let present: HashSet<&String> = current.iter().collect();
            let mut placed = HashSet::new();
            let first: Vec<&String> = ids
                .iter()
                .filter(|id| present.contains(id) && placed.insert(id.as_str()))
                .collect();
            let order: Vec<&String> = first
                .into_iter()
                .chain(current.iter().filter(|id| !placed.contains(id.as_str())))
                .collect();
            let mut set = tx.prepare(
                "UPDATE images SET position = ?1 WHERE id = ?2 \
                 AND album IN (SELECT id FROM albums WHERE id = ?3 AND owner = ?4)",
            )?;
            for (position, id) in order.iter().enumerate() {
                set.execute(params![position as i64, id, album, o])?;
            }
            tx.execute(
                "UPDATE albums SET next_position = MAX(next_position, ?1) WHERE id = ?2 AND owner = ?3",
                params![order.len() as i64, album, o],
            )?;
            Ok(true)
        })
        .await
    }

    /// Move `ids` to the end of album `to`, in that order. Only images in
    /// `owner`'s albums move; the count says how many did.
    pub async fn move_images(&self, owner: &str, ids: Vec<String>, to: &str) -> Result<usize> {
        let (o, to) = (owner.to_string(), to.to_string());
        self.write_as(owner, move |tx| {
            let Some(start) = reserve(tx, &o, &to, ids.len())? else {
                return Ok(0);
            };
            let mut moved = 0;
            for (i, id) in ids.iter().enumerate() {
                let n = tx.execute(
                    "UPDATE images SET album = ?1, position = ?2 WHERE id = ?3 AND album != ?1 \
                     AND album IN (SELECT id FROM albums WHERE owner = ?4)",
                    params![to, start + i as i64, id, o],
                )?;
                if n == 1 {
                    // A cover that left its album no longer covers it.
                    tx.execute(
                        "UPDATE albums SET cover = NULL WHERE cover = ?1 AND id != ?2 AND owner = ?3",
                        params![id, to, o],
                    )?;
                    moved += 1;
                }
            }
            Ok(moved)
        })
        .await
    }

    /// The ids actually deleted, for their files to follow. Their copies are
    /// recorded as strays until the store confirms them gone.
    pub async fn delete_images(&self, owner: &str, ids: Vec<String>) -> Result<Vec<String>> {
        let o = owner.to_string();
        self.write_as(owner, move |tx| {
            let mut deleted = Vec::new();
            for id in &ids {
                let n = tx.execute(
                    "DELETE FROM images WHERE id = ?1 AND album IN (SELECT id FROM albums WHERE owner = ?2)",
                    params![id, o],
                )?;
                if n == 1 {
                    tx.execute(
                        "UPDATE albums SET cover = NULL WHERE cover = ?1 AND owner = ?2",
                        params![id, o],
                    )?;
                    deleted.push(id.clone());
                }
            }
            strays(tx, &deleted)?;
            Ok(deleted)
        })
        .await
    }

    /// One upload per file, at the end of `album` in the order given, or
    /// `None` when `owner` does not own it.
    pub async fn begin_uploads(
        &self,
        owner: &str,
        album: &str,
        files: Vec<(String, u64)>,
    ) -> Result<Option<Vec<Upload>>> {
        let (o, album) = (owner.to_string(), album.to_string());
        self.write_as(owner, move |tx| {
            let Some(start) = reserve(tx, &o, &album, files.len())? else {
                return Ok(None);
            };
            let created_at = now();
            let mut uploads = Vec::with_capacity(files.len());
            let mut insert = tx.prepare(
                "INSERT INTO uploads (id, owner, album, position, name, size, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (i, (name, size)) in files.into_iter().enumerate() {
                let upload = Upload {
                    id: crate::id::new(),
                    owner: o.clone(),
                    album: album.clone(),
                    position: start + i as i64,
                    name,
                    size,
                    received: 0,
                    image: None,
                    error: None,
                    created_at,
                };
                insert.execute(params![
                    upload.id,
                    upload.owner,
                    upload.album,
                    upload.position,
                    upload.name,
                    upload.size as i64,
                    upload.created_at
                ])?;
                uploads.push(upload);
            }
            Ok(Some(uploads))
        })
        .await
    }

    pub async fn upload(&self, id: &str) -> Result<Option<Upload>> {
        let id = id.to_string();
        self.run(move |c| {
            c.prepare_cached("SELECT * FROM uploads WHERE id = ?1")?
                .query_row([id], Upload::from_row)
                .optional()
        })
        .await
    }

    /// False when the upload is not `owner`'s or has ended.
    pub async fn set_received(&self, owner: &str, id: &str, received: u64) -> Result<bool> {
        let (owner, id) = (owner.to_string(), id.to_string());
        self.run_quiet(move |c| {
            c.execute(
                "UPDATE uploads SET received = ?1 \
                 WHERE id = ?2 AND owner = ?3 AND image IS NULL AND error IS NULL",
                params![received as i64, id, owner],
            )
            .map(|n| n == 1)
        })
        .await
    }

    /// The upload's image, added where its upload reserved, claiming the
    /// copies [`add_stray`](Self::add_stray) recorded. False when the upload
    /// is gone (its album was deleted while the file was on its way), is not
    /// `owner`'s, or ended another way first (it was cancelled), or when the
    /// sweep took the copies' record first.
    pub async fn finish_upload(&self, owner: &str, upload: &str, image: NewImage) -> Result<bool> {
        let (o, upload) = (owner.to_string(), upload.to_string());
        self.write_as(owner, move |tx| {
            let Some((album, position, name)) = tx
                .query_row(
                    "SELECT album, position, name FROM uploads \
                     WHERE id = ?1 AND owner = ?2 AND image IS NULL AND error IS NULL",
                    params![upload, o],
                    |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
                })
                .optional()?
            else {
                return Ok(false);
            };
            // Whoever removes the record first has the copies: the sweep, to
            // delete them, or this, to keep them.
            if tx.execute("DELETE FROM strays WHERE id = ?1", [&image.id])? == 0 {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO images (id, album, position, kind, width, height, bytes, animated, display, sha256, name, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    image.id,
                    album,
                    position,
                    image.kind.as_str(),
                    image.width,
                    image.height,
                    image.bytes as i64,
                    image.animated,
                    image.display,
                    image.sha256,
                    name,
                    now()
                ],
            )?;
            tx.execute(
                "UPDATE uploads SET image = ?1 WHERE id = ?2 AND owner = ?3",
                params![image.id, upload, o],
            )?;
            tx.execute(
                "UPDATE albums SET updated_at = ?1 WHERE id = ?2 AND owner = ?3",
                params![now(), album, o],
            )?;
            Ok(true)
        })
        .await
    }

    /// End an upload that has not ended, recording why.
    pub async fn fail_upload(&self, owner: &str, id: &str, error: &str) -> Result<()> {
        let (owner, id, error) = (owner.to_string(), id.to_string(), error.to_string());
        self.run(move |c| {
            c.execute(
                "UPDATE uploads SET error = ?1 \
                 WHERE id = ?2 AND owner = ?3 AND image IS NULL AND error IS NULL",
                params![error, id, owner],
            )
            .map(drop)
        })
        .await
    }

    /// Uploads started before `before`, finished or not: their records and
    /// partial files are swept.
    pub async fn sweep_uploads(&self, before: i64) -> Result<Vec<String>> {
        self.run(move |c| {
            c.prepare("DELETE FROM uploads WHERE created_at < ?1 RETURNING id")?
                .query_map([before], |r| r.get(0))?
                .collect()
        })
        .await
    }

    /// Record copies of image `id` about to be stored, before any is: should
    /// recording the image fail, or the process stop, the sweep knows to
    /// remove them. Quiet, as a restored database without the record only
    /// leaves the copies in place.
    pub async fn add_stray(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.run_quiet(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO strays (id, since) VALUES (?1, ?2)",
                params![id, now()],
            )
            .map(drop)
        })
        .await
    }

    /// Forget the records of copies the store confirmed gone.
    pub async fn settle_strays(&self, ids: Vec<String>) -> Result<()> {
        self.run_quiet(move |c| {
            let mut delete = c.prepare_cached("DELETE FROM strays WHERE id = ?1")?;
            for id in &ids {
                delete.execute([id])?;
            }
            Ok(())
        })
        .await
    }

    /// Take the records of copies recorded before `before` that no image
    /// claims, for the sweep to delete. Taken, so an upload finishing after
    /// this can no longer claim them.
    pub async fn claim_strays(&self, before: i64) -> Result<Vec<String>> {
        self.run_quiet(move |c| {
            c.prepare(
                "DELETE FROM strays WHERE since < ?1 \
                 AND NOT EXISTS (SELECT 1 FROM images WHERE images.id = strays.id) RETURNING id",
            )?
            .query_map([before], |r| r.get(0))?
            .collect()
        })
        .await
    }

    /// Count one more copy of the database taken, and say which this is.
    /// Quiet, or taking a copy would ask for another.
    pub async fn next_generation(&self) -> Result<i64> {
        self.run_quiet(|c| {
            c.query_row("UPDATE generation SET n = n + 1 RETURNING n", [], |r| {
                r.get(0)
            })
        })
        .await
    }

    /// How many images an album holds.
    pub async fn count(&self, album: &str) -> Result<u64> {
        let album = album.to_string();
        self.run(move |c| {
            c.prepare_cached("SELECT COUNT(*) FROM images WHERE album = ?1")?
                .query_row([album], |r| r.get::<_, i64>(0))
                .map(|n| n as u64)
        })
        .await
    }

    /// How many images `owner` has and how many bytes their originals take.
    pub async fn usage(&self, owner: &str) -> Result<(u64, u64)> {
        let owner = owner.to_string();
        self.run(move |c| {
            c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(i.bytes), 0) FROM images i JOIN albums a ON a.id = i.album WHERE a.owner = ?1",
                [owner],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
        })
        .await
    }

    /// Images and albums in all, for the metrics.
    pub async fn totals(&self) -> Result<(u64, u64, u64)> {
        self.run(|c| {
            c.query_row(
                "SELECT (SELECT COUNT(*) FROM albums), (SELECT COUNT(*) FROM images), (SELECT COALESCE(SUM(bytes), 0) FROM images)",
                [],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64)),
            )
        })
        .await
    }

    pub async fn create_session(&self, id_hash: Vec<u8>, sub: &str, expires_at: i64) -> Result<()> {
        let sub = sub.to_string();
        self.run(move |c| {
            // Expired rows go on each sign-in; there are too few for a sweeper.
            c.execute("DELETE FROM sessions WHERE expires_at < ?1", [now()])?;
            c.execute(
                "INSERT INTO sessions (id_hash, sub, expires_at) VALUES (?1, ?2, ?3)",
                params![id_hash, sub, expires_at],
            )
            .map(drop)
        })
        .await
    }

    pub async fn session(&self, id_hash: Vec<u8>) -> Result<Option<String>> {
        self.run(move |c| {
            c.prepare_cached("SELECT sub FROM sessions WHERE id_hash = ?1 AND expires_at > ?2")?
                .query_row(params![id_hash, now()], |r| r.get(0))
                .optional()
        })
        .await
    }

    pub async fn delete_session(&self, id_hash: Vec<u8>) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM sessions WHERE id_hash = ?1", [id_hash])
                .map(drop)
        })
        .await
    }
}

fn images_by_id(c: &Connection, ids: &[String]) -> Result<Vec<Image>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(500) {
        let sql = format!(
            "SELECT * FROM images WHERE id IN ({})",
            placeholders(1, chunk.len())
        );
        let mut stmt = c.prepare(&sql)?;
        for image in stmt.query_map(rusqlite::params_from_iter(chunk), Image::from_row)? {
            out.push(image?);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn new_image(id: &str) -> NewImage {
        NewImage {
            id: id.to_string(),
            kind: Kind::Jpeg,
            width: 400,
            height: 300,
            bytes: 1234,
            animated: false,
            display: false,
            sha256: vec![1; 32],
        }
    }

    /// Finish an upload as the service does: its copies recorded first.
    pub async fn finish(db: &Db, owner: &str, upload: &str, image: &str) -> bool {
        db.add_stray(image).await.unwrap();
        db.finish_upload(owner, upload, new_image(image))
            .await
            .unwrap()
    }

    /// An album of `owner` holding `n` images, added through uploads as
    /// the service adds them.
    pub async fn album_with(db: &Db, owner: &str, n: usize) -> (Album, Vec<String>) {
        let album = db.create_album(owner, "trip").await.unwrap();
        let files = (0..n).map(|i| (format!("{i}.jpg"), 10)).collect();
        let uploads = db
            .begin_uploads(owner, &album.id, files)
            .await
            .unwrap()
            .unwrap();
        let mut ids = Vec::new();
        for u in uploads {
            let id = crate::id::new();
            assert!(finish(db, owner, &u.id, &id).await);
            ids.push(id);
        }
        (album, ids)
    }

    async fn order(db: &Db, album: &str) -> Vec<String> {
        db.images(album)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect()
    }

    #[tokio::test]
    async fn uploads_keep_the_order_they_were_picked_in_whatever_order_they_finish() {
        let db = Db::temp().await;
        let album = db.create_album("ann", "").await.unwrap();
        let files = vec![("a".into(), 1), ("b".into(), 1), ("c".into(), 1)];
        let uploads = db
            .begin_uploads("ann", &album.id, files)
            .await
            .unwrap()
            .unwrap();
        for (u, id) in uploads.iter().rev().zip(["C", "B", "A"]) {
            finish(&db, "ann", &u.id, id).await;
        }
        assert_eq!(order(&db, &album.id).await, ["A", "B", "C"]);
        let names: Vec<_> = db
            .images(&album.id)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn nobody_touches_an_album_they_do_not_own() {
        let db = Db::temp().await;
        let (album, ids) = album_with(&db, "ann", 2).await;
        let theirs = db.create_album("bob", "").await.unwrap();
        let edit = AlbumEdit {
            title: Some("mine now".into()),
            ..Default::default()
        };
        assert!(!db.edit_album("bob", &album.id, edit).await.unwrap());
        assert!(!db.set_caption("bob", &ids[0], "hi").await.unwrap());
        assert!(!db.reorder("bob", &album.id, ids.clone()).await.unwrap());
        assert!(!db.set_cover("bob", &album.id, Some(&ids[1])).await.unwrap());
        assert_eq!(
            db.move_images("bob", ids.clone(), &theirs.id)
                .await
                .unwrap(),
            0
        );
        assert!(
            db.delete_images("bob", ids.clone())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(db.delete_album("bob", &album.id).await.unwrap().is_none());
        assert!(
            db.begin_uploads("bob", &album.id, vec![("x".into(), 1)])
                .await
                .unwrap()
                .is_none()
        );
        // And not by moving their own images into it either.
        let (_, bobs) = album_with(&db, "bob", 1).await;
        assert_eq!(db.move_images("bob", bobs, &album.id).await.unwrap(), 0);

        assert_eq!(db.album(&album.id).await.unwrap().unwrap().title, "trip");
        assert_eq!(order(&db, &album.id).await, ids);
    }

    #[tokio::test]
    async fn reorder_puts_the_named_first_and_keeps_the_rest() {
        let db = Db::temp().await;
        let (album, ids) = album_with(&db, "ann", 4).await;
        let wanted = vec![
            ids[2].clone(),
            "not-there".into(),
            ids[0].clone(),
            ids[2].clone(),
        ];
        assert!(db.reorder("ann", &album.id, wanted).await.unwrap());
        assert_eq!(
            order(&db, &album.id).await,
            [
                ids[2].clone(),
                ids[0].clone(),
                ids[1].clone(),
                ids[3].clone()
            ]
        );
        // Images added after a reorder still go at the end.
        let uploads = db
            .begin_uploads("ann", &album.id, vec![("e".into(), 1)])
            .await
            .unwrap()
            .unwrap();
        finish(&db, "ann", &uploads[0].id, "EEEEEEEEEE").await;
        assert_eq!(order(&db, &album.id).await.last().unwrap(), "EEEEEEEEEE");
    }

    #[tokio::test]
    async fn moving_images_appends_them_and_clears_a_cover_they_left() {
        let db = Db::temp().await;
        let (from, ids) = album_with(&db, "ann", 3).await;
        let (to, theirs) = album_with(&db, "ann", 1).await;
        assert!(db.set_cover("ann", &from.id, Some(&ids[1])).await.unwrap());
        assert_eq!(
            db.move_images("ann", vec![ids[1].clone(), ids[0].clone()], &to.id)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            order(&db, &to.id).await,
            [theirs[0].clone(), ids[1].clone(), ids[0].clone()]
        );
        assert_eq!(order(&db, &from.id).await, [ids[2].clone()]);
        assert_eq!(db.album(&from.id).await.unwrap().unwrap().cover, None);
    }

    #[tokio::test]
    async fn deleting_an_album_takes_its_images_and_uploads() {
        let db = Db::temp().await;
        let (album, ids) = album_with(&db, "ann", 2).await;
        let pending = db
            .begin_uploads("ann", &album.id, vec![("p".into(), 1)])
            .await
            .unwrap()
            .unwrap();
        let mut gone = db.delete_album("ann", &album.id).await.unwrap().unwrap();
        gone.sort();
        let mut ids = ids;
        ids.sort();
        assert_eq!(gone, ids);
        assert!(db.image(&ids[0]).await.unwrap().is_none());
        assert!(db.upload(&pending[0].id).await.unwrap().is_none());
        // A file that finishes after its album went is not added anywhere.
        assert!(!finish(&db, "ann", &pending[0].id, "LATELATE00").await);
    }

    #[tokio::test]
    async fn an_ended_upload_takes_no_more_and_never_becomes_an_image() {
        let db = Db::temp().await;
        let album = db.create_album("ann", "").await.unwrap();
        let uploads = db
            .begin_uploads("ann", &album.id, vec![("a".into(), 10), ("b".into(), 10)])
            .await
            .unwrap()
            .unwrap();
        // Someone else's upload is not theirs to finish or fail.
        assert!(!finish(&db, "bob", &uploads[0].id, "BOBBOBBOB0").await);
        assert!(!db.set_received("bob", &uploads[0].id, 5).await.unwrap());
        db.fail_upload("bob", &uploads[0].id, "cancelled")
            .await
            .unwrap();
        assert_eq!(
            db.upload(&uploads[0].id).await.unwrap().unwrap().error,
            None
        );
        // Cancelled: no more chunks, and no image even if processing finishes.
        db.fail_upload("ann", &uploads[1].id, "cancelled")
            .await
            .unwrap();
        assert!(!db.set_received("ann", &uploads[1].id, 5).await.unwrap());
        assert!(!finish(&db, "ann", &uploads[1].id, "LATELATE01").await);
        assert!(db.images(&album.id).await.unwrap().is_empty());
        // The first still finishes, once.
        assert!(finish(&db, "ann", &uploads[0].id, "ANNANNANN0").await);
        assert!(!finish(&db, "ann", &uploads[0].id, "ANNANNANN1").await);
        assert_eq!(db.images(&album.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn upload_progress_is_not_announced() {
        let db = Db::temp().await;
        let album = db.create_album("ann", "").await.unwrap();
        let upload = db
            .begin_uploads("ann", &album.id, vec![("a".into(), 10)])
            .await
            .unwrap()
            .unwrap()
            .remove(0);
        let rx = db.writes();
        let before = *rx.borrow();
        assert!(db.set_received("ann", &upload.id, 5).await.unwrap());
        assert_eq!(*rx.borrow(), before);
    }

    #[tokio::test]
    async fn cards_show_counts_and_the_cover_or_the_first_image() {
        let db = Db::temp().await;
        let (album, ids) = album_with(&db, "ann", 3).await;
        db.create_album("ann", "empty").await.unwrap();
        db.create_album("bob", "not mine").await.unwrap();
        let cards = db.cards("ann").await.unwrap();
        assert_eq!(cards.len(), 2);
        let card = cards.iter().find(|c| c.album.id == album.id).unwrap();
        assert_eq!((card.count, card.bytes), (3, 3 * 1234));
        assert_eq!(card.cover.as_ref().unwrap().id, ids[0]);
        db.set_cover("ann", &album.id, Some(&ids[2])).await.unwrap();
        let cards = db.cards("ann").await.unwrap();
        let card = cards.iter().find(|c| c.album.id == album.id).unwrap();
        assert_eq!(card.cover.as_ref().unwrap().id, ids[2]);
        assert!(cards.iter().any(|c| c.count == 0 && c.cover.is_none()));
    }

    #[tokio::test]
    async fn writes_are_announced_and_reads_are_not() {
        let db = Db::temp().await;
        let rx = db.writes();
        let before = *rx.borrow();
        db.cards("ann").await.unwrap();
        assert_eq!(*rx.borrow(), before);
        db.create_album("ann", "").await.unwrap();
        assert!(*rx.borrow() > before);
    }

    #[tokio::test]
    async fn a_change_wakes_only_the_pages_of_whoever_owns_it() {
        let db = Db::temp().await;
        let mut rx = db.changes();
        let album = db.create_album("ann", "").await.unwrap();
        assert_eq!(&*rx.try_recv().unwrap(), "ann");
        // Signing in, and a refused edit, change nothing anyone's pages show.
        db.upsert_user("bob", "bob").await.unwrap();
        let edit = AlbumEdit {
            title: Some("mine".into()),
            ..Default::default()
        };
        assert!(!db.edit_album("bob", &album.id, edit).await.unwrap());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn deleted_images_stay_recorded_until_their_copies_are_gone() {
        let db = Db::temp().await;
        let (album, ids) = album_with(&db, "ann", 3).await;
        let gone = db.delete_images("ann", vec![ids[0].clone()]).await.unwrap();
        let rest = db.delete_album("ann", &album.id).await.unwrap().unwrap();
        // Too recent to claim; then everything deleted, and nothing else.
        assert!(db.claim_strays(now() - 3_600_000).await.unwrap().is_empty());
        let mut claimed = db.claim_strays(now() + 1).await.unwrap();
        claimed.sort();
        let mut all = [gone, rest].concat();
        all.sort();
        assert_eq!(claimed, all);
        // Claimed once: the next sweep finds nothing.
        assert!(db.claim_strays(now() + 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_sweep_and_a_finishing_upload_never_both_have_the_copies() {
        let db = Db::temp().await;
        let album = db.create_album("ann", "").await.unwrap();
        let uploads = db
            .begin_uploads("ann", &album.id, vec![("a".into(), 1), ("b".into(), 1)])
            .await
            .unwrap()
            .unwrap();
        // The sweep took this one's record first: the upload cannot claim
        // copies being deleted.
        db.add_stray("SLOWSLOW00").await.unwrap();
        assert_eq!(db.claim_strays(now() + 1).await.unwrap(), ["SLOWSLOW00"]);
        assert!(
            !db.finish_upload("ann", &uploads[0].id, new_image("SLOWSLOW00"))
                .await
                .unwrap()
        );
        // This one finished first: the sweep never claims an image's copies,
        // even should a record of them be left.
        assert!(finish(&db, "ann", &uploads[1].id, "FASTFAST00").await);
        db.add_stray("FASTFAST00").await.unwrap();
        assert!(db.claim_strays(now() + 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn each_copy_of_the_database_has_a_later_generation() {
        let db = Db::temp().await;
        assert_eq!(db.next_generation().await.unwrap(), 1);
        assert_eq!(db.next_generation().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn sessions_expire_and_end() {
        let db = Db::temp().await;
        db.create_session(vec![1], "ann", now() + 60_000)
            .await
            .unwrap();
        db.create_session(vec![2], "ann", now() - 1).await.unwrap();
        assert_eq!(db.session(vec![1]).await.unwrap().as_deref(), Some("ann"));
        assert_eq!(db.session(vec![2]).await.unwrap(), None);
        db.delete_session(vec![1]).await.unwrap();
        assert_eq!(db.session(vec![1]).await.unwrap(), None);
    }
}
