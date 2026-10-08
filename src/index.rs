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
    pub size: u64,
    pub modified: i64,
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
        }
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
        ))
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
        }
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
        config, data_dir, generation, targets, cancel, progress, None,
    )
}

/// Called for each monitored folder as the scan reaches it, before the
/// folder is read, so changes during the scan are not missed.
pub type WatchHook<'a> = Option<&'a dyn Fn(&Path)>;

pub fn scan_config_watched(
    config: &crate::config::IndexConfig,
    data_dir: &Path,
    generation: u64,
    targets: Option<&[PathBuf]>,
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
    Ok(Some(Snapshot {
        roots: config
            .roots
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.0.clone())
            .collect(),
        entries: builder.finish(),
        generation,
        scanned_at: now(),
        errors,
        error_count,
    }))
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
        for path in paths {
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
                        if (kind == Kind::Folder || rules.include_file(item.path(), &root))
                            && !builder.push(
                                item.path().as_os_str().as_bytes(),
                                kind,
                                m.len(),
                                m.mtime(),
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
