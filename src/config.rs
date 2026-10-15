//! Versioned, durable preferences. Paths round-trip even when they are not UTF-8.
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawPath(pub Vec<u8>);
impl RawPath {
    pub fn new(path: &Path) -> Self {
        Self(path.as_os_str().as_bytes().to_vec())
    }
    pub fn path(&self) -> PathBuf {
        std::ffi::OsString::from_vec(self.0.clone()).into()
    }
    pub fn display(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}
impl Serialize for RawPath {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match std::str::from_utf8(&self.0) {
            Ok(text) => s.serialize_str(text),
            Err(_) => self.0.serialize(s),
        }
    }
}
impl<'de> Deserialize<'de> for RawPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Bytes(Vec<u8>),
        }
        Ok(Self(match Repr::deserialize(d)? {
            Repr::Text(s) => s.into_bytes(),
            Repr::Bytes(b) => b,
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    Interval { seconds: u64 },
    Daily { hour: u32, minute: u32 },
    Never,
}
/// File monitoring catches changes as they happen, so the full rescan is
/// only a safety net. Daily matches Everything's default and keeps a large
/// tree from being re-read every few minutes.
impl Default for Schedule {
    fn default() -> Self {
        Self::Daily { hour: 3, minute: 0 }
    }
}
impl Schedule {
    /// Daily times are local wall-clock time. A rescan missed while the
    /// computer was off or asleep runs at the next start-up scan instead.
    pub fn next_after(&self, unix_seconds: u64) -> Option<u64> {
        use chrono::{Offset, TimeZone};
        let offset = chrono::Local
            .timestamp_opt(unix_seconds as i64, 0)
            .single()
            .map_or(0, |t| i64::from(t.offset().fix().local_minus_utc()));
        self.next_after_in(unix_seconds, offset)
    }
    /// `next_after` for a fixed UTC offset in seconds.
    pub fn next_after_in(&self, unix_seconds: u64, offset: i64) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::Interval { seconds } => Some(unix_seconds.saturating_add(*seconds)),
            Self::Daily { hour, minute } => {
                let local = unix_seconds as i64 + offset;
                let mut candidate = local.div_euclid(86400) * 86400
                    + i64::from(*hour) * 3600
                    + i64::from(*minute) * 60;
                if candidate <= local {
                    candidate += 86400;
                }
                Some((candidate - offset).max(0) as u64)
            }
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Interval { seconds } if !(1..=31_536_000).contains(seconds) => {
                bail!("Rescan interval must be 1 second to 1 year")
            }
            Self::Daily { hour, minute } if *hour > 23 || *minute > 59 => {
                bail!("Invalid daily rescan time")
            }
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RootConfig {
    pub path: RawPath,
    pub enabled: bool,
    pub recursive: bool,
    pub monitor: bool,
    pub schedule: Schedule,
    /// Keep the indexed entries while the folder is unavailable (an
    /// unplugged drive or an unmounted share), instead of dropping them.
    pub keep_offline: bool,
    /// Added automatically because a removable drive was plugged in.
    pub removable: bool,
}
impl Default for RootConfig {
    fn default() -> Self {
        Self {
            path: RawPath::default(),
            enabled: true,
            recursive: true,
            monitor: true,
            schedule: Schedule::default(),
            keep_offline: false,
            removable: false,
        }
    }
}
impl RootConfig {
    pub fn new(path: &Path) -> Self {
        Self {
            path: RawPath::new(path),
            ..Self::default()
        }
    }
    /// A root for a folder the user picked. Network folders get a 30-minute
    /// rescan instead of the daily one: inotify only reports changes made
    /// through this computer, not by others on the share.
    pub fn for_folder(path: &Path) -> Self {
        let mut root = Self::new(path);
        if crate::mounts::network_fs(path).is_some() {
            root.schedule = Schedule::Interval { seconds: 1800 };
        }
        root
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub roots: Vec<RootConfig>,
    pub excluded_folders: Vec<RawPath>,
    /// Folders indexed even though they are inside an excluded folder (or
    /// hidden, or a snapshot folder).
    pub included_folders: Vec<RawPath>,
    /// Skip snapshot and system image folders (see `SNAPSHOT_FOLDERS`),
    /// which hold whole extra copies of other files.
    pub exclude_snapshots: bool,
    pub excluded_files: Vec<String>,
    pub included_files: Vec<String>,
    pub exclude_hidden: bool,
    /// Whether `excluded_file_types` applies; the list is kept when off.
    pub exclude_file_types: bool,
    /// Lower-case extensions without the dot (`tmp`, `tar.gz`).
    pub excluded_file_types: Vec<String>,
    /// Index removable drives when they are plugged in (their entries are
    /// kept while unplugged).
    pub removable_drives: bool,
    pub event_debounce_ms: u64,
    pub fallback_rescan_seconds: u64,
}
impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            excluded_folders: Vec::new(),
            included_folders: Vec::new(),
            exclude_snapshots: true,
            excluded_files: Vec::new(),
            included_files: Vec::new(),
            exclude_hidden: false,
            exclude_file_types: true,
            excluded_file_types: DEFAULT_EXCLUDED_FILE_TYPES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            removable_drives: false,
            event_debounce_ms: 200,
            fallback_rescan_seconds: 1800,
        }
    }
}
/// File types most people never search for: temporary files and partial
/// downloads, editor swap files and compiler output. Users can change the list.
pub const DEFAULT_EXCLUDED_FILE_TYPES: &[&str] = &[
    "class",
    "crdownload",
    "gch",
    "lo",
    "o",
    "part",
    "pch",
    "pyc",
    "pyo",
    "rlib",
    "rmeta",
    "swo",
    "swp",
    "temp",
    "tmp",
];
/// Turns user input such as `*.TMP`, `.tmp` or ` tmp ` into `tmp`.
pub fn normalize_file_type(input: &str) -> Option<String> {
    let ext = input
        .trim()
        .trim_start_matches('*')
        .trim_start_matches('.')
        .to_lowercase();
    (!ext.is_empty()
        && ext.len() <= 32
        && !ext.starts_with('.')
        && !ext.ends_with('.')
        && !ext.contains("..")
        && !ext
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '\0' | '*' | '?' | '[' | ']' | ';')))
    .then_some(ext)
}
impl IndexConfig {
    pub fn from_roots(roots: &[PathBuf]) -> Self {
        Self {
            roots: roots.iter().map(|p| RootConfig::new(p)).collect(),
            ..Self::default()
        }
    }
    pub fn paths(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.path())
            .collect()
    }
    pub fn validate(&self) -> Result<()> {
        if self.roots.len() > 256 {
            bail!("At most 256 indexing roots are supported");
        }
        if !(20..=60_000).contains(&self.event_debounce_ms) {
            bail!("Event debounce must be between 20 and 60000 ms");
        }
        if !(5..=86400).contains(&self.fallback_rescan_seconds) {
            bail!("Fallback rescan interval must be between 5 seconds and 1 day");
        }
        for (i, root) in self.roots.iter().enumerate() {
            validate_path(&root.path)?;
            root.schedule.validate()?;
            let path = root.path.path();
            if ["/proc", "/sys", "/dev", "/run"]
                .iter()
                .any(|p| path.starts_with(p))
            {
                bail!("Pseudo/runtime roots cannot be indexed: {}", path.display());
            }
            for other in &self.roots[..i] {
                let other = other.path.path();
                if path.starts_with(&other) || other.starts_with(&path) {
                    bail!(
                        "Index roots overlap: {} and {}",
                        path.display(),
                        other.display()
                    );
                }
            }
        }
        for path in self.excluded_folders.iter().chain(&self.included_folders) {
            validate_path(path)?;
        }
        if self.excluded_file_types.len() > 1024 {
            bail!("At most 1024 excluded file types are allowed");
        }
        for ext in &self.excluded_file_types {
            if normalize_file_type(ext).as_deref() != Some(ext.as_str()) {
                bail!("Invalid file type: {ext} (use letters such as tmp or tar.gz)");
            }
        }
        PatternRules::new(self)?;
        Ok(())
    }
}
fn validate_path(path: &RawPath) -> Result<()> {
    if path.0.contains(&0) || !path.path().is_absolute() {
        bail!("Folder paths must be absolute and contain no NUL bytes");
    }
    if path.path().components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        bail!("Folder paths must be normalized (no . or .. components)");
    }
    Ok(())
}

/// Snapshot and system image folders, skipped unless the user turns
/// `exclude_snapshots` off: each holds whole extra copies of other files.
/// Absolute paths, and folder name sequences that match anywhere below a
/// root. A root chosen inside one of them is still indexed.
pub const SNAPSHOT_PATHS: &[&str] = &[
    // Mounted snap packages (Ubuntu and others).
    "/snap",
    // The Nix store (NixOS, Nix on other distributions).
    "/nix/store",
    // Flatpak's object store; installed apps stay searchable.
    "/var/lib/flatpak/repo",
];
pub const SNAPSHOT_FOLDERS: &[&str] = &[
    // Snapper (openSUSE, Fedora and others with Btrfs).
    ".snapshots",
    // ZFS snapshots when snapdir=visible.
    ".zfs",
    // Timeshift on any drive, rsync and Btrfs modes.
    "timeshift/snapshots",
    "timeshift-btrfs/snapshots",
    // Rootless Podman image layers.
    ".local/share/containers/storage",
    ".local/share/flatpak/repo",
];
/// The last folder names of `SNAPSHOT_FOLDERS`, checked before the rest.
const SNAPSHOT_NAMES: &[&[u8]] = &[b".snapshots", b".zfs", b"snapshots", b"storage", b"repo"];

/// Compiled once per settings change, never once per directory entry.
pub struct PatternRules {
    excluded: GlobSet,
    included: GlobSet,
    include_active: bool,
    folders: Vec<PathBuf>,
    /// Folders indexed although an exclusion would skip them.
    exceptions: Vec<PathBuf>,
    snapshots: bool,
    hidden: bool,
    /// `.ext` suffixes, lower case; empty when file types are not excluded.
    types: Vec<Vec<u8>>,
}
impl PatternRules {
    pub fn new(config: &IndexConfig) -> Result<Self> {
        fn compile(patterns: &[String]) -> Result<GlobSet> {
            if patterns.len() > 1024 {
                bail!("At most 1024 patterns are allowed");
            }
            let mut builder = GlobSetBuilder::new();
            for p in patterns {
                if p.is_empty() || p.len() > 4096 {
                    bail!("Patterns must contain 1 to 4096 bytes");
                }
                builder.add(
                    GlobBuilder::new(p)
                        .literal_separator(true)
                        .backslash_escape(false)
                        .build()
                        .with_context(|| format!("Invalid file pattern: {p}"))?,
                );
            }
            Ok(builder.build()?)
        }
        Ok(Self {
            excluded: compile(&config.excluded_files)?,
            included: compile(&config.included_files)?,
            include_active: !config.included_files.is_empty(),
            folders: config.excluded_folders.iter().map(RawPath::path).collect(),
            exceptions: config.included_folders.iter().map(RawPath::path).collect(),
            snapshots: config.exclude_snapshots,
            hidden: config.exclude_hidden,
            types: if config.exclude_file_types {
                config
                    .excluded_file_types
                    .iter()
                    .map(|ext| format!(".{}", ext.to_lowercase()).into_bytes())
                    .collect()
            } else {
                Vec::new()
            },
        })
    }
    /// Whether the scan enters (or, for a file, keeps) `path`. A folder
    /// that leads to an exception is entered even when excluded, but is not
    /// itself indexed (`index_folder`).
    pub fn traverse(&self, path: &Path, root: &Path) -> bool {
        !self.excluded(path, root) || self.leads_to_exception(path)
    }
    /// Whether a folder the scan entered is indexed itself.
    pub fn index_folder(&self, path: &Path, root: &Path) -> bool {
        !self.excluded(path, root) || self.within_exception(path)
    }
    /// Excluded by a folder rule, the hidden rule or the snapshot rule,
    /// unless inside an exception. Snapshot names are only checked on the
    /// path itself: scans never enter an excluded folder, so its contents
    /// are not reached.
    fn excluded(&self, path: &Path, root: &Path) -> bool {
        if self.within_exception(path) {
            return false;
        }
        self.folders.iter().any(|p| path.starts_with(p))
            || (self.hidden
                && path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .components()
                    .any(|part| part.as_os_str().as_bytes().starts_with(b".")))
            || (self.snapshots && snapshot_folder(path, root))
    }
    fn within_exception(&self, path: &Path) -> bool {
        self.exceptions.iter().any(|e| path.starts_with(e))
    }
    fn leads_to_exception(&self, path: &Path) -> bool {
        self.exceptions.iter().any(|e| e.starts_with(path))
    }
    pub fn include_file(&self, path: &Path, root: &Path) -> bool {
        let name = Path::new(path.file_name().unwrap_or(path.as_os_str()));
        if self.excluded_type(name.as_os_str().as_bytes()) {
            return false;
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        !(self.excluded.is_match(name) || self.excluded.is_match(relative))
            && (!self.include_active
                || self.included.is_match(name)
                || self.included.is_match(relative))
    }
}

/// Whether `path` is a snapshot or system image folder (`SNAPSHOT_PATHS`,
/// `SNAPSHOT_FOLDERS`) below `root`.
fn snapshot_folder(path: &Path, root: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    if SNAPSHOT_PATHS
        .iter()
        .any(|p| crate::table::within(bytes, p.as_bytes()) && !root.starts_with(p))
    {
        return true;
    }
    let name = path.file_name().map_or(&b""[..], |n| n.as_bytes());
    SNAPSHOT_NAMES.contains(&name)
        && path != root
        && SNAPSHOT_FOLDERS.iter().any(|folder| {
            path.ends_with(folder)
                && path
                    .strip_prefix(root)
                    .is_ok_and(|below| below.components().count() >= 1)
        })
}

impl PatternRules {
    /// Whether a file name ends in an excluded type. A name that is only the
    /// extension (`.tmp`) is not a file of that type.
    fn excluded_type(&self, name: &[u8]) -> bool {
        !self.types.is_empty()
            && memchr::memchr(b'.', name).is_some()
            && self.types.iter().any(|ext| {
                name.len() > ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub search_delay_ms: u64,
    pub tray_enabled: bool,
    pub close_to_tray: bool,
    pub start_hidden: bool,
    pub global_shortcut: String,
    /// Start with an empty search box each time the window opens from the
    /// tray or the shortcut. Off: the last search stays, selected.
    pub clear_search_on_open: bool,
    /// Keep the result list empty until something is searched (a term or a
    /// type filter), like Everything's "Hide results when search is empty".
    pub hide_results_until_search: bool,
    pub history_enabled: bool,
    pub history_days: u32,
    pub history_limit: usize,
    /// User-defined type filters (Options → Filters), shown after the
    /// built-in ones beside the search box.
    pub filters: Vec<Filter>,
}
/// A named search that the filter drop-down adds to whatever is typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub name: String,
    pub query: String,
}
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            search_delay_ms: 35,
            tray_enabled: true,
            close_to_tray: true,
            start_hidden: false,
            global_shortcut: "Ctrl+Alt+Space".into(),
            clear_search_on_open: true,
            hide_results_until_search: false,
            history_enabled: true,
            history_days: 90,
            history_limit: 200,
            filters: Vec::new(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub name: String,
    pub query: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub revision: u64,
    pub index: IndexConfig,
    pub ui: UiConfig,
    pub bookmarks: Vec<Bookmark>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            index: IndexConfig::default(),
            ui: UiConfig::default(),
            bookmarks: Vec::new(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("Unsupported settings version {}", self.version);
        }
        self.index.validate()?;
        if !["system", "dark", "light"].contains(&self.ui.theme.as_str()) {
            bail!("Theme must be system, dark or light");
        }
        if self.ui.search_delay_ms > 1000 {
            bail!("Search delay must be 0–1000 ms");
        }
        if !self.ui.global_shortcut.trim().is_empty() {
            let chord = crate::shortcuts::Chord::parse(&self.ui.global_shortcut)?;
            if !(chord.ctrl || chord.alt || chord.super_key || chord.key.starts_with('F')) {
                bail!("Global shortcuts need Ctrl, Alt, Super or a function key");
            }
        }
        if self.ui.history_limit > 10_000 || self.ui.history_days > 3650 {
            bail!("History limits are too large");
        }
        if self.ui.filters.len() > 200 {
            bail!("At most 200 filters are allowed");
        }
        for filter in &self.ui.filters {
            if filter.name.trim().is_empty() || filter.name.len() > 64 {
                bail!("Filter needs a name of at most 64 bytes");
            }
            if filter.query.trim().is_empty() {
                bail!("Filter “{}” needs a search", filter.name);
            }
            crate::query::Query::parse(&filter.query)?;
        }
        if self.bookmarks.len() > 1000 {
            bail!("At most 1000 bookmarks are allowed");
        }
        for bookmark in &self.bookmarks {
            if bookmark.name.trim().is_empty() || bookmark.name.len() > 256 {
                bail!("Bookmark needs a name of at most 256 bytes");
            }
            crate::query::Query::parse(&bookmark.query)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    pub path: PathBuf,
}
impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let default = crate::store::default_dir();
        let is_default = fs::canonicalize(&default).unwrap_or(default) == data_dir;
        let path = if is_default {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                });
            base.join("file-minnow/settings.json")
        } else {
            data_dir.join("settings.json")
        };
        Self { path }
    }
    pub fn load(&self) -> Result<Settings> {
        if !self.path.exists() {
            return Ok(Settings::default());
        }
        let bytes = fs::read(&self.path)?;
        if bytes.len() > 4 * 1024 * 1024 {
            bail!("Settings file exceeds 4 MiB");
        }
        let settings: Settings = serde_json::from_slice(&bytes)
            .context("Settings are unreadable; original file has been preserved")?;
        settings.validate()?;
        Ok(settings)
    }
    /// Compare-and-swap avoids silently overwriting edits from another window/client.
    pub fn save(&self, expected_revision: u64, mut settings: Settings) -> Result<Settings> {
        settings.validate()?;
        let parent = self.path.parent().context("Settings path has no parent")?;
        crate::store::prepare(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("lock"))?;
        lock.lock_exclusive()?;
        let old = self.load()?;
        if old.revision != expected_revision {
            bail!("Settings changed elsewhere. Reload before saving.");
        }
        settings.revision = old
            .revision
            .checked_add(1)
            .context("Settings revision overflow")?;
        let bytes = serde_json::to_vec_pretty(&settings)?;
        atomic_write(&self.path, &bytes)?;
        Ok(settings)
    }
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("No parent directory")?;
    crate::store::prepare(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temp = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Explicit roots override only the root selection, preserving matching policies.
/// A previous cache is migrated once; an explicitly saved empty root list stays empty.
pub fn open_for_roots(roots: &[PathBuf], dir: &Path) -> Result<(ConfigStore, Settings)> {
    let store = ConfigStore::for_data_dir(dir);
    let mut settings = store.load()?;
    if !roots.is_empty() {
        let roots = crate::index::normalize_roots(roots)?;
        settings.index.roots = roots
            .iter()
            .map(|p| {
                settings
                    .index
                    .roots
                    .iter()
                    .find(|r| r.path.path() == *p)
                    .cloned()
                    .unwrap_or_else(|| RootConfig::new(p))
            })
            .collect();
    } else if !store.path.exists()
        && let Ok(snapshot) = crate::store::load(dir)
    {
        settings.index.roots = crate::index::root_paths(&snapshot.roots)
            .iter()
            .map(|p| RootConfig::new(p))
            .collect();
    }
    settings.validate()?;
    Ok((store, settings))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_paths_round_trip() {
        for p in [
            RawPath(b"/tmp/hello world".to_vec()),
            RawPath(b"/tmp/invalid-\xff".to_vec()),
        ] {
            assert_eq!(
                p,
                serde_json::from_str::<RawPath>(&serde_json::to_string(&p).unwrap()).unwrap()
            );
        }
    }
    #[test]
    fn revisions_and_atomic_settings() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join("settings.json"));
        let mut s = store.load().unwrap();
        s.ui.theme = "light".into();
        let saved = store.save(0, s.clone()).unwrap();
        assert_eq!(saved.revision, 1);
        assert!(store.save(0, s).is_err());
        assert_eq!(store.load().unwrap().ui.theme, "light");
        fs::write(&store.path, b"invalid").unwrap();
        assert!(store.load().is_err());
        assert!(store.save(1, saved).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), b"invalid");
    }
    #[test]
    fn daily_and_interval_schedules() {
        let s = Schedule::Daily { hour: 3, minute: 0 };
        assert_eq!(s.next_after_in(0, 0), Some(10800));
        assert_eq!(s.next_after_in(10800, 0), Some(97200));
        // UTC+3: 03:00 local is midnight UTC.
        assert_eq!(s.next_after_in(0, 3 * 3600), Some(86400));
        assert_eq!(s.next_after_in(86400 - 1, 3 * 3600), Some(86400));
        assert_eq!(
            Schedule::Interval { seconds: 30 }.next_after(100),
            Some(130)
        );
        assert_eq!(Schedule::Never.next_after(100), None);
    }
    #[test]
    fn exclusions_and_include_patterns() {
        let cfg = IndexConfig {
            excluded_folders: vec![RawPath(b"/data/cache".to_vec())],
            excluded_files: vec!["*.tmp".into()],
            included_files: vec!["*.rs".into(), "*.tmp".into()],
            exclude_hidden: true,
            ..Default::default()
        };
        let rules = PatternRules::new(&cfg).unwrap();
        let root = Path::new("/data");
        assert!(!rules.traverse(Path::new("/data/cache/x"), root));
        assert!(!rules.traverse(Path::new("/data/.git/x"), root));
        assert!(rules.include_file(Path::new("/data/src/main.rs"), root));
        assert!(!rules.include_file(Path::new("/data/a.tmp"), root));
        assert!(!rules.include_file(Path::new("/data/a.txt"), root));
    }
    #[test]
    fn excluded_file_types() {
        let mut cfg = IndexConfig {
            excluded_file_types: vec!["tmp".into(), "tar.gz".into()],
            ..Default::default()
        };
        cfg.validate().unwrap();
        let rules = PatternRules::new(&cfg).unwrap();
        let root = Path::new("/data");
        for excluded in ["a.tmp", "B.TMP", "x.tar.gz"] {
            assert!(
                !rules.include_file(&root.join(excluded), root),
                "{excluded}"
            );
        }
        for kept in ["a.tmpl", "tmp", ".tmp", "a.gz", "x.tar.gz.txt"] {
            assert!(rules.include_file(&root.join(kept), root), "{kept}");
        }
        // Folders are never excluded by type.
        assert!(rules.traverse(Path::new("/data/build.tmp"), root));
        cfg.exclude_file_types = false;
        assert!(
            PatternRules::new(&cfg)
                .unwrap()
                .include_file(&root.join("a.tmp"), root)
        );
        // Defaults apply to settings written before the option existed.
        let old: IndexConfig = serde_json::from_str(r#"{"exclude_hidden": true}"#).unwrap();
        assert!(old.exclude_file_types && old.excluded_file_types.contains(&"tmp".into()));
        assert_eq!(normalize_file_type(" *.TMP "), Some("tmp".into()));
        assert_eq!(normalize_file_type(".Tar.GZ"), Some("tar.gz".into()));
        for bad in ["", "*", "a/b", "a b", "a..b", "a.", "*.?"] {
            assert_eq!(normalize_file_type(bad), None, "{bad}");
        }
        cfg.excluded_file_types = vec!["TMP".into()];
        assert!(cfg.validate().is_err());
    }
    #[test]
    fn snapshot_folders_and_exceptions() {
        let rules = PatternRules::new(&IndexConfig::default()).unwrap();
        let root = Path::new("/");
        for skipped in [
            "/snap",
            "/snap/firefox/123",
            "/nix/store",
            "/timeshift/snapshots",
            "/media/alex/Backup/timeshift/snapshots",
            "/.snapshots",
            "/home/.snapshots",
            "/tank/data/.zfs",
            "/home/alex/.local/share/containers/storage",
            "/var/lib/flatpak/repo",
        ] {
            assert!(!rules.traverse(Path::new(skipped), root), "{skipped}");
        }
        for kept in [
            "/snapshots",
            "/home/alex/snap",
            "/home/alex/snapshots",
            "/timeshift",
            "/home/alex/projects/repo",
            "/var/lib/flatpak/app",
            "/nix",
        ] {
            assert!(rules.traverse(Path::new(kept), root), "{kept}");
        }
        // A root chosen inside one is indexed.
        assert!(rules.traverse(Path::new("/snap/app/1"), Path::new("/snap/app")));
        assert!(rules.traverse(
            Path::new("/timeshift/snapshots/x"),
            Path::new("/timeshift/snapshots")
        ));
        let off = IndexConfig {
            exclude_snapshots: false,
            ..Default::default()
        };
        assert!(
            PatternRules::new(&off)
                .unwrap()
                .traverse(Path::new("/snap"), root)
        );
        // Exceptions inside an excluded or hidden folder: the way there is
        // walked but not indexed, the exception and its contents are.
        let cfg = IndexConfig {
            excluded_folders: vec![RawPath(b"/home/a/cache".to_vec())],
            included_folders: vec![
                RawPath(b"/home/a/cache/keep/this".to_vec()),
                RawPath(b"/home/a/.config/app".to_vec()),
            ],
            exclude_hidden: true,
            ..Default::default()
        };
        let rules = PatternRules::new(&cfg).unwrap();
        let root = Path::new("/home/a");
        let check = |path: &str| {
            let path = Path::new(path);
            (rules.traverse(path, root), rules.index_folder(path, root))
        };
        assert_eq!(check("/home/a/cache"), (true, false));
        assert_eq!(check("/home/a/cache/keep"), (true, false));
        assert_eq!(check("/home/a/cache/other"), (false, false));
        assert_eq!(check("/home/a/cache/keep/file.txt"), (false, false));
        assert_eq!(check("/home/a/cache/keep/this"), (true, true));
        assert_eq!(check("/home/a/cache/keep/this/deep/x.txt"), (true, true));
        assert_eq!(check("/home/a/.config"), (true, false));
        assert_eq!(check("/home/a/.config/app/.hidden"), (true, true));
        assert_eq!(check("/home/a/.config/other"), (false, false));
        assert_eq!(check("/home/a/docs"), (true, true));
    }
}
