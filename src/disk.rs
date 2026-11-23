//! A cache on the serving machine's disk for a store elsewhere (a bucket, or
//! an S3 service on another machine): objects read or written recently, kept
//! whole up to a budget. A read of one costs no round trip, and still works
//! while the store is unreachable.
//!
//! Files are named by their keys under the cache's directory, written aside
//! first and renamed into place, so a file that is there is whole. Which were
//! used last is kept in memory, and the least recently used go once the
//! budget is passed; after a restart, their modification times stand in.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

pub struct Disk {
    dir: PathBuf,
    /// Bytes the cache may hold.
    budget: u64,
    /// The largest object it keeps.
    item_max: u64,
    index: Mutex<Index>,
    /// Keys being fetched into it now.
    filling: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Entry>,
    total: u64,
    /// Counts uses, so the least recently used has the lowest.
    clock: u64,
}

struct Entry {
    size: u64,
    used: u64,
}

/// Where files are written before they are whole.
const ASIDE: &str = ".aside";

impl Disk {
    /// The cache in `dir`, holding up to `budget` bytes, each object at most
    /// a quarter of that. What is already there is taken up, oldest first,
    /// and anything left part-written is removed.
    pub fn open(dir: &Path, budget: u64) -> io::Result<Self> {
        std::fs::create_dir_all(dir.join(ASIDE))?;
        for entry in std::fs::read_dir(dir.join(ASIDE))? {
            let _ = std::fs::remove_file(entry?.path());
        }
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(at) = pending.pop() {
            for entry in std::fs::read_dir(&at)? {
                let entry = entry?;
                let path = entry.path();
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    if path != dir.join(ASIDE) {
                        pending.push(path);
                    }
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
        let mut index = Index::default();
        for (_, key, size) in found {
            index.clock += 1;
            index.total += size;
            let used = index.clock;
            index.entries.insert(key, Entry { size, used });
        }
        let disk = Self {
            dir: dir.to_path_buf(),
            budget,
            item_max: budget / 4,
            index: Mutex::new(index),
            filling: Mutex::default(),
        };
        disk.evict();
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
        let mut index = self.index.lock();
        index.clock += 1;
        let clock = index.clock;
        let entry = index.entries.get_mut(key)?;
        entry.used = clock;
        Some((self.path(key), entry.size))
    }

    /// Somewhere to write an object before it is whole.
    pub fn aside(&self) -> PathBuf {
        self.dir.join(ASIDE).join(crate::id::new())
    }

    /// Take `written`, `size` bytes written aside, as `key`.
    pub fn keep(&self, key: &str, written: &Path, size: u64) -> io::Result<()> {
        if !self.admits(size) {
            let _ = std::fs::remove_file(written);
            return Ok(());
        }
        let to = self.path(key);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(written, &to)?;
        {
            let mut index = self.index.lock();
            index.clock += 1;
            let used = index.clock;
            if let Some(old) = index.entries.insert(key.to_string(), Entry { size, used }) {
                index.total -= old.size;
            }
            index.total += size;
        }
        self.evict();
        Ok(())
    }

    /// `key` is no longer what the store holds.
    pub fn forget(&self, key: &str) {
        {
            let mut index = self.index.lock();
            if let Some(old) = index.entries.remove(key) {
                index.total -= old.size;
            }
        }
        let _ = std::fs::remove_file(self.path(key));
    }

    /// The store moved `from` to `to`: what is here of it moves too.
    pub fn rename(&self, from: &str, to: &str) {
        let moved = {
            let mut index = self.index.lock();
            let entry = index.entries.remove(from);
            if let Some(old) = index.entries.remove(to) {
                index.total -= old.size;
            }
            entry
        };
        let _ = std::fs::remove_file(self.path(to));
        let Some(entry) = moved else {
            return;
        };
        let target = self.path(to);
        let renamed = target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(self.path(from), &target));
        let mut index = self.index.lock();
        if renamed.is_ok() {
            index.entries.insert(to.to_string(), entry);
        } else {
            index.total -= entry.size;
        }
    }

    /// Whether to fetch `key` now: false when it is already being fetched.
    pub fn start(&self, key: &str) -> bool {
        self.filling.lock().insert(key.to_string())
    }

    pub fn done(&self, key: &str) {
        self.filling.lock().remove(key);
    }

    /// Remove the least recently used until the budget holds.
    fn evict(&self) {
        let gone = {
            let mut index = self.index.lock();
            let mut gone = Vec::new();
            while index.total > self.budget {
                let Some(oldest) = index
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.used)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some(entry) = index.entries.remove(&oldest) {
                    index.total -= entry.size;
                }
                gone.push(oldest);
            }
            gone
        };
        for key in gone {
            let _ = std::fs::remove_file(self.path(&key));
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
        disk.keep(key, &aside, bytes.len() as u64).unwrap();
    }

    #[test]
    fn the_least_recently_used_go_past_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path(), 300).unwrap();
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
    }

    #[test]
    fn what_is_too_large_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path(), 400).unwrap();
        write(&disk, "o/aa/a", &[1; 101]);
        assert!(disk.get("o/aa/a").is_none());
        assert_eq!(disk.total(), 0);
    }

    #[test]
    fn it_is_taken_up_again_and_moves_with_the_store() {
        let dir = tempfile::tempdir().unwrap();
        {
            let disk = Disk::open(dir.path(), 1000).unwrap();
            write(&disk, "work/a/w/original", b"made");
            std::fs::write(disk.aside(), b"half").unwrap();
        }
        let disk = Disk::open(dir.path(), 1000).unwrap();
        assert_eq!(disk.total(), 4, "part-written files are not taken up");
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
    fn a_key_is_fetched_once_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path(), 1000).unwrap();
        assert!(disk.start("o/aa/a"));
        assert!(!disk.start("o/aa/a"));
        disk.done("o/aa/a");
        assert!(disk.start("o/aa/a"));
    }
}
