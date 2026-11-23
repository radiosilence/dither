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
//! async runtime call them on its blocking threads.

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
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Entry>,
    /// Keys by when they were last used, oldest first.
    order: BTreeMap<u64, String>,
    total: u64,
    clock: u64,
    /// Keys being fetched now, and whether the key changed since.
    filling: HashMap<String, bool>,
}

struct Entry {
    size: u64,
    used: u64,
}

impl Index {
    fn touch(&mut self, key: &str) -> Option<u64> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(key)?;
        self.order.remove(&entry.used);
        entry.used = clock;
        self.order.insert(clock, key.to_string());
        Some(entry.size)
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

    /// `key`'s object changed: a fetch of it under way is stale.
    fn changed(&mut self, key: &str) {
        if let Some(stale) = self.filling.get_mut(key) {
            *stale = true;
        }
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

    /// The file holding `key`, and its length, when it is here.
    pub fn get(&self, key: &str) -> Option<(PathBuf, u64)> {
        let size = self.index.lock().touch(key)?;
        Some((self.path(key), size))
    }

    /// Somewhere to write an object before it is whole.
    pub fn aside(&self) -> PathBuf {
        self.dir.join(ASIDE).join(crate::id::new())
    }

    /// Start fetching `key` into the cache: false when it is already being
    /// fetched.
    pub fn start(&self, key: &str) -> bool {
        let mut index = self.index.lock();
        if index.filling.contains_key(key) {
            return false;
        }
        index.filling.insert(key.to_string(), false);
        true
    }

    /// The fetch of `key` has ended, kept or not.
    pub fn done(&self, key: &str) {
        self.index.lock().filling.remove(key);
    }

    /// Take `written`, `size` bytes written aside, as `key`: `fetched` into
    /// the cache, kept only when nothing changed the key meanwhile, or
    /// written by the store itself, which makes any fetch of it stale.
    pub fn keep(&self, key: &str, written: &Path, size: u64, fetched: bool) -> io::Result<()> {
        let doomed = {
            let mut index = self.index.lock();
            let stale = fetched && index.filling.get(key).copied().unwrap_or(true);
            if stale || !self.admits(size) {
                drop(index);
                let _ = std::fs::remove_file(written);
                return Ok(());
            }
            if !fetched {
                index.changed(key);
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
        index.changed(key);
        if index.remove(key) {
            let _ = std::fs::remove_file(self.path(key));
        }
    }

    /// The store moved `from` to `to`: what is here of it moves too.
    pub fn rename(&self, from: &str, to: &str) {
        let mut index = self.index.lock();
        index.changed(from);
        index.changed(to);
        if index.remove(to) {
            let _ = std::fs::remove_file(self.path(to));
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
            Ok(()) => index.insert(to, size),
            Err(_) => {
                let _ = std::fs::remove_file(self.path(from));
            }
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
            if std::fs::rename(self.path(&oldest), &away).is_ok() {
                doomed.push(away);
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
        !self.index.lock().filling.is_empty()
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
}
