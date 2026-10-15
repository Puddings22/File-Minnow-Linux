pub use crate::table::{Builder, Table, path_cmp};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Folder,
    Link,
    Other,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Folder => "folder",
            Self::Link => "link",
            Self::Other => "other",
        }
    }
}
impl From<&str> for Kind {
    fn from(s: &str) -> Self {
        match s {
            "file" => Self::File,
            "folder" => Self::Folder,
            "link" => Self::Link,
            _ => Self::Other,
        }
    }
}
impl PartialEq<&str> for Kind {
    fn eq(&self, s: &&str) -> bool {
        self.as_str() == *s
    }
}

/// One materialized result. The index itself stores entries compactly in
/// [`Table`]; full paths are only built for results that are returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: Box<[u8]>,
    name_start: u32,
    pub kind: Kind,
    /// File size; for a folder, the total size of the indexed files inside.
    pub size: u64,
    pub modified: i64,
    /// Creation time (seconds), or `table::NO_DATE` when unknown.
    #[serde(default = "no_date")]
    pub created: i64,
}
fn no_date() -> i64 {
    crate::table::NO_DATE
}
impl Entry {
    pub fn new(path: Vec<u8>, kind: Kind, size: u64, modified: i64) -> Self {
        let name_start = if path == b"/" {
            0
        } else {
            path.iter()
                .rposition(|b| *b == b'/')
                .map(|i| i + 1)
                .unwrap_or(0)
        } as u32;
        Self {
            path: path.into_boxed_slice(),
            name_start,
            kind,
            size,
            modified,
            created: crate::table::NO_DATE,
        }
    }
    pub fn created(mut self, created: i64) -> Self {
        self.created = created;
        self
    }
    pub fn name(&self) -> &[u8] {
        &self.path[self.name_start as usize..]
    }
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let m = fs::symlink_metadata(path)?;
        Ok(Self::new(
            path.as_os_str().as_bytes().to_vec(),
            kind_of(&m),
            m.len(),
            m.mtime(),
        )
        .created(created_of(&m)))
    }
    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(std::ffi::OsString::from_vec(self.path.to_vec()))
    }
    pub fn display_path(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }
    pub fn display_name(&self) -> String {
        String::from_utf8_lossy(self.name()).into_owned()
    }
}
/// Creation time from statx, where the filesystem records one.
fn created_of(m: &fs::Metadata) -> i64 {
    m.created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(crate::table::NO_DATE, |d| d.as_secs() as i64)
}
fn kind_of(m: &fs::Metadata) -> Kind {
    if m.is_dir() {
        Kind::Folder
    } else if m.file_type().is_symlink() {
        Kind::Link
    } else if m.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub roots: Vec<Vec<u8>>,
    #[serde(skip)]
    pub entries: Table,
    pub generation: u64,
    pub scanned_at: u64,
    pub errors: Vec<String>,
    pub error_count: usize,
    /// When the index was last saved (set in the saved copy only).
    #[serde(default)]
    pub saved_at: u64,
    /// `IndexConfig::fingerprint` of the settings these entries follow.
    #[serde(default)]
    pub config: u64,
    /// When each root was last walked completely, for scheduled rescans
    /// that were missed while File Minnow was not running.
    #[serde(default)]
    pub walked: Vec<(Vec<u8>, u64)>,
}
impl Snapshot {
    /// The same metadata with no entries, for persistence headers.
    pub fn header(&self) -> Self {
        Self {
            roots: self.roots.clone(),
            entries: Table::default(),
            generation: self.generation,
            scanned_at: self.scanned_at,
            errors: self.errors.clone(),
            error_count: self.error_count,
            saved_at: self.saved_at,
            config: self.config,
            walked: self.walked.clone(),
        }
    }
    /// When `root` was last walked completely (0: never).
    pub fn walked_at(&self, root: &[u8]) -> u64 {
        self.walked
            .iter()
            .find(|(r, _)| r == root)
            .map_or(0, |(_, t)| *t)
    }
    /// Records that these roots were just walked completely.
    pub fn mark_walked(&mut self, roots: impl IntoIterator<Item = Vec<u8>>) {
        let time = now();
        for root in roots {
            match self.walked.iter_mut().find(|(r, _)| *r == root) {
                Some(entry) => entry.1 = time,
                None => self.walked.push((root, time)),
            }
        }
        let current = self.roots.clone();
        self.walked.retain(|(r, _)| current.contains(r));
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn root_paths(roots: &[Vec<u8>]) -> Vec<PathBuf> {
    roots
        .iter()
        .map(|r| PathBuf::from(std::ffi::OsString::from_vec(r.clone())))
        .collect()
}
pub fn normalize_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for root in roots {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            bail!("Index root is not a directory: {}", root.display());
        }
        if ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .any(|p| root.starts_with(p))
        {
            bail!("Pseudo/runtime filesystem is excluded: {}", root.display());
        }
        paths.push(root);
    }
    paths.sort();
    paths.dedup();
    let mut result: Vec<PathBuf> = Vec::new();
    for p in paths {
        if !result.iter().any(|r| p.starts_with(r)) {
            result.push(p);
        }
    }
    if result.is_empty() {
        bail!("Select at least one folder to index");
    }
    Ok(result)
}
pub fn excluded(path: &Path, data_dir: &Path) -> bool {
    path.starts_with(data_dir)
        || ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .any(|p| path.starts_with(p))
}
pub fn record_error(errors: &mut Vec<String>, count: &mut usize, message: String) {
    *count += 1;
    if errors.len() < 50 {
        errors.push(message);
    }
}
pub fn scan(roots: &[PathBuf], data_dir: &Path, generation: u64) -> Snapshot {
    scan_config(
        &crate::config::IndexConfig::from_roots(roots),
        data_dir,
        generation,
        None,
        &|| false,
        &|_| {},
    )
    .expect("default scan configuration is valid")
    .expect("uncancellable scan")
}

/// Returns None when interrupted. Callers must never publish a partial traversal
/// as a complete generation. Target paths reconcile subtrees using root policy.
pub fn scan_config(
    config: &crate::config::IndexConfig,
    data_dir: &Path,
    generation: u64,
    targets: Option<&[PathBuf]>,
    cancel: &dyn Fn() -> bool,
    progress: &dyn Fn(usize),
) -> Result<Option<Snapshot>> {
    scan_config_watched(
        config, data_dir, generation, targets, None, cancel, progress, None,
    )
}

/// Called for each monitored folder as the scan reaches it, before the
/// folder is read, so changes during the scan are not missed.
pub type WatchHook<'a> = Option<&'a dyn Fn(&Path)>;

/// Whether a root folder is unavailable: missing, or an empty directory
/// (an unmounted mount point). Only meaningful for roots that keep their
/// entries while offline.
pub fn is_offline(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}
/// Enabled roots that keep their entries and are unavailable right now.
pub fn offline_roots(config: &crate::config::IndexConfig) -> Vec<String> {
    config
        .roots
        .iter()
        .filter(|r| r.enabled && r.keep_offline && is_offline(&r.path.path()))
        .map(|r| r.path.display())
        .collect()
}

#[allow(clippy::too_many_arguments)] // scan inputs plus three callbacks
pub fn scan_config_watched(
    config: &crate::config::IndexConfig,
    data_dir: &Path,
    generation: u64,
    targets: Option<&[PathBuf]>,
    previous: Option<&Table>,
    cancel: &dyn Fn() -> bool,
    progress: &dyn Fn(usize),
    watch: WatchHook,
) -> Result<Option<Snapshot>> {
    let mut builder = Builder::default();
    let mut errors = Vec::new();
    let mut error_count = 0;
    let mut visited = 0;
    let completed = scan_into(
        config,
        data_dir,
        targets,
        previous,
        &mut builder,
        &mut ScanLog {
            errors: &mut errors,
            count: &mut error_count,
            visited: &mut visited,
        },
        cancel,
        progress,
        watch,
    )?;
    if !completed {
        return Ok(None);
    }
    let roots: Vec<Vec<u8>> = config
        .roots
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.path.0.clone())
        .collect();
    let mut snapshot = Snapshot {
        roots: roots.clone(),
        entries: builder.finish(),
        generation,
        scanned_at: now(),
        errors,
        error_count,
        config: config.fingerprint(),
        ..Snapshot::default()
    };
    snapshot.mark_walked(roots);
    Ok(Some(snapshot))
}

pub struct ScanLog<'a> {
    pub errors: &'a mut Vec<String>,
    pub count: &'a mut usize,
    pub visited: &'a mut usize,
}

/// Walks roots (or targets inside them) and appends rows in component path
/// order, so no sorted copy of all paths is ever built. Returns false when
/// cancelled; the builder then holds a partial walk and must be discarded.
#[allow(clippy::too_many_arguments)] // scan inputs plus three callbacks
pub fn scan_into(
    config: &crate::config::IndexConfig,
    data_dir: &Path,
    targets: Option<&[PathBuf]>,
    previous: Option<&Table>,
    builder: &mut Builder,
    log: &mut ScanLog,
    cancel: &dyn Fn() -> bool,
    progress: &dyn Fn(usize),
    watch: WatchHook,
) -> Result<bool> {
    let rules = crate::config::PatternRules::new(config)?;
    let mut roots: Vec<_> = config.roots.iter().filter(|r| r.enabled).collect();
    roots.sort_by(|a, b| path_cmp(&a.path.0, &b.path.0));
    for root_config in roots {
        let root = root_config.path.path();
        let mut paths: Vec<_> = match targets {
            None => vec![root.clone()],
            Some(paths) => paths
                .iter()
                .filter(|p| p.starts_with(&root))
                .cloned()
                .collect(),
        };
        paths.sort_by(|a, b| path_cmp(a.as_os_str().as_bytes(), b.as_os_str().as_bytes()));
        // An unplugged drive or unmounted share: keep what was indexed.
        if root_config.keep_offline
            && is_offline(&root)
            && let Some(previous) = previous
        {
            for path in paths {
                let bytes = path.as_os_str().as_bytes();
                let start = previous.lower_bound(bytes);
                let end = previous.subtree_end(start, bytes);
                builder.copy_range(previous, start, end);
            }
            continue;
        }
        for path in paths {
            // A changed path inside a folder the rules skip (the scan never
            // entered it) stays out of the index.
            if path
                .ancestors()
                .skip(1)
                .take_while(|a| a.starts_with(&root) && *a != root)
                .any(|a| !rules.traverse(a, &root))
            {
                continue;
            }
            let depth = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .components()
                .count();
            if !root_config.recursive && depth > 1 {
                continue;
            }
            let max_depth = if root_config.recursive {
                usize::MAX
            } else {
                1 - depth
            };
            for item in WalkDir::new(&path)
                .follow_links(false)
                .max_depth(max_depth)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|e| !excluded(e.path(), data_dir) && rules.traverse(e.path(), &root))
            {
                if cancel() {
                    return Ok(false);
                }
                *log.visited += 1;
                if (*log.visited).is_multiple_of(512) {
                    progress(*log.visited);
                }
                let item = match item {
                    Ok(item) => item,
                    Err(err) => {
                        record_error(log.errors, log.count, err.to_string());
                        continue;
                    }
                };
                match item.metadata() {
                    Ok(m) => {
                        let kind = kind_of(&m);
                        // Non-recursive roots watch only the root folder itself.
                        if kind == Kind::Folder
                            && root_config.monitor
                            && (root_config.recursive || depth + item.depth() == 0)
                            && let Some(watch) = watch
                        {
                            watch(item.path());
                        }
                        let indexed = if kind == Kind::Folder {
                            rules.index_folder(item.path(), &root)
                        } else {
                            rules.include_file(item.path(), &root)
                        };
                        if indexed
                            && !builder.push_with(
                                item.path().as_os_str().as_bytes(),
                                kind,
                                m.len(),
                                m.mtime(),
                                created_of(&m),
                            )
                        {
                            record_error(
                                log.errors,
                                log.count,
                                format!("{}: name too long to index", item.path().display()),
                            );
                        }
                    }
                    Err(err) => record_error(
                        log.errors,
                        log.count,
                        format!("{}: {err}", item.path().display()),
                    ),
                }
            }
        }
    }
    progress(*log.visited);
    Ok(true)
}

/// Start-up catch-up from a saved index. Every indexed folder is checked
/// (and watched) again, but only the folders whose modification time
/// changed since the index was saved are read; the others keep their saved
/// entries. That is one `stat` per folder instead of one per entry. A file
/// changed in place while File Minnow was not running does not change its
/// folder, so it keeps its old size and date until the next full walk (the
/// scheduled rescan). Returns None when interrupted.
pub fn quick_scan(
    config: &crate::config::IndexConfig,
    data_dir: &Path,
    previous: &Snapshot,
    cancel: &dyn Fn() -> bool,
    progress: &dyn Fn(usize),
    watch: WatchHook,
) -> Result<Option<Snapshot>> {
    let rules = crate::config::PatternRules::new(config)?;
    let table = &previous.entries;
    let mut builder = Builder::with_capacity(table.len());
    let mut errors = Vec::new();
    let mut error_count = 0;
    let mut visited = 0;
    let mut kept = Vec::new();
    let mut roots: Vec<_> = config.roots.iter().filter(|r| r.enabled).collect();
    roots.sort_by(|a, b| path_cmp(&a.path.0, &b.path.0));
    for root_config in roots {
        let root = root_config.path.path();
        let bytes = root.as_os_str().as_bytes();
        let start = table.lower_bound(bytes);
        let indexed = start < table.len()
            && table.path(start) == bytes
            && table.row(start).kind == Kind::Folder;
        let mut log = ScanLog {
            errors: &mut errors,
            count: &mut error_count,
            visited: &mut visited,
        };
        if !root_config.recursive || !indexed || (root_config.keep_offline && is_offline(&root)) {
            // Walked, or kept while offline, as a full scan would.
            let targets = [root.clone()];
            if !scan_into(
                config,
                data_dir,
                Some(&targets),
                Some(table),
                &mut builder,
                &mut log,
                cancel,
                progress,
                watch,
            )? {
                return Ok(None);
            }
            continue;
        }
        let mut walk = Quick {
            config,
            data_dir,
            rules: &rules,
            table,
            // A folder changed within two seconds of the save may have
            // changed after it was read, within the same second.
            changed_after: previous.saved_at as i64 - 2,
            root: &root,
            monitor: root_config.monitor,
            builder: &mut builder,
            kept: &mut kept,
            log,
            cancel,
            progress,
            watch,
        };
        let end = table.subtree_end(start, bytes);
        if !walk.folder(&root, start, end)? {
            return Ok(None);
        }
    }
    progress(visited);
    let entries = builder.finish();
    entries.splice_name_order(table, &kept);
    Ok(Some(Snapshot {
        roots: config
            .roots
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.0.clone())
            .collect(),
        entries,
        generation: previous.generation + 1,
        scanned_at: now(),
        errors,
        error_count,
        saved_at: 0,
        config: config.fingerprint(),
        walked: previous.walked.clone(),
    }))
}

struct Quick<'a, 'b> {
    config: &'a crate::config::IndexConfig,
    data_dir: &'a Path,
    rules: &'a crate::config::PatternRules,
    table: &'a Table,
    changed_after: i64,
    root: &'a Path,
    monitor: bool,
    builder: &'a mut Builder,
    /// Rows copied unchanged: (old start, old end, new start).
    kept: &'a mut Vec<(usize, usize, usize)>,
    log: ScanLog<'b>,
    cancel: &'a dyn Fn() -> bool,
    progress: &'a dyn Fn(usize),
    watch: WatchHook<'a>,
}
impl Quick<'_, '_> {
    fn copy(&mut self, start: usize, end: usize) {
        if start < end {
            self.kept.push((start, end, self.builder.len()));
            self.builder.copy_range(self.table, start, end);
        }
    }
    fn tick(&mut self) -> bool {
        *self.log.visited += 1;
        if (*self.log.visited).is_multiple_of(512) {
            (self.progress)(*self.log.visited);
        }
        !(self.cancel)()
    }
    /// A path that is new, or no longer a folder: walked in full.
    fn walk(&mut self, path: &Path) -> Result<bool> {
        let targets = [path.to_path_buf()];
        scan_into(
            self.config,
            self.data_dir,
            Some(&targets),
            None,
            self.builder,
            &mut self.log,
            self.cancel,
            self.progress,
            self.watch,
        )
    }
    /// The indexed folder `path` at `row`, its subtree ending at `end`.
    fn folder(&mut self, path: &Path, row: usize, end: usize) -> Result<bool> {
        if !self.tick() {
            return Ok(false);
        }
        // Watched before it is checked, so no change is missed in between.
        if self.monitor
            && let Some(watch) = self.watch
        {
            watch(path);
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => {
                record_error(
                    self.log.errors,
                    self.log.count,
                    format!("{}: {e}", path.display()),
                );
                self.copy(row, end);
                return Ok(true);
            }
        };
        if !meta.is_dir() {
            return self.walk(path);
        }
        let unchanged =
            meta.mtime() == self.table.row(row).modified && meta.mtime() < self.changed_after;
        if unchanged {
            // The folder's own entries are as saved; its subfolders are
            // checked in turn.
            let (mut i, mut run) = (row + 1, row);
            while i < end {
                if self.table.row(i).kind != Kind::Folder {
                    i += 1;
                    continue;
                }
                self.copy(run, i);
                let child = self.table.path(i);
                let child_end = self.table.subtree_end(i, &child);
                let child = PathBuf::from(std::ffi::OsString::from_vec(child));
                if !self.folder(&child, i, child_end)? {
                    return Ok(false);
                }
                (i, run) = (child_end, child_end);
            }
            self.copy(run, end);
            return Ok(true);
        }
        if self.rules.index_folder(path, self.root) {
            self.builder.push_with(
                path.as_os_str().as_bytes(),
                Kind::Folder,
                meta.len(),
                meta.mtime(),
                created_of(&meta),
            );
        }
        let mut names: Vec<std::ffi::OsString> = match fs::read_dir(path) {
            Ok(entries) => entries
                .filter_map(|e| e.ok().map(|e| e.file_name()))
                .collect(),
            Err(e) => {
                record_error(
                    self.log.errors,
                    self.log.count,
                    format!("{}: {e}", path.display()),
                );
                self.copy(row + 1, end);
                return Ok(true);
            }
        };
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for name in names {
            if !self.tick() {
                return Ok(false);
            }
            let child = path.join(&name);
            if excluded(&child, self.data_dir) || !self.rules.traverse(&child, self.root) {
                continue;
            }
            let m = match fs::symlink_metadata(&child) {
                Ok(m) => m,
                Err(e) => {
                    record_error(
                        self.log.errors,
                        self.log.count,
                        format!("{}: {e}", child.display()),
                    );
                    continue;
                }
            };
            let kind = kind_of(&m);
            if kind == Kind::Folder {
                let bytes = child.as_os_str().as_bytes();
                let at = self.table.lower_bound(bytes);
                let known = at < end
                    && self.table.row(at).kind == Kind::Folder
                    && self.table.path(at) == bytes;
                let completed = if known {
                    let child_end = self.table.subtree_end(at, bytes);
                    self.folder(&child, at, child_end)?
                } else {
                    self.walk(&child)?
                };
                if !completed {
                    return Ok(false);
                }
            } else if self.rules.include_file(&child, self.root)
                && !self.builder.push_with(
                    child.as_os_str().as_bytes(),
                    kind,
                    m.len(),
                    m.mtime(),
                    created_of(&m),
                )
            {
                record_error(
                    self.log.errors,
                    self.log.count,
                    format!("{}: name too long to index", child.display()),
                );
            }
        }
        Ok(true)
    }
}
