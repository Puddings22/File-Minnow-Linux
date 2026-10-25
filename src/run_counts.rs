//! Run counts: how often each file was opened or launched through
//! File Minnow, like the "Run count" column in Everything for Windows.
//! The counts live in a small JSON file beside the settings, separate
//! from the search index so a mutable counter never churns the
//! path-ordered index rows. Only opens that go through the app are
//! counted (option 1): launching from the menus, Enter/double-click and
//! the portal `Open` path.
use crate::config::atomic_write;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Keep at most this many entries; the least recently used go first.
const LIMIT: usize = 5_000;

/// One row of the JSON file. A list of records, not a map, because
/// JSON object keys must be strings and paths are easier kept whole.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Item {
    pub path: PathBuf,
    /// Times opened or launched.
    pub count: u32,
    /// Unix seconds of the last use.
    pub time: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A path-keyed counter, shareable across threads (`Arc<RunCounts>`).
#[derive(Debug)]
pub struct RunCounts {
    path: PathBuf,
    entries: Mutex<HashMap<PathBuf, (u32, u64)>>,
}

impl RunCounts {
    /// The file for the settings file's directory (`run_counts.json`).
    pub fn path_for(settings: &Path) -> PathBuf {
        settings.with_file_name("run_counts.json")
    }
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() < 8 << 20)
            .and_then(|bytes| serde_json::from_slice::<Vec<Item>>(&bytes).ok())
            .map(|items| {
                items
                    .into_iter()
                    .map(|i| (i.path, (i.count, i.time)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path,
            entries: Mutex::new(map),
        }
    }
    /// Records one open of `path` and returns the new count. Saves the
    /// file; a failed write is ignored, like the search history's.
    pub fn record(&self, path: &Path) -> u32 {
        let count = {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let (count, time) = entries.entry(path.to_path_buf()).or_insert((0, 0));
            *count = count.saturating_add(1);
            *time = now();
            if entries.len() > LIMIT {
                // Drop the least recently used down to the limit.
                let mut by_time: Vec<(u64, PathBuf)> = entries
                    .iter()
                    .map(|(p, (_, time))| (*time, p.clone()))
                    .collect();
                by_time.sort_unstable();
                for (_, p) in by_time.drain(..entries.len() - LIMIT) {
                    entries.remove(&p);
                }
            }
            entries.get(path).map_or(1, |(count, _)| *count)
        };
        self.save();
        count
    }
    /// Records several opens, one per path.
    pub fn record_all(&self, paths: &[PathBuf]) {
        for path in paths {
            self.record(path);
        }
    }
    /// The count for `path`, zero when unseen.
    pub fn count(&self, path: &Path) -> u32 {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .map_or(0, |(count, _)| *count)
    }
    /// The counts keyed by raw path bytes, for the query engine: comparing
    /// index rows must not allocate a `PathBuf` per lookup.
    pub fn snapshot(&self) -> HashMap<Vec<u8>, u32> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(path, (count, _))| (path.as_os_str().as_bytes().to_vec(), *count))
            .collect()
    }
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.save();
    }
    fn save(&self) {
        let items: Vec<Item> = {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let mut items: Vec<Item> = entries
                .iter()
                .map(|(path, (count, time))| Item {
                    path: path.clone(),
                    count: *count,
                    time: *time,
                })
                .collect();
            items.sort_by(|a, b| a.path.cmp(&b.path));
            items
        };
        if let Ok(json) = serde_json::to_vec(&items) {
            let _ = atomic_write(&self.path, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_increment_and_persist() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run_counts.json");
        let counts = RunCounts::load(path.clone());
        assert!(counts.is_empty());
        let file = Path::new("/home/user/app.bin");
        assert_eq!(counts.record(file), 1);
        assert_eq!(counts.record(file), 2);
        assert_eq!(counts.record(Path::new("/home/user/other.txt")), 1);
        assert_eq!(counts.count(file), 2);
        assert_eq!(counts.count(Path::new("/home/user/never.txt")), 0);
        let reloaded = RunCounts::load(path);
        assert_eq!(reloaded.count(file), 2);
        assert_eq!(reloaded.len(), 2);
        reloaded.clear();
        assert_eq!(
            RunCounts::load(temp.path().join("run_counts.json")).len(),
            0
        );
    }

    #[test]
    fn record_all_and_path_for() {
        let settings = Path::new("/cfg/file-minnow/settings.json");
        assert_eq!(
            RunCounts::path_for(settings),
            PathBuf::from("/cfg/file-minnow/run_counts.json")
        );
        let temp = tempfile::tempdir().unwrap();
        let counts = RunCounts::load(temp.path().join("run_counts.json"));
        let paths: Vec<PathBuf> = ["/a", "/b", "/a"].map(PathBuf::from).to_vec();
        counts.record_all(&paths);
        assert_eq!(counts.count(Path::new("/a")), 2);
        assert_eq!(counts.count(Path::new("/b")), 1);
    }
}
