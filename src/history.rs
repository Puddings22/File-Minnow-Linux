//! Search history: the searches a user ran, newest first, kept in a small
//! JSON file beside the settings. Pruned by age and count from the
//! settings, so the file never grows without bound.
use crate::config::atomic_write;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub query: String,
    /// Unix seconds of the last use.
    pub time: u64,
}

#[derive(Debug)]
pub struct History {
    path: PathBuf,
    items: Vec<Item>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl History {
    /// The file for the settings file's directory (`history.json`).
    pub fn path_for(settings: &Path) -> PathBuf {
        settings.with_file_name("history.json")
    }
    pub fn load(path: PathBuf) -> Self {
        let items = std::fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() < 4 << 20)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, items }
    }
    /// Newest first.
    pub fn items(&self) -> &[Item] {
        &self.items
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// Records a search (moving it to the front if it exists) and prunes to
    /// `limit` entries not older than `days`. Returns whether anything changed.
    pub fn record(&mut self, query: &str, days: u32, limit: usize) -> bool {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return false;
        }
        self.items.retain(|i| i.query != query);
        self.items.insert(
            0,
            Item {
                query: query.to_owned(),
                time: now(),
            },
        );
        self.prune(days, limit);
        self.save();
        true
    }
    pub fn prune(&mut self, days: u32, limit: usize) {
        let oldest = now().saturating_sub(u64::from(days) * 86_400);
        self.items.retain(|i| i.time >= oldest);
        self.items.truncate(limit);
    }
    pub fn remove(&mut self, query: &str) {
        self.items.retain(|i| i.query != query);
        self.save();
    }
    pub fn clear(&mut self) {
        self.items.clear();
        self.save();
    }
    fn save(&self) {
        if let Ok(json) = serde_json::to_vec(&self.items) {
            let _ = atomic_write(&self.path, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_newest_first_and_prunes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        let mut history = History::load(path.clone());
        assert!(history.is_empty());
        assert!(!history.record("   ", 90, 10));
        for query in ["alpha", "beta", "alpha", "gamma"] {
            assert!(history.record(query, 90, 10));
        }
        let queries: Vec<&str> = history.items().iter().map(|i| i.query.as_str()).collect();
        assert_eq!(queries, ["gamma", "alpha", "beta"]);
        history.record("delta", 90, 2);
        assert_eq!(history.items().len(), 2);
        let reloaded = History::load(path);
        assert_eq!(reloaded.items()[0].query, "delta");
        let mut old = reloaded;
        old.remove("delta");
        assert_eq!(old.items().len(), 1);
        old.clear();
        assert!(old.is_empty());
    }
}
