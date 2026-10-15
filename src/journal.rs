//! A history of what changed in the indexed folders: files and folders
//! created, deleted, changed, moved or renamed, as the indexer noticed them
//! (live, or at start-up for changes made while File Minnow was not
//! running). Found by comparing the index before and after each update;
//! kept in `journal.log` in the data folder for 30 days.
use crate::{
    config::RawPath,
    index::Kind,
    table::{Table, path_cmp, within},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Created,
    Deleted,
    /// A file's size or modification time changed.
    Modified,
    /// Moved or renamed: `path` is the old path, `to` the new one.
    Moved,
}
impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Deleted => "Deleted",
            Self::Modified => "Changed",
            Self::Moved => "Moved",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// When File Minnow noticed it (seconds since the epoch).
    #[serde(rename = "t")]
    pub time: i64,
    #[serde(rename = "a")]
    pub action: Action,
    #[serde(rename = "k")]
    pub kind: Kind,
    #[serde(rename = "p")]
    pub path: RawPath,
    #[serde(rename = "to", default, skip_serializing_if = "Option::is_none")]
    pub to: Option<RawPath>,
    #[serde(rename = "s", default)]
    pub size: u64,
    /// Modification and creation times, to pair a deletion with the
    /// creation of the same file; not stored.
    #[serde(skip)]
    stamp: (i64, i64),
}
impl Change {
    /// "Renamed" for a move within one folder, else the action's label.
    pub fn label(&self) -> &'static str {
        let parent = |p: &RawPath| p.path().parent().map(Path::to_path_buf);
        match &self.to {
            Some(to) if parent(to) == parent(&self.path) => "Renamed",
            _ => self.action.label(),
        }
    }
    /// The path the entry has now (the new path of a move).
    pub fn current(&self) -> &RawPath {
        self.to.as_ref().unwrap_or(&self.path)
    }
}

/// How long changes are kept, and at most how many.
const KEEP_SECONDS: i64 = 30 * 86_400;
const KEEP_CHANGES: usize = 100_000;
/// One update that changes more entries than this is recorded as far as
/// this limit (a huge copy or deletion), keeping the history readable.
const MAX_PER_UPDATE: usize = 20_000;

/// Differences between `old` and `new` inside `targets` (all rows when
/// None), with deletions and creations of the same file paired into moves.
pub fn diff(old: &Table, new: &Table, targets: Option<&[PathBuf]>, time: i64) -> Vec<Change> {
    use std::os::unix::ffi::OsStrExt;
    let mut changes = Vec::new();
    match targets {
        None => compare(old, (0, old.len()), new, (0, new.len()), time, &mut changes),
        Some(targets) => {
            // Outermost targets only, in path order.
            let mut sorted: Vec<&[u8]> = targets.iter().map(|t| t.as_os_str().as_bytes()).collect();
            sorted.sort_by(|a, b| path_cmp(a, b));
            let mut kept: Vec<&[u8]> = Vec::new();
            for target in sorted {
                if kept.last().is_none_or(|last| !within(target, last)) {
                    kept.push(target);
                }
            }
            for target in kept {
                let range = |table: &Table| {
                    let start = table.lower_bound(target);
                    (start, table.subtree_end(start, target))
                };
                compare(old, range(old), new, range(new), time, &mut changes);
                if changes.len() > MAX_PER_UPDATE {
                    break;
                }
            }
        }
    }
    pair_moves(&mut changes);
    changes.truncate(MAX_PER_UPDATE);
    changes
}
/// Merges two ranges of rows in path order.
fn compare(
    old: &Table,
    (mut i, old_end): (usize, usize),
    new: &Table,
    (mut j, new_end): (usize, usize),
    time: i64,
    out: &mut Vec<Change>,
) {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let change = |table: &Table, row: usize, path: &[u8], action| Change {
        time,
        action,
        kind: table.row(row).kind,
        path: RawPath(path.to_vec()),
        to: None,
        size: table.row(row).size,
        stamp: (table.row(row).modified, table.row(row).created),
    };
    while (i < old_end || j < new_end) && out.len() <= MAX_PER_UPDATE {
        if i < old_end {
            old.write_path(i, &mut a);
        }
        if j < new_end {
            new.write_path(j, &mut b);
        }
        let order = match (i < old_end, j < new_end) {
            (true, true) => path_cmp(&a, &b),
            (true, false) => Ordering::Less,
            _ => Ordering::Greater,
        };
        match order {
            Ordering::Less => {
                out.push(change(old, i, &a, Action::Deleted));
                i += 1;
            }
            Ordering::Greater => {
                out.push(change(new, j, &b, Action::Created));
                j += 1;
            }
            Ordering::Equal => {
                let (x, y) = (old.row(i), new.row(j));
                if x.kind != y.kind {
                    out.push(change(old, i, &a, Action::Deleted));
                    out.push(change(new, j, &b, Action::Created));
                } else if y.kind == Kind::File && (x.size != y.size || x.modified != y.modified) {
                    out.push(change(new, j, &b, Action::Modified));
                }
                i += 1;
                j += 1;
            }
        }
    }
}
/// A deletion and a creation of the same entry (same kind, size and times,
/// and the same name or the same folder) are one move or rename. Entries
/// inside a moved folder are not listed again.
fn pair_moves(changes: &mut Vec<Change>) {
    let name =
        |p: &RawPath| -> Vec<u8> { p.0.rsplit(|b| *b == b'/').next().unwrap_or(&[]).to_vec() };
    let parent = |p: &RawPath| -> Vec<u8> {
        match p.0.iter().rposition(|b| *b == b'/') {
            Some(i) => p.0[..i].to_vec(),
            None => Vec::new(),
        }
    };
    let created: Vec<usize> = (0..changes.len())
        .filter(|&i| changes[i].action == Action::Created)
        .collect();
    let mut used = vec![false; changes.len()];
    let mut moved_folders: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    // Moves keep the name; renames keep the folder. Names first, so a
    // folder moved into a new folder is not taken for a rename of it.
    for same_name in [true, false] {
        for d in 0..changes.len() {
            if changes[d].action != Action::Deleted {
                continue;
            }
            let gone = changes[d].clone();
            let partner = created.iter().copied().find(|&c| {
                let new = &changes[c];
                !used[c]
                    && new.kind == gone.kind
                    && new.size == gone.size
                    && new.stamp == gone.stamp
                    && if same_name {
                        name(&new.path) == name(&gone.path)
                    } else {
                        parent(&new.path) == parent(&gone.path)
                    }
                    && !within(&new.path.0, &gone.path.0)
            });
            if let Some(c) = partner {
                used[c] = true;
                if gone.kind == Kind::Folder {
                    moved_folders.push((gone.path.0.clone(), changes[c].path.0.clone()));
                }
                let to = changes[c].path.clone();
                changes[d].action = Action::Moved;
                changes[d].to = Some(to);
            }
        }
    }
    let inside_moved = |change: &Change| {
        moved_folders.iter().any(|(from, to)| {
            (change.action == Action::Deleted
                && within(&change.path.0, from)
                && change.path.0 != *from)
                || (change.action == Action::Created
                    && within(&change.path.0, to)
                    && change.path.0 != *to)
                || (change.action == Action::Moved
                    && within(&change.path.0, from)
                    && change.path.0 != *from)
        })
    };
    let mut index = 0;
    changes.retain(|change| {
        let keep = !used[index] && !inside_moved(change);
        index += 1;
        keep
    });
}

/// The history file of a data folder.
pub struct Journal {
    path: PathBuf,
}
impl Journal {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("journal.log"),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Appends changes (one JSON line each).
    pub fn append(&self, changes: &[Change]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut text = Vec::new();
        for change in changes {
            serde_json::to_writer(&mut text, change)?;
            text.push(b'\n');
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(&text)?;
        Ok(())
    }
    /// Changes noticed since `since` (seconds), newest first.
    pub fn read(&self, since: i64) -> Vec<Change> {
        let Ok(file) = fs::File::open(&self.path) else {
            return Vec::new();
        };
        let mut changes: Vec<Change> = BufReader::new(file)
            .split(b'\n')
            .map_while(std::result::Result::ok)
            .filter_map(|line| serde_json::from_slice::<Change>(&line).ok())
            .filter(|c| c.time >= since)
            .collect();
        changes.reverse();
        changes
    }
    /// Drops changes older than 30 days and all but the newest 100,000.
    pub fn trim(&self, now: i64) -> Result<()> {
        let size = fs::metadata(&self.path).map_or(0, |m| m.len());
        if size < 1 << 20 {
            return Ok(());
        }
        let mut kept = self.read(now - KEEP_SECONDS);
        kept.truncate(KEEP_CHANGES);
        kept.reverse();
        let mut text = Vec::new();
        for change in &kept {
            serde_json::to_writer(&mut text, change)?;
            text.push(b'\n');
        }
        if text.is_empty() {
            let _ = fs::remove_file(&self.path);
            return Ok(());
        }
        crate::config::atomic_write(&self.path, &text[..text.len() - 1])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Entry;
    fn table(rows: &[(&str, Kind, u64)]) -> Table {
        Table::from_entries(
            rows.iter()
                .map(|(p, k, s)| Entry::new(p.as_bytes().to_vec(), *k, *s, 100))
                .collect(),
        )
    }
    fn summary(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .map(|c| match &c.to {
                Some(to) => format!(
                    "{} {} -> {}",
                    c.action.label(),
                    c.path.display(),
                    to.display()
                ),
                None => format!("{} {}", c.action.label(), c.path.display()),
            })
            .collect()
    }
    #[test]
    fn creations_deletions_changes_and_moves() {
        let (f, d) = (Kind::File, Kind::Folder);
        let old = table(&[
            ("/r", d, 0),
            ("/r/a.txt", f, 1),
            ("/r/b.txt", f, 2),
            ("/r/photos", d, 0),
            ("/r/photos/1.jpg", f, 10),
            ("/r/photos/2.jpg", f, 20),
            ("/r/report.odt", f, 5),
            ("/r/x/keep.txt", f, 3),
        ]);
        let new = table(&[
            ("/r", d, 0),
            ("/r/a.txt", f, 9),
            ("/r/archive", d, 0),
            ("/r/archive/photos", d, 0),
            ("/r/archive/photos/1.jpg", f, 10),
            ("/r/archive/photos/2.jpg", f, 20),
            ("/r/c.txt", f, 7),
            ("/r/report final.odt", f, 5),
            ("/r/x/keep.txt", f, 3),
        ]);
        let mut got = summary(&diff(&old, &new, None, 1));
        got.sort();
        let mut expected = vec![
            "Changed /r/a.txt",
            "Created /r/archive",
            "Moved /r/photos -> /r/archive/photos",
            "Created /r/c.txt",
            "Deleted /r/b.txt",
            "Moved /r/report.odt -> /r/report final.odt",
        ];
        expected.sort();
        assert_eq!(got, expected);
        // Only inside the targets.
        let changes = diff(&old, &new, Some(&[PathBuf::from("/r/x")]), 1);
        assert!(changes.is_empty());
    }
    #[test]
    fn journal_round_trip_and_trim() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Journal::new(temp.path());
        let change = |time, path: &[u8]| Change {
            time,
            action: Action::Created,
            kind: Kind::File,
            path: RawPath(path.to_vec()),
            to: None,
            size: 1,
            stamp: (0, 0),
        };
        journal
            .append(&[change(10, b"/a"), change(20, b"/b-\xff")])
            .unwrap();
        journal.append(&[change(30, b"/c")]).unwrap();
        let read = journal.read(15);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].path.0, b"/c");
        assert_eq!(read[1].path.0, b"/b-\xff");
        journal.trim(1_000_000_000).unwrap();
        assert_eq!(journal.read(0).len(), 3, "small files are left alone");
    }
}
