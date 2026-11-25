//! SQLite: accounts and sessions (koan-auth's tables), albums, images and
//! uploads in progress.
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
use std::time::Duration;

use koan_auth::Role;
use koan_auth::accounts::{self, AccountError};
use koan_auth::users::UserRow;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use tokio::sync::{broadcast, watch};

use crate::sanitize::Kind;

pub use rusqlite::Result;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_processing.sql"),
    include_str!("../migrations/0003_as_recorded.sql"),
    include_str!("../migrations/0004_sessions.sql"),
    include_str!("../migrations/0005_views.sql"),
    include_str!("../migrations/0006_accounts.sql"),
];

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
            // After dither's own, the last of which drops the tables
            // these replace.
            koan_auth::schema::migrate(&conn)?;
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
    ///
    /// It has the accounts tests act as: `ann`, `bob` and `cat`, and `dev`,
    /// an admin.
    #[cfg(test)]
    pub async fn temp() -> Self {
        let dir = tempfile::tempdir().expect("temp dir").keep();
        let db = Self::open(&dir.join("dither.db"))
            .await
            .expect("test database");
        db.test_accounts().await;
        db
    }

    /// The accounts tests act as, made once.
    #[cfg(test)]
    pub async fn test_accounts(&self) {
        self.run(|c| {
            for (name, role) in [
                ("dev", Role::Admin),
                ("ann", Role::User),
                ("bob", Role::User),
                ("cat", Role::User),
            ] {
                if koan_auth::users::get_user_by_username(c, name)?.is_none() {
                    koan_auth::users::create_user(c, name, "a test password", role)?;
                }
            }
            Ok(())
        })
        .await
        .expect("test accounts")
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
    /// A video's length; `None` for a picture.
    pub duration_ms: Option<u64>,
    /// A video with sound.
    pub audio: bool,
    pub status: Status,
    /// Why its processing failed: a key the pages word (`jobs::FAILED_*`).
    pub failure: String,
    /// How many times a worker has claimed it.
    pub attempts: u32,
    /// A video kept as recorded, rather than converted to H.264.
    pub as_recorded: bool,
}

/// Whether an image's copies are there to serve. A picture is ready when its
/// row is added; a video is processing until its copies are made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ready,
    Processing,
    Failed,
}

impl Status {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ready" => Status::Ready,
            "processing" => Status::Processing,
            "failed" => Status::Failed,
            _ => return None,
        })
    }
}

impl Image {
    pub fn is_video(&self) -> bool {
        self.kind.is_video()
    }

    pub fn ready(&self) -> bool {
        self.status == Status::Ready
    }

    /// How long a video runs, in seconds.
    pub fn seconds(&self) -> f64 {
        self.duration_ms.unwrap_or_default() as f64 / 1000.0
    }

    fn from_row(r: &Row) -> Result<Self> {
        let kind: String = r.get("kind")?;
        let status: String = r.get("state")?;
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
            duration_ms: r.get::<_, Option<i64>>("duration_ms")?.map(|d| d as u64),
            audio: r.get("audio")?,
            status: Status::parse(&status).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    format!("unknown state {status}").into(),
                )
            })?,
            failure: r.get("failure")?,
            attempts: r.get("attempts")?,
            as_recorded: r.get("as_recorded")?,
        })
    }

    /// The entity tag every copy of it is served under, with the variant's
    /// letter so the copies differ.
    pub fn etag(&self, variant: char) -> String {
        let hex = crate::hex::encode(&self.sha256[..self.sha256.len().min(12)]);
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
    /// Its videos are kept as recorded rather than converted.
    pub as_recorded: bool,
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
            as_recorded: r.get("as_recorded")?,
        })
    }
}

/// An upload that finished and waits to be processed: what is known of it
/// before a worker reads it whole.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub id: String,
    pub kind: Kind,
    pub bytes: u64,
}

/// What a worker made of a job: what its row records once it is ready.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Made {
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub animated: bool,
    pub display: bool,
    pub sha256: Vec<u8>,
    pub duration_ms: Option<u64>,
    pub audio: bool,
}

/// A job a worker has claimed: the image to make and whose it is.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub image: Image,
    pub owner: String,
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

/// Forget the view counts of images or albums `ids`.
fn forget_views(tx: &rusqlite::Transaction, ids: &[String]) -> Result<()> {
    for table in ["views", "visitors", "referrers", "fetchers"] {
        let mut delete = tx.prepare_cached(&format!("DELETE FROM {table} WHERE target = ?1"))?;
        for id in ids {
            delete.execute([id])?;
        }
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
                       COALESCE(a.cover, (SELECT id FROM images WHERE album = a.id AND state = 'ready' ORDER BY position, created_at LIMIT 1)) AS cover_id \
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
            forget_views(tx, &images)?;
            forget_views(tx, std::slice::from_ref(&id))?;
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
                 AND (?1 IS NULL OR EXISTS (SELECT 1 FROM images WHERE id = ?1 AND album = ?3 AND state = 'ready'))",
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
            forget_views(tx, &deleted)?;
            Ok(deleted)
        })
        .await
    }

    /// One upload per file, at the end of `album` in the order given, or
    /// `None` when `owner` does not own it. `as_recorded` keeps its videos as
    /// they were recorded rather than converting them.
    pub async fn begin_uploads(
        &self,
        owner: &str,
        album: &str,
        files: Vec<(String, u64)>,
        as_recorded: bool,
    ) -> Result<Option<Vec<Upload>>> {
        let (o, album) = (owner.to_string(), album.to_string());
        self.write_as(owner, move |tx| {
            let Some(start) = reserve(tx, &o, &album, files.len())? else {
                return Ok(None);
            };
            let created_at = now();
            let mut uploads = Vec::with_capacity(files.len());
            let mut insert = tx.prepare(
                "INSERT INTO uploads (id, owner, album, position, name, size, created_at, as_recorded) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
                    as_recorded,
                };
                insert.execute(params![
                    upload.id,
                    upload.owner,
                    upload.album,
                    upload.position,
                    upload.name,
                    upload.size as i64,
                    upload.created_at,
                    as_recorded
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

    /// The upload's image, added as processing where its upload reserved:
    /// the job a worker takes up. False when the upload is gone (its album
    /// was deleted while the file was on its way), is not `owner`'s, or
    /// ended another way first (it was cancelled).
    pub async fn begin_job(&self, owner: &str, upload: &str, job: NewJob) -> Result<bool> {
        let (o, upload) = (owner.to_string(), upload.to_string());
        self.write_as(owner, move |tx| {
            let Some((album, position, name, as_recorded)) = tx
                .query_row(
                    "SELECT album, position, name, as_recorded FROM uploads \
                     WHERE id = ?1 AND owner = ?2 AND image IS NULL AND error IS NULL",
                    params![upload, o],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, bool>(3)?,
                        ))
                    },
                )
                .optional()?
            else {
                return Ok(false);
            };
            tx.execute(
                "INSERT INTO images (id, album, position, kind, width, height, bytes, sha256, name, created_at, state, as_recorded) \
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, X'', ?6, ?7, 'processing', ?8)",
                params![
                    job.id,
                    album,
                    position,
                    job.kind.as_str(),
                    job.bytes as i64,
                    name,
                    now(),
                    as_recorded,
                ],
            )?;
            tx.execute(
                "UPDATE uploads SET image = ?1 WHERE id = ?2 AND owner = ?3",
                params![job.id, upload, o],
            )?;
            tx.execute(
                "UPDATE albums SET updated_at = ?1 WHERE id = ?2 AND owner = ?3",
                params![now(), album, o],
            )?;
            Ok(true)
        })
        .await
    }

    /// Claim the oldest job no worker holds, of videos or of images, for
    /// `worker` until `until`, other than those it is `making`: one it held
    /// and lost track of (its end never heard) it takes up again once the
    /// lease lapses. Quiet: a claim changes nothing anyone sees.
    pub async fn claim(
        &self,
        worker: &str,
        video: bool,
        until: i64,
        making: &[String],
    ) -> Result<Option<Claimed>> {
        let worker = worker.to_string();
        let making = serde_json::to_string(making).expect("serialisable");
        self.run_quiet(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let image = tx
                .query_row(
                    "UPDATE images SET lease_until = ?1, claimed_by = ?2, attempts = attempts + 1 \
                     WHERE id = (SELECT id FROM images WHERE state = 'processing' \
                       AND (lease_until IS NULL OR lease_until < ?3) AND (kind = 'mp4') = ?4 \
                       AND id NOT IN (SELECT value FROM json_each(?5)) \
                       ORDER BY created_at LIMIT 1) \
                     RETURNING *",
                    params![until, worker, now(), video, making],
                    Image::from_row,
                )
                .optional()?;
            let claimed = match image {
                Some(image) => {
                    // Copies a worker stores for it are found and removed
                    // should the job go before it is made.
                    tx.execute(
                        "INSERT OR REPLACE INTO strays (id, since) VALUES (?1, ?2)",
                        params![image.id, now()],
                    )?;
                    let owner = tx.query_row(
                        "SELECT owner FROM albums WHERE id = ?1",
                        [&image.album],
                        |r| r.get(0),
                    )?;
                    Some(Claimed { image, owner })
                }
                None => None,
            };
            tx.commit()?;
            Ok(claimed)
        })
        .await
    }

    /// Keep `worker`'s claim on job `id` until `until`. False when it no
    /// longer holds it: the job was deleted, or its lease lapsed and another
    /// took it.
    pub async fn renew(&self, worker: &str, id: &str, until: i64) -> Result<bool> {
        let (worker, id) = (worker.to_string(), id.to_string());
        self.run_quiet(move |c| {
            c.execute(
                "UPDATE images SET lease_until = ?1 \
                 WHERE id = ?2 AND claimed_by = ?3 AND state = 'processing'",
                params![until, id, worker],
            )
            .map(|n| n == 1)
        })
        .await
    }

    /// Give job `id` back, for any worker to take after `after`. False when
    /// `worker` did not hold it.
    pub async fn release(&self, worker: &str, id: &str, after: i64) -> Result<bool> {
        let (worker, id) = (worker.to_string(), id.to_string());
        self.run_quiet(move |c| {
            c.execute(
                "UPDATE images SET lease_until = ?1, claimed_by = NULL \
                 WHERE id = ?2 AND claimed_by = ?3 AND state = 'processing'",
                params![after, id, worker],
            )
            .map(|n| n == 1)
        })
        .await
    }

    /// Record the original `worker` stored for job `id`: its length and
    /// SHA-256, which the row keeps once the job is made. False when `worker`
    /// does not hold the job.
    pub async fn record_original(
        &self,
        worker: &str,
        id: &str,
        bytes: u64,
        sha256: [u8; 32],
    ) -> Result<bool> {
        let (worker, id) = (worker.to_string(), id.to_string());
        self.run_quiet(move |c| {
            c.execute(
                "UPDATE images SET bytes = ?1, sha256 = ?2 \
                 WHERE id = ?3 AND claimed_by = ?4 AND state = 'processing'",
                params![bytes as i64, sha256.to_vec(), id, worker],
            )
            .map(|n| n == 1)
        })
        .await
    }

    /// Give back every job `worker` holds, refunding their attempts: a worker
    /// stopping.
    pub async fn release_all(&self, worker: &str) -> Result<()> {
        let worker = worker.to_string();
        self.run_quiet(move |c| {
            c.execute(
                "UPDATE images SET lease_until = 0, claimed_by = NULL, attempts = MAX(0, attempts - 1) \
                 WHERE claimed_by = ?1 AND state = 'processing'",
                [worker],
            )
            .map(drop)
        })
        .await
    }

    /// Mark job `id`, which `worker` holds, ready with what its copies came
    /// to, claiming the copies [`add_stray`](Self::add_stray) recorded.
    /// False when `worker` no longer holds it: deleted while its copies were
    /// made, or its lease lapsed and another took it.
    pub async fn finish_job(
        &self,
        owner: &str,
        worker: &str,
        id: &str,
        made: Made,
    ) -> Result<bool> {
        let (o, worker, id) = (owner.to_string(), worker.to_string(), id.to_string());
        self.write_as(owner, move |tx| {
            let n = tx.execute(
                "UPDATE images SET state = 'ready', width = ?1, height = ?2, bytes = ?3, animated = ?4, \
                   display = ?5, sha256 = ?6, duration_ms = ?7, audio = ?8, lease_until = NULL, claimed_by = NULL \
                 WHERE id = ?9 AND state = 'processing' AND claimed_by = ?11 \
                   AND album IN (SELECT id FROM albums WHERE owner = ?10)",
                params![
                    made.width,
                    made.height,
                    made.bytes as i64,
                    made.animated,
                    made.display,
                    made.sha256,
                    made.duration_ms.map(|d| d as i64),
                    made.audio,
                    id,
                    o,
                    worker
                ],
            )?;
            if n == 0 {
                return Ok(false);
            }
            tx.execute("DELETE FROM strays WHERE id = ?1", [&id])?;
            Ok(true)
        })
        .await
    }

    /// Mark job `id`, which `worker` holds, failed, with a key for why
    /// (`jobs::FAILED`, …). False when `worker` no longer holds it.
    pub async fn fail_job(&self, owner: &str, worker: &str, id: &str, why: &str) -> Result<bool> {
        let (o, worker, id, why) = (
            owner.to_string(),
            worker.to_string(),
            id.to_string(),
            why.to_string(),
        );
        self.run_as(owner, move |c| {
            c.execute(
                "UPDATE images SET state = 'failed', failure = ?1, bytes = 0, lease_until = NULL, claimed_by = NULL \
                 WHERE id = ?2 AND state = 'processing' AND claimed_by = ?4 \
                   AND album IN (SELECT id FROM albums WHERE owner = ?3)",
                params![why, id, o, worker],
            )
            .map(|n| n == 1)
        })
        .await
    }

    /// Record that a finished upload's image was refused once made: the
    /// upload ends with why, naming no image.
    pub async fn refuse_upload(&self, owner: &str, id: &str, error: &str) -> Result<()> {
        let (owner, id, error) = (owner.to_string(), id.to_string(), error.to_string());
        self.run(move |c| {
            c.execute(
                "UPDATE uploads SET image = NULL, error = ?1 WHERE id = ?2 AND owner = ?3",
                params![error, id, owner],
            )
            .map(drop)
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

    /// How many images an album shows to anyone with its link.
    pub async fn count(&self, album: &str) -> Result<u64> {
        let album = album.to_string();
        self.run(move |c| {
            c.prepare_cached("SELECT COUNT(*) FROM images WHERE album = ?1 AND state = 'ready'")?
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

    /// Tell the snapshotter the database changed, for writes made quietly.
    pub fn announce(&self) {
        self.0.writes.send_modify(|n| *n += 1);
    }

    /// Add counts gathered in memory, in one transaction, quietly: the
    /// caller decides when the snapshotter hears of them
    /// ([`Self::announce`]). Counts for an image or album deleted meanwhile
    /// are dropped. `p` stays the caller's, to take back on any failure.
    pub async fn add_views(&self, p: Arc<parking_lot::Mutex<crate::views::Pending>>) -> Result<()> {
        // A target that still exists: counts never outlive what they count.
        const EXISTS: &str = "WHERE EXISTS (SELECT 1 FROM images WHERE id = ?1) \
             OR EXISTS (SELECT 1 FROM albums WHERE id = ?1)";
        let add = |c: &mut Connection, p: &crate::views::Pending| -> Result<()> {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            {
                let mut hits = tx.prepare(&format!(
                    "INSERT INTO views (target, day, kind, hits, views, bytes) \
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6 {EXISTS} \
                     ON CONFLICT (target, day, kind) DO UPDATE SET hits = hits + excluded.hits, \
                     views = views + excluded.views, bytes = bytes + excluded.bytes"
                ))?;
                for ((target, day, kind), t) in &p.hits {
                    hits.execute(params![
                        target,
                        day,
                        kind,
                        t.hits as i64,
                        t.views as i64,
                        t.bytes as i64
                    ])?;
                }
                let mut visitors = tx.prepare(&format!(
                    "INSERT INTO visitors (target, day, visitors) SELECT ?1, ?2, ?3 {EXISTS} \
                     ON CONFLICT (target, day) DO UPDATE SET visitors = visitors + excluded.visitors"
                ))?;
                for ((target, day), n) in &p.visitors {
                    visitors.execute(params![target, day, *n as i64])?;
                }
                for (table, column, rows) in [
                    (
                        "referrers",
                        "host",
                        p.referrers
                            .iter()
                            .map(|((t, d, h), n)| (t, *d, h.as_str(), *n))
                            .collect::<Vec<_>>(),
                    ),
                    (
                        "fetchers",
                        "fetcher",
                        p.fetchers
                            .iter()
                            .map(|((t, d, f), n)| (t, *d, *f, *n))
                            .collect(),
                    ),
                ] {
                    let mut add = tx.prepare(&format!(
                        "INSERT INTO {table} (target, day, {column}, hits) SELECT ?1, ?2, ?3, ?4 {EXISTS} \
                         ON CONFLICT (target, day, {column}) DO UPDATE SET hits = hits + excluded.hits"
                    ))?;
                    for (target, day, name, n) in rows {
                        add.execute(params![target, day, name, n as i64])?;
                    }
                }
            }
            tx.commit()
        };
        self.run_quiet(move |c| add(c, &p.lock())).await
    }

    /// An image's or album's counts: since day `since`, and in all. An
    /// album's are its page's and those of the images in it now; its
    /// visitors were counted across the album as they came.
    pub async fn view_stats(&self, target: &str, since: i64) -> Result<crate::views::Stats> {
        use crate::views::{Stats, Totals};
        const TARGETS: &str = "(SELECT ?1 UNION ALL SELECT id FROM images WHERE album = ?1)";
        let target = target.to_string();
        self.run(move |c| {
            let (views, bytes, recent_views, recent_bytes) = c.query_row(
                &format!("SELECT COALESCE(SUM(views), 0), COALESCE(SUM(bytes), 0), \
                 COALESCE(SUM(CASE WHEN day >= ?2 THEN views END), 0), \
                 COALESCE(SUM(CASE WHEN day >= ?2 THEN bytes END), 0) FROM views WHERE target IN {TARGETS}"),
                params![target, since],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
            )?;
            let (visitors, recent_visitors) = c.query_row(
                "SELECT COALESCE(SUM(visitors), 0), COALESCE(SUM(CASE WHEN day >= ?2 THEN visitors END), 0) \
                 FROM visitors WHERE target = ?1",
                params![target, since],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )?;
            let top = |sql: &str| -> Result<Vec<(String, u64)>> {
                c.prepare(sql)?
                    .query_map([&target], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
                    .collect()
            };
            Ok(Stats {
                recent: Totals {
                    views: recent_views as u64,
                    visitors: recent_visitors as u64,
                    bytes: recent_bytes as u64,
                },
                all: Totals {
                    views: views as u64,
                    visitors: visitors as u64,
                    bytes: bytes as u64,
                },
                shared: top(
                    &format!("SELECT fetcher, SUM(hits) FROM fetchers WHERE target IN {TARGETS} \
                     GROUP BY fetcher ORDER BY 2 DESC, 1"),
                )?,
                referrers: top(
                    &format!("SELECT host, SUM(hits) FROM referrers WHERE target IN {TARGETS} \
                     GROUP BY host ORDER BY 2 DESC, 1 LIMIT 5"),
                )?,
            })
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

/// An account, as the admins' page lists it, with what its albums hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub albums: u64,
    pub bytes: u64,
}

/// A new session for `user_id`, lasting `ttl`: the secret its cookie carries.
/// The table keeps only its SHA-256.
fn open_session(c: &Connection, user_id: i64, ttl: Duration) -> Result<String> {
    // Expired sessions go as new ones are made; there are too few for a sweeper.
    koan_auth::sessions::cleanup_expired_tokens(c)?;
    let secret = koan_auth::secret::random_token()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    let expires = now() / 1000 + ttl.as_secs() as i64;
    koan_auth::sessions::store_refresh_token(c, &secret, user_id, expires)?;
    Ok(secret)
}

/// The account `id`, or why there is none to change.
fn existing(c: &Connection, id: i64) -> std::result::Result<UserRow, AccountError> {
    koan_auth::users::get_user_by_id(c, id)?.ok_or_else(|| AccountError::NoSuchUser(id.to_string()))
}

/// Accounts and sessions, in koan-auth's tables. A session is stored as the
/// SHA-256 of the secret its cookie carries, so the table alone signs no one
/// in, and an account's sessions end with a new password or with the account.
impl Db {
    /// Run `f` in a transaction that takes the write lock up front, committed
    /// only if `f` succeeds, so an account's change and dither's own land
    /// together or not at all.
    async fn account_write<T, F>(
        &self,
        announce: Announce,
        f: F,
    ) -> std::result::Result<T, AccountError>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Transaction) -> std::result::Result<T, AccountError> + Send + 'static,
    {
        self.exec(announce, move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = f(&tx);
            if out.is_ok() {
                tx.commit()?;
            }
            Ok(out)
        })
        .await?
    }

    /// The account a session's secret belongs to, as it stands, while the
    /// session lasts.
    pub async fn holder(&self, secret: &str) -> Result<Option<UserRow>> {
        let secret = secret.to_string();
        self.run(move |c| koan_auth::sessions::holder(c, &secret))
            .await
    }

    pub async fn open_session(&self, user_id: i64, ttl: Duration) -> Result<String> {
        self.run(move |c| open_session(c, user_id, ttl)).await
    }

    pub async fn end_session(&self, secret: &str) -> Result<()> {
        let secret = secret.to_string();
        self.run(move |c| koan_auth::sessions::revoke_refresh_token(c, &secret).map(drop))
            .await
    }

    pub async fn account_named(&self, username: &str) -> Result<Option<UserRow>> {
        let username = username.to_string();
        self.run(move |c| koan_auth::users::get_user_by_username(c, &username))
            .await
    }

    /// Whether anyone can sign in yet: a server with no account waits for its
    /// first admin.
    pub async fn has_accounts(&self) -> Result<bool> {
        self.run(|c| koan_auth::users::has_users(c)).await
    }

    /// Every account, oldest first.
    pub async fn accounts(&self) -> Result<Vec<Account>> {
        self.run(|c| {
            c.prepare(
                "SELECT u.id, u.username, u.role, \
                   (SELECT COUNT(*) FROM albums a WHERE a.owner = u.username), \
                   (SELECT COALESCE(SUM(i.bytes), 0) FROM images i \
                      JOIN albums a ON a.id = i.album WHERE a.owner = u.username) \
                 FROM users u ORDER BY u.id",
            )?
            .query_map([], |r| {
                Ok(Account {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    role: r.get::<_, String>(2)?.parse().unwrap_or(Role::Readonly),
                    albums: r.get::<_, i64>(3)? as u64,
                    bytes: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect()
        })
        .await
    }

    /// Make the first admin, signed in. Refused once any account exists.
    pub async fn create_first_admin(
        &self,
        username: &str,
        password: &str,
        ttl: Duration,
    ) -> std::result::Result<(UserRow, String), AccountError> {
        let (username, password) = (username.to_string(), password.to_string());
        self.account_write(Announce::Writes, move |tx| {
            let id = accounts::create_first_admin(tx, &username, &password)?;
            let secret = open_session(tx, id, ttl)?;
            Ok((existing(tx, id)?, secret))
        })
        .await
    }

    /// Make an account whose password nobody is told: the invite sent for it
    /// is how its owner chooses one.
    pub async fn create_account(
        &self,
        username: &str,
        role: Role,
    ) -> std::result::Result<UserRow, AccountError> {
        let username = username.to_string();
        self.account_write(Announce::Writes, move |tx| {
            let made = accounts::create_account(tx, &username, role)?;
            existing(tx, made.id)
        })
        .await
    }

    /// An invite for the account `id`, signed with `private_pem`, good for a
    /// week or until the account's password changes.
    pub async fn invite(
        &self,
        id: i64,
        private_pem: Vec<u8>,
    ) -> std::result::Result<(UserRow, String), AccountError> {
        self.run(move |c| {
            Ok(existing(c, id).and_then(|user| {
                koan_auth::invite::mint_token(
                    c,
                    &private_pem,
                    id,
                    koan_auth::invite::TOKEN_TTL_SECS,
                )
                .map(|token| (user, token))
            }))
        })
        .await?
    }

    /// The account an invite names, while the invite still works.
    pub async fn invited(
        &self,
        token: &str,
        public_pem: Vec<u8>,
    ) -> std::result::Result<UserRow, AccountError> {
        let token = token.to_string();
        self.run(move |c| Ok(koan_auth::invite::invited(c, &public_pem, &token)))
            .await?
    }

    /// Give the account an invite names `password`, which spends the invite,
    /// and sign it in. One transaction: an invite opened twice at once sets
    /// one password.
    pub async fn join(
        &self,
        token: &str,
        public_pem: Vec<u8>,
        password: &str,
        ttl: Duration,
    ) -> std::result::Result<(UserRow, String), AccountError> {
        let (token, password) = (token.to_string(), password.to_string());
        self.account_write(Announce::Writes, move |tx| {
            let user = koan_auth::invite::invited(tx, &public_pem, &token)?;
            // A new password ends every session the account had.
            accounts::set_password(tx, &user.username, Some(&password))?;
            let secret = open_session(tx, user.id, ttl)?;
            Ok((existing(tx, user.id)?, secret))
        })
        .await
    }

    /// Give the account `id` a password nobody is told, which signs it out
    /// everywhere and withdraws its invites: its next invite sets one.
    pub async fn reset_password(&self, id: i64) -> std::result::Result<UserRow, AccountError> {
        self.account_write(Announce::Writes, move |tx| {
            let user = existing(tx, id)?;
            accounts::set_password(tx, &user.username, None)?;
            Ok(user)
        })
        .await
    }

    /// Change the account `id`'s role, refusing to demote the last admin.
    pub async fn set_role(
        &self,
        id: i64,
        role: Role,
    ) -> std::result::Result<UserRow, AccountError> {
        self.account_write(Announce::Writes, move |tx| {
            let user = existing(tx, id)?;
            accounts::set_role(tx, &user.username, role)?;
            Ok(user)
        })
        .await
    }

    /// Delete the account `id` and its albums, refusing to delete the last
    /// admin. Answers with its images, whose copies are recorded as strays
    /// until the store confirms them gone.
    pub async fn delete_account(&self, id: i64) -> std::result::Result<Vec<String>, AccountError> {
        let owner = self
            .run(move |c| koan_auth::users::get_user_by_id(c, id))
            .await?
            .ok_or_else(|| AccountError::NoSuchUser(id.to_string()))?
            .username;
        let o = owner.clone();
        self.account_write(Announce::Owner(owner.into()), move |tx| {
            let images = tx
                .prepare(
                    "SELECT i.id FROM images i JOIN albums a ON a.id = i.album WHERE a.owner = ?1",
                )?
                .query_map([&o], |r| r.get(0))?
                .collect::<Result<Vec<String>>>()?;
            let albums = tx
                .prepare("SELECT id FROM albums WHERE owner = ?1")?
                .query_map([&o], |r| r.get(0))?
                .collect::<Result<Vec<String>>>()?;
            tx.execute("DELETE FROM albums WHERE owner = ?1", [&o])?;
            strays(tx, &images)?;
            forget_views(tx, &images)?;
            forget_views(tx, &albums)?;
            accounts::delete_account(tx, &o)?;
            Ok(images)
        })
        .await
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Finish an upload as the service does: queued, its copies recorded,
    /// then made.
    pub async fn finish(db: &Db, owner: &str, upload: &str, image: &str) -> bool {
        let job = NewJob {
            id: image.to_string(),
            kind: Kind::Jpeg,
            bytes: 1234,
        };
        if !db.begin_job(owner, upload, job).await.unwrap() {
            return false;
        }
        db.add_stray(image).await.unwrap();
        let made = Made {
            width: 400,
            height: 300,
            bytes: 1234,
            sha256: vec![1; 32],
            ..Made::default()
        };
        let claimed = db
            .claim("w", false, now() + 60_000, &[])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.image.id, image);
        db.finish_job(owner, "w", image, made).await.unwrap()
    }

    /// An album of `owner` holding `n` images, added through uploads as
    /// the service adds them.
    pub async fn album_with(db: &Db, owner: &str, n: usize) -> (Album, Vec<String>) {
        let album = db.create_album(owner, "trip").await.unwrap();
        let files = (0..n).map(|i| (format!("{i}.jpg"), 10)).collect();
        let uploads = db
            .begin_uploads(owner, &album.id, files, false)
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
            .begin_uploads("ann", &album.id, files, false)
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
            db.begin_uploads("bob", &album.id, vec![("x".into(), 1)], false)
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
            .begin_uploads("ann", &album.id, vec![("e".into(), 1)], false)
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
            .begin_uploads("ann", &album.id, vec![("p".into(), 1)], false)
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
            .begin_uploads(
                "ann",
                &album.id,
                vec![("a".into(), 10), ("b".into(), 10)],
                false,
            )
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
            .begin_uploads("ann", &album.id, vec![("a".into(), 10)], false)
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
        // A refused edit changes nothing anyone's pages show.
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
    async fn a_jobs_copies_are_never_swept_while_it_is_made() {
        let db = Db::temp().await;
        let album = db.create_album("ann", "").await.unwrap();
        let uploads = db
            .begin_uploads("ann", &album.id, vec![("a".into(), 1)], false)
            .await
            .unwrap()
            .unwrap();
        let job = NewJob {
            id: "MAKING0000".into(),
            kind: Kind::Jpeg,
            bytes: 1,
        };
        assert!(db.begin_job("ann", &uploads[0].id, job).await.unwrap());
        // Recorded before they are written, and the row stands: the sweep
        // leaves them.
        db.add_stray("MAKING0000").await.unwrap();
        assert!(db.claim_strays(now() + 1).await.unwrap().is_empty());
        // Deleted meanwhile: the worker's finish fails and the sweep has them.
        db.delete_images("ann", vec!["MAKING0000".into()])
            .await
            .unwrap();
        assert!(
            !db.finish_job("ann", "w", "MAKING0000", Made::default())
                .await
                .unwrap()
        );
        assert_eq!(db.claim_strays(now() + 1).await.unwrap(), ["MAKING0000"]);
    }

    /// An album of ann's with a job of each kind queued.
    pub(crate) async fn queued(db: &Db) -> (String, String) {
        let album = db.create_album("ann", "").await.unwrap();
        let uploads = db
            .begin_uploads(
                "ann",
                &album.id,
                vec![("a.jpg".into(), 1), ("b.mp4".into(), 1)],
                false,
            )
            .await
            .unwrap()
            .unwrap();
        for (upload, id, kind) in [
            (&uploads[0], "PICTURE000", Kind::Jpeg),
            (&uploads[1], "VIDEO00000", Kind::Mp4),
        ] {
            let job = NewJob {
                id: id.into(),
                kind,
                bytes: 1,
            };
            assert!(db.begin_job("ann", &upload.id, job).await.unwrap());
        }
        ("PICTURE000".into(), "VIDEO00000".into())
    }

    #[tokio::test]
    async fn a_job_is_claimed_once_until_its_lease_lapses() {
        let db = Db::temp().await;
        let (picture, video) = queued(&db).await;
        let later = now() + 60_000;
        let claimed = db.claim("w1", false, later, &[]).await.unwrap().unwrap();
        assert_eq!(
            (claimed.image.id.as_str(), claimed.owner.as_str()),
            (picture.as_str(), "ann")
        );
        assert_eq!(claimed.image.status, Status::Processing);
        // Pictures and videos are claimed apart, and a held one is not offered.
        assert!(db.claim("w2", false, later, &[]).await.unwrap().is_none());
        assert_eq!(
            db.claim("w2", true, later, &[])
                .await
                .unwrap()
                .unwrap()
                .image
                .id,
            video
        );
        assert!(db.renew("w1", &picture, later).await.unwrap());
        assert!(
            !db.renew("w2", &picture, later).await.unwrap(),
            "only its holder renews it"
        );
        // A lapsed lease is anyone's.
        assert!(db.renew("w1", &picture, now() - 1).await.unwrap());
        let again = db.claim("w3", false, later, &[]).await.unwrap().unwrap();
        assert_eq!(
            (again.image.id.as_str(), again.image.status),
            (picture.as_str(), Status::Processing)
        );
        assert!(
            !db.renew("w1", &picture, later).await.unwrap(),
            "w1 lost it"
        );
        // Put back to wait a while, then taken up again.
        db.release("w3", &picture, now() + 60_000).await.unwrap();
        assert!(db.claim("w4", false, later, &[]).await.unwrap().is_none());
        db.release("w3", &picture, 0).await.unwrap();
        assert!(
            db.claim("w4", false, later, &[]).await.unwrap().is_none(),
            "w3 no longer held it"
        );
        // A worker takes up a lapsed job of its own it is not making, as
        // when its end was never heard, but not one it is.
        assert!(db.renew("w2", &video, now() - 1).await.unwrap());
        assert!(
            db.claim("w2", true, later, std::slice::from_ref(&video))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.claim("w2", true, later, &[])
                .await
                .unwrap()
                .unwrap()
                .image
                .id,
            video
        );
    }

    #[tokio::test]
    async fn a_made_job_is_ready_and_a_failed_one_says_why() {
        let db = Db::temp().await;
        let (picture, video) = queued(&db).await;
        let made = Made {
            width: 640,
            height: 360,
            bytes: 99,
            sha256: vec![7; 32],
            duration_ms: Some(4200),
            audio: true,
            display: true,
            ..Made::default()
        };
        db.claim("w", true, now() + 60_000, &[])
            .await
            .unwrap()
            .unwrap();
        db.claim("w", false, now() + 60_000, &[])
            .await
            .unwrap()
            .unwrap();
        assert!(
            !db.finish_job("ann", "other", &video, made.clone())
                .await
                .unwrap(),
            "only its holder finishes it"
        );
        assert!(db.finish_job("ann", "w", &video, made).await.unwrap());
        let (v, _) = db.image(&video).await.unwrap().unwrap();
        assert_eq!(
            (v.status, v.width, v.duration_ms, v.audio, v.display),
            (Status::Ready, 640, Some(4200), true, true)
        );
        assert!(
            !db.finish_job("bob", "w", &picture, Made::default())
                .await
                .unwrap(),
            "not bob's"
        );
        assert!(
            !db.fail_job("ann", "other", &picture, "unreadable")
                .await
                .unwrap()
        );
        assert!(
            db.fail_job("ann", "w", &picture, "unreadable")
                .await
                .unwrap()
        );
        let (p, _) = db.image(&picture).await.unwrap().unwrap();
        assert_eq!(
            (p.status, p.failure.as_str()),
            (Status::Failed, "unreadable")
        );
        assert!(
            db.claim("w", false, now() + 1000, &[])
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn each_copy_of_the_database_has_a_later_generation() {
        let db = Db::temp().await;
        assert_eq!(db.next_generation().await.unwrap(), 1);
        assert_eq!(db.next_generation().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn sessions_last_until_they_end() {
        let db = Db::temp().await;
        let ann = db.account_named("ann").await.unwrap().unwrap();
        let secret = db
            .open_session(ann.id, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(db.holder(&secret).await.unwrap().unwrap().username, "ann");
        assert!(db.holder("not a session").await.unwrap().is_none());
        db.end_session(&secret).await.unwrap();
        assert!(db.holder(&secret).await.unwrap().is_none());
        let lapsed = db.open_session(ann.id, Duration::ZERO).await.unwrap();
        assert!(db.holder(&lapsed).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_invite_sets_a_password_once_and_signs_in() {
        let db = Db::temp().await;
        let (private, public) = koan_auth::keys::generate_pem().unwrap();
        let eve = db.create_account("eve", Role::User).await.unwrap();
        let (_, token) = db.invite(eve.id, private.into_bytes()).await.unwrap();
        let public = public.into_bytes();
        assert_eq!(
            db.invited(&token, public.clone()).await.unwrap().username,
            "eve"
        );
        let minute = Duration::from_secs(60);
        assert!(matches!(
            db.join(&token, public.clone(), "short", minute).await,
            Err(AccountError::ShortPassword)
        ));
        let (user, secret) = db
            .join(&token, public.clone(), "a long password", minute)
            .await
            .unwrap();
        assert_eq!(user.username, "eve");
        assert_eq!(db.holder(&secret).await.unwrap().unwrap().id, eve.id);
        // Spent: the password it set changed what it was made against.
        assert!(matches!(
            db.join(&token, public, "another password", minute).await,
            Err(AccountError::BadInvite)
        ));
    }

    #[tokio::test]
    async fn a_reset_signs_out_and_withdraws_invites() {
        let db = Db::temp().await;
        let (private, public) = koan_auth::keys::generate_pem().unwrap();
        let ann = db.account_named("ann").await.unwrap().unwrap();
        let secret = db
            .open_session(ann.id, Duration::from_secs(60))
            .await
            .unwrap();
        let (_, token) = db.invite(ann.id, private.into_bytes()).await.unwrap();
        db.reset_password(ann.id).await.unwrap();
        assert!(db.holder(&secret).await.unwrap().is_none());
        assert!(matches!(
            db.invited(&token, public.into_bytes()).await,
            Err(AccountError::BadInvite)
        ));
    }

    #[tokio::test]
    async fn an_account_goes_with_its_albums_and_their_copies() {
        let db = Db::temp().await;
        let bob = db.account_named("bob").await.unwrap().unwrap();
        let album = db.create_album("bob", "trip").await.unwrap();
        db.create_album("ann", "kept").await.unwrap();
        let secret = db
            .open_session(bob.id, Duration::from_secs(60))
            .await
            .unwrap();
        db.delete_account(bob.id).await.unwrap();
        assert!(db.album(&album.id).await.unwrap().is_none());
        assert!(db.holder(&secret).await.unwrap().is_none());
        assert!(db.account_named("bob").await.unwrap().is_none());
        assert_eq!(db.cards("ann").await.unwrap().len(), 1);
        // The last admin stays, albums and all.
        let dev = db.account_named("dev").await.unwrap().unwrap();
        assert!(matches!(
            db.delete_account(dev.id).await,
            Err(AccountError::LastAdmin)
        ));
        let names: Vec<String> = db
            .accounts()
            .await
            .unwrap()
            .into_iter()
            .map(|a| a.username)
            .collect();
        assert_eq!(names, ["dev", "ann", "cat"]);
    }

    #[tokio::test]
    async fn an_album_names_an_account_that_exists() {
        let db = Db::temp().await;
        assert!(db.create_album("nobody", "trip").await.is_err());
    }
}
