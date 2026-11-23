//! A cache on the serving machine's disk for a store elsewhere (a bucket, or
//! an S3 service on another machine): objects read or written recently, kept
//! whole up to a budget. A read of one costs no round trip, and still works
//! while the store is unreachable.
//!
//! Files are named by their keys under the cache's directory, written aside
//! first and renamed into place, so a file that is there is whole. The index
//! and the files change together under one lock, so a file is never removed
//! after something newer took its place: what is evicted is moved aside
//! under the lock and deleted after it. A fetch into the cache is marked
//! stale by any write, delete or rename of its key while it runs, and is
//! then thrown away. The least recently used go first, once the budget is
//! passed or the disk nears the cache's floor, which sits above the one
//! uploads keep so the cache never crowds them out; after a restart,
//! modification times stand in for use.
//!
//! Its methods that write touch the filesystem and block: callers on the
//! async runtime call them on its blocking threads. A read waits on the
//! index for a few milliseconds at most, and is a miss past that, so a long
//! eviction never holds up the requests reading it.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

pub struct Disk {
    dir: PathBuf,
    /// Bytes the cache may hold.
    budget: u64,
    /// The largest object it keeps.
    item_max: u64,
    /// The free space it leaves its disk.
    floor: u64,
    index: Mutex<Index>,
    /// Keys being fetched now, and whether the key changed since. Its lock
    /// is taken alone, or inside the index's.
    filling: Mutex<HashMap<String, bool>>,
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Entry>,
    /// Keys by when they were last used, oldest first.
    order: BTreeMap<u64, String>,
    total: u64,
    clock: u64,
}

struct Entry {
    size: u64,
    used: u64,
}

impl Index {
    /// `key`'s size, and when it was used now.
    fn touch(&mut self, key: &str) -> Option<(u64, u64)> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(key)?;
        self.order.remove(&entry.used);
        entry.used = clock;
        self.order.insert(clock, key.to_string());
        Some((entry.size, clock))
    }

    fn insert(&mut self, key: &str, size: u64) {
        self.remove(key);
        self.clock += 1;
        let used = self.clock;
        self.entries.insert(key.to_string(), Entry { size, used });
        self.order.insert(used, key.to_string());
        self.total += size;
    }

    fn remove(&mut self, key: &str) -> bool {
        let Some(entry) = self.entries.remove(key) else {
            return false;
        };
        self.order.remove(&entry.used);
        self.total -= entry.size;
        true
    }
}

/// Where files are written before they are whole, and moved to be deleted.
const ASIDE: &str = ".aside";

impl Disk {
    /// The cache in `dir`, holding up to `budget` bytes, each object at most
    /// a quarter of that, and leaving its disk `floor` bytes free. What is
    /// already there is taken up, oldest first; anything left part-written,
    /// or too large for the budget now, is removed.
    pub fn open(dir: &Path, budget: u64, floor: u64) -> io::Result<Self> {
        let aside = dir.join(ASIDE);
        std::fs::create_dir_all(&aside)?;
        if let Ok(entries) = std::fs::read_dir(&aside) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let disk = Self {
            dir: dir.to_path_buf(),
            budget,
            item_max: budget / 4,
            floor,
            index: Mutex::default(),
            filling: Mutex::default(),
        };
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(at) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let hidden = entry.file_name().to_string_lossy().starts_with('.');
                if meta.is_dir() {
                    if !hidden {
                        pending.push(path);
                    }
                } else if hidden || !disk.admits(meta.len()) {
                    let _ = std::fs::remove_file(&path);
                } else if let Ok(key) = path.strip_prefix(dir) {
                    let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    found.push((
                        modified,
                        key.to_string_lossy().replace('\\', "/"),
                        meta.len(),
                    ));
                }
            }
        }
        found.sort();
        let doomed = {
            let mut index = disk.index.lock();
            for (_, key, size) in found {
                index.insert(&key, size);
            }
            disk.evict(&mut index)
        };
        disk.delete(doomed);
        Ok(disk)
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// Whether an object of `size` bytes is kept.
    pub fn admits(&self, size: u64) -> bool {
        size <= self.item_max
    }

    /// The file holding `key`, its length, and a mark of this use for
    /// [`forget_if`](Self::forget_if), when it is here.
    pub fn get(&self, key: &str) -> Option<(PathBuf, u64, u64)> {
        let mut index = self
            .index
            .try_lock_for(std::time::Duration::from_millis(5))?;
        let (size, used) = index.touch(key)?;
        Some((self.path(key), size, used))
    }

    /// Whether `size` more bytes fit above the floor.
    pub fn has_room(&self, size: u64) -> bool {
        crate::store::disk(&self.dir)
            .is_none_or(|(free, _)| free >= self.floor.saturating_add(size))
    }

    /// `key`'s object changed: a fetch of it under way is stale.
    fn changed(&self, key: &str) {
        if let Some(stale) = self.filling.lock().get_mut(key) {
            *stale = true;
        }
    }

    /// Somewhere to write an object before it is whole.
    pub fn aside(&self) -> PathBuf {
        self.dir.join(ASIDE).join(crate::id::new())
    }

    /// Start fetching `key` into the cache: false when it is already being
    /// fetched.
    pub fn start(&self, key: &str) -> bool {
        let mut filling = self.filling.lock();
        if filling.contains_key(key) {
            return false;
        }
        filling.insert(key.to_string(), false);
        true
    }

    /// The fetch of `key` has ended, kept or not.
    pub fn done(&self, key: &str) {
        self.filling.lock().remove(key);
    }

    /// Take `written`, `size` bytes written aside, as `key`: `fetched` into
    /// the cache, kept only when nothing changed the key meanwhile, or
    /// written by the store itself, which makes any fetch of it stale.
    pub fn keep(&self, key: &str, written: &Path, size: u64, fetched: bool) -> io::Result<()> {
        let doomed = {
            let mut index = self.index.lock();
            let stale = fetched && self.filling.lock().get(key).copied().unwrap_or(true);
            if stale || !self.admits(size) {
                drop(index);
                let _ = std::fs::remove_file(written);
                return Ok(());
            }
            if !fetched {
                self.changed(key);
            }
            let to = self.path(key);
            let moved = to
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::rename(written, &to));
            if let Err(e) = moved {
                drop(index);
                let _ = std::fs::remove_file(written);
                return Err(e);
            }
            index.insert(key, size);
            self.evict(&mut index)
        };
        self.delete(doomed);
        Ok(())
    }

    /// `key` is no longer what the store holds.
    pub fn forget(&self, key: &str) {
        let mut index = self.index.lock();
        self.changed(key);
        if index.remove(key) {
            self.remove(&self.path(key));
        }
    }

    /// `key` could not be read as it was found at `used`: forgotten, unless
    /// it was used or replaced since, when it is not what failed.
    pub fn forget_if(&self, key: &str, used: u64) {
        let mut index = self.index.lock();
        if index.entries.get(key).is_some_and(|e| e.used == used) {
            index.remove(key);
            self.remove(&self.path(key));
        }
    }

    /// Give back what is needed for `needed` bytes more to fit above
    /// `min_free` (the uploads' floor), the least recently used first: an
    /// upload never waits on the cache.
    pub fn yield_room(&self, needed: u64, min_free: Option<u64>) {
        let mut index = self.index.lock();
        while crate::store::room(&self.dir, min_free).is_some_and(|room| room < needed) {
            let Some((_, oldest)) = index.order.pop_first() else {
                break;
            };
            if let Some(entry) = index.entries.remove(&oldest) {
                index.total -= entry.size;
            }
            // Removed now rather than moved aside: the room is wanted at once.
            self.remove(&self.path(&oldest));
        }
    }

    /// Remove the file at `path`, and the directories it leaves empty.
    fn remove(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
        let mut at = path.parent();
        while let Some(dir) = at {
            if dir == self.dir || std::fs::remove_dir(dir).is_err() {
                break;
            }
            at = dir.parent();
        }
    }

    /// The store moved `from` to `to`: what is here of it moves too.
    pub fn rename(&self, from: &str, to: &str) {
        let mut index = self.index.lock();
        self.changed(from);
        self.changed(to);
        if index.remove(to) {
            self.remove(&self.path(to));
        }
        let Some(size) = index.entries.get(from).map(|e| e.size) else {
            return;
        };
        index.remove(from);
        let target = self.path(to);
        let moved = target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(self.path(from), &target));
        match moved {
            Ok(()) => {
                index.insert(to, size);
                self.remove_dirs(&self.path(from));
            }
            Err(_) => self.remove(&self.path(from)),
        }
    }

    /// Remove the directories `path` leaves empty, now it has gone.
    fn remove_dirs(&self, path: &Path) {
        let mut at = path.parent();
        while let Some(dir) = at {
            if dir == self.dir || std::fs::remove_dir(dir).is_err() {
                break;
            }
            at = dir.parent();
        }
    }

    /// Take the least recently used out of the index, and move their files
    /// aside, until the budget holds and the disk is above the floor: what
    /// [`delete`](Self::delete) then removes, outside the lock.
    fn evict(&self, index: &mut Index) -> Vec<PathBuf> {
        let mut free = crate::store::disk(&self.dir).map(|(free, _)| free);
        let mut doomed = Vec::new();
        while index.total > self.budget || free.is_some_and(|f| f < self.floor) {
            let Some((_, oldest)) = index.order.pop_first() else {
                break;
            };
            let Some(entry) = index.entries.remove(&oldest) else {
                continue;
            };
            index.total -= entry.size;
            free = free.map(|f| f + entry.size);
            let away = self.aside();
            let path = self.path(&oldest);
            if std::fs::rename(&path, &away).is_ok() {
                doomed.push(away);
                self.remove_dirs(&path);
            }
        }
        doomed
    }

    fn delete(&self, doomed: Vec<PathBuf>) {
        for path in doomed {
            let _ = std::fs::remove_file(path);
        }
    }

    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.index.lock().total
    }

    #[cfg(test)]
    pub fn filling(&self) -> bool {
        !self.filling.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(disk: &Disk, key: &str, bytes: &[u8]) {
        let aside = disk.aside();
        std::fs::write(&aside, bytes).unwrap();
        disk.keep(key, &aside, bytes.len() as u64, false).unwrap();
    }

    fn open(dir: &Path, budget: u64) -> Disk {
        Disk::open(dir, budget, 0).unwrap()
    }

    #[test]
    fn the_least_recently_used_go_past_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let disk = open(dir.path(), 300);
        write(&disk, "o/aa/a", &[1; 70]);
        write(&disk, "o/bb/b", &[2; 70]);
        write(&disk, "o/cc/c", &[3; 70]);
        // a was used last, so b is the oldest when d needs room.
        assert!(disk.get("o/aa/a").is_some());
        write(&disk, "o/dd/d", &[4; 70]);
        write(&disk, "o/ee/e", &[5; 70]);
        assert!(disk.get("o/bb/b").is_none());
        assert!(disk.get("o/aa/a").is_some());
        assert_eq!(disk.total(), 280);
        assert_eq!(
            std::fs::read(disk.get("o/ee/e").unwrap().0).unwrap(),
            [5; 70]
        );
        assert!(!dir.path().join("o/bb/b").exists());
        assert_eq!(
            std::fs::read_dir(dir.path().join(ASIDE)).unwrap().count(),
            0
        );
    }

    #[test]
    fn what_is_too_large_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let disk = open(dir.path(), 400);
        write(&disk, "o/aa/a", &[1; 101]);
        assert!(disk.get("o/aa/a").is_none());
        assert_eq!(disk.total(), 0);
    }

    #[test]
    fn a_disk_near_its_floor_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path(), 1000, u64::MAX / 2).unwrap();
        write(&disk, "o/aa/a", &[1; 10]);
        assert_eq!(disk.total(), 0, "no room above the floor");
    }

    #[test]
    fn it_is_taken_up_again_and_moves_with_the_store() {
        let dir = tempfile::tempdir().unwrap();
        {
            let disk = open(dir.path(), 1000);
            write(&disk, "work/a/w/original", b"made");
            std::fs::write(disk.aside(), b"half").unwrap();
        }
        std::fs::write(dir.path().join(".DS_Store"), b"x").unwrap();
        let disk = open(dir.path(), 1000);
        assert_eq!(disk.total(), 4, "only whole objects are taken up");
        disk.rename("work/a/w/original", "o/aa/a");
        assert!(disk.get("work/a/w/original").is_none());
        assert!(!dir.path().join("work").exists(), "empty directories go");
        assert_eq!(
            std::fs::read(disk.get("o/aa/a").unwrap().0).unwrap(),
            b"made"
        );
        disk.forget("o/aa/a");
        assert!(disk.get("o/aa/a").is_none());
        assert_eq!(disk.total(), 0);
    }

    #[test]
    fn a_fetch_overtaken_by_a_change_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let disk = open(dir.path(), 1000);
        assert!(disk.start("o/aa/a"));
        assert!(!disk.start("o/aa/a"), "one fetch at a time");
        // The object is deleted while the fetch runs.
        disk.forget("o/aa/a");
        let aside = disk.aside();
        std::fs::write(&aside, b"old").unwrap();
        disk.keep("o/aa/a", &aside, 3, true).unwrap();
        disk.done("o/aa/a");
        assert!(disk.get("o/aa/a").is_none());
        assert!(!aside.exists());
        // A write while a fetch runs wins over it too.
        assert!(disk.start("o/aa/a"));
        write(&disk, "o/aa/a", b"new");
        let aside = disk.aside();
        std::fs::write(&aside, b"old").unwrap();
        disk.keep("o/aa/a", &aside, 3, true).unwrap();
        disk.done("o/aa/a");
        assert_eq!(
            std::fs::read(disk.get("o/aa/a").unwrap().0).unwrap(),
            b"new"
        );
        // A fetch nothing overtook is kept.
        assert!(disk.start("o/bb/b"));
        let aside = disk.aside();
        std::fs::write(&aside, b"b").unwrap();
        disk.keep("o/bb/b", &aside, 1, true).unwrap();
        disk.done("o/bb/b");
        assert!(disk.get("o/bb/b").is_some());
    }

    #[test]
    fn a_failed_read_does_not_drop_a_newer_copy() {
        let dir = tempfile::tempdir().unwrap();
        let disk = open(dir.path(), 1000);
        write(&disk, "o/aa/a", b"old");
        let (_, _, used) = disk.get("o/aa/a").unwrap();
        write(&disk, "o/aa/a", b"new");
        disk.forget_if("o/aa/a", used);
        assert!(disk.get("o/aa/a").is_some());
        let (_, _, used) = disk.get("o/aa/a").unwrap();
        disk.forget_if("o/aa/a", used);
        assert!(disk.get("o/aa/a").is_none());
    }
}
