use crate::{
    config::{ConfigStore, IndexConfig, PatternRules, RawPath, RootConfig, Settings},
    index::{self, Snapshot},
    mounts, store,
    watch::{Signal, Watcher},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub phase: String,
    pub scanning: bool,
    pub paused: bool,
    pub visited: usize,
    pub entries: usize,
    pub generation: u64,
    pub revision: u64,
    pub last_scan: u64,
    pub errors: Vec<String>,
    pub error_count: usize,
    pub watch_errors: Vec<String>,
    /// Folders currently monitored for changes.
    #[serde(default)]
    pub watched: usize,
    /// Roots that keep their entries and are unavailable right now
    /// (unplugged drives, unmounted shares).
    #[serde(default)]
    pub offline: Vec<String>,
    pub message: String,
}
pub struct State {
    pub snapshot: Arc<Snapshot>,
    pub status: Status,
}
pub type Shared = Arc<RwLock<State>>;
type Callback = Box<dyn Fn() + Send + Sync>;
#[derive(Default)]
struct Signals {
    version: Mutex<u64>,
    changed: Condvar,
    callbacks: Mutex<Vec<Callback>>,
    closed: AtomicBool,
}
impl Signals {
    fn notify(&self, state: &Shared) {
        let revision = {
            let mut s = state.write().unwrap();
            s.status.revision += 1;
            s.status.revision
        };
        *self.version.lock().unwrap() = revision;
        self.changed.notify_all();
        for f in self.callbacks.lock().unwrap().iter() {
            f();
        }
    }
    fn wait(&self, after: u64, timeout: Duration) {
        let version = self.version.lock().unwrap();
        let _guard = self
            .changed
            .wait_timeout_while(version, timeout, |v| {
                *v <= after && !self.closed.load(Ordering::Relaxed)
            })
            .unwrap();
    }
}
enum Command {
    Settings(Box<Settings>),
    Rescan(Option<PathBuf>),
    Pause(bool),
    Event(Vec<PathBuf>),
    /// The mount table changed (a drive was plugged in or removed).
    Mounts,
    Stop,
}
#[derive(Clone)]
pub struct Handle {
    pub state: Shared,
    settings: Arc<RwLock<Settings>>,
    config: ConfigStore,
    /// Files opened or launched through the app, kept beside the settings.
    pub counts: Arc<crate::run_counts::RunCounts>,
    commands: mpsc::SyncSender<Command>,
    wake: thread::Thread,
    signals: Arc<Signals>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    interrupt: Arc<AtomicBool>,
}
impl Handle {
    /// Records opens or launches, one increment per path.
    pub fn record_runs(&self, paths: &[std::path::PathBuf]) {
        self.counts.record_all(paths);
    }
    fn send(&self, command: Command) -> Result<()> {
        self.commands.send(command).context("Indexer has stopped")?;
        self.wake.unpark();
        Ok(())
    }
    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }
    pub fn apply_settings(&self, settings: Settings) -> Result<Settings> {
        let saved = self.config.save(settings.revision, settings)?;
        self.interrupt.store(true, Ordering::Relaxed);
        self.send(Command::Settings(Box::new(saved.clone())))?;
        Ok(saved)
    }
    pub fn rescan(&self, root: Option<PathBuf>) -> Result<()> {
        self.send(Command::Rescan(root))
    }
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.paused.store(paused, Ordering::Relaxed);
        self.interrupt.store(true, Ordering::Relaxed);
        self.send(Command::Pause(paused))
    }
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.interrupt.store(true, Ordering::Relaxed);
        let _ = self.commands.try_send(Command::Stop);
        self.wake.unpark();
        self.signals.closed.store(true, Ordering::Relaxed);
        self.signals.changed.notify_all();
    }
    pub fn status(&self) -> Status {
        self.state.read().unwrap().status.clone()
    }
    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        self.signals.callbacks.lock().unwrap().push(Box::new(f));
    }
    pub fn wait_for_change(&self, after: u64, timeout: Duration) -> Status {
        self.signals.wait(after, timeout);
        self.status()
    }
    pub fn pulse(&self) {
        self.signals.notify(&self.state);
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}
pub struct Runtime {
    pub state: Shared,
    pub handle: Handle,
    worker: Option<thread::JoinHandle<()>>,
}
impl Runtime {
    pub fn start(roots: Vec<PathBuf>, dir: PathBuf, interval: Duration) -> Result<Self> {
        let dir = store::prepare(&dir)?;
        let config = ConfigStore::for_data_dir(&dir);
        let mut settings = config.load()?;
        settings.index.roots = roots
            .iter()
            .map(|p| {
                let mut r = crate::config::RootConfig::new(p);
                r.schedule = crate::config::Schedule::Interval {
                    seconds: interval.as_secs().max(1),
                };
                r
            })
            .collect();
        Self::configured(settings, dir, config)
    }
    pub fn configured(mut settings: Settings, dir: PathBuf, config: ConfigStore) -> Result<Self> {
        settings.validate()?;
        let dir = store::prepare(&dir)?;
        let lock = store::writer_lock(&dir)?;
        let disk = config.load()?;
        if !config.path.exists() || disk != settings {
            settings = config.save(settings.revision, settings)?;
        }
        let state = Arc::new(RwLock::new(State {
            snapshot: Arc::new(Snapshot::default()),
            status: Status {
                scanning: true,
                phase: "loading".into(),
                message: "Loading index…".into(),
                ..Status::default()
            },
        }));
        let settings = Arc::new(RwLock::new(settings));
        let signals = Arc::new(Signals::default());
        let paused = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(2048);
        let mut worker = Worker {
            dir,
            state: state.clone(),
            settings: settings.clone(),
            config: config.clone(),
            signals: signals.clone(),
            paused: paused.clone(),
            stop: stop.clone(),
            interrupt: interrupt.clone(),
            rx,
            tx: tx.clone(),
        };
        let thread = thread::spawn(move || {
            let _lock = lock;
            if let Err(error) = worker.run() {
                worker.state.write().unwrap().status.message =
                    format!("Indexer stopped: {error:#}");
                worker.state.write().unwrap().status.phase = "error".into();
                worker.state.write().unwrap().status.scanning = false;
                worker.signals.notify(&worker.state);
            }
        });
        let counts = Arc::new(crate::run_counts::RunCounts::load(
            crate::run_counts::RunCounts::path_for(&config.path),
        ));
        let handle = Handle {
            state: state.clone(),
            settings,
            config,
            counts,
            commands: tx,
            wake: thread.thread().clone(),
            signals,
            paused,
            stop,
            interrupt,
        };
        Ok(Self {
            state,
            handle,
            worker: Some(thread),
        })
    }
    pub fn rescan(&self) {
        let _ = self.handle.rescan(None);
    }
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        let mut settings = self.handle.settings();
        settings.index.roots = roots
            .iter()
            .map(|p| crate::config::RootConfig::new(p))
            .collect();
        if let Err(e) = self.handle.apply_settings(settings) {
            self.state.write().unwrap().status.message = e.to_string();
            self.handle.pulse();
        }
    }
    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        self.handle.on_change(f);
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct EventPolicy {
    config: IndexConfig,
    rules: PatternRules,
}
impl EventPolicy {
    fn new(config: &IndexConfig) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            rules: PatternRules::new(config)?,
        })
    }
    fn accepts(&self, path: &Path, dir: &Path) -> bool {
        !index::excluded(path, dir)
            && self
                .config
                .roots
                .iter()
                .filter(|r| r.enabled && r.monitor)
                .any(|r| {
                    let root = r.path.path();
                    path.starts_with(&root)
                        && self.rules.traverse(path, &root)
                        && (r.recursive
                            || path
                                .strip_prefix(root)
                                .map(|p| p.components().count() <= 1)
                                .unwrap_or(false))
                })
    }
}
/// glibc keeps freed heap memory for reuse; after replacing a large index,
/// return it to the system so idle memory reflects what is actually in use.
fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases unused heap pages.
    unsafe {
        libc::malloc_trim(0);
    }
}
/// Wakes the indexer when the mount table changes. `/proc/self/mountinfo`
/// signals a change through poll, so the thread sleeps until then.
fn watch_mounts(tx: mpsc::SyncSender<Command>, wake: thread::Thread, stop: Arc<AtomicBool>) {
    use std::{io::Read, os::fd::AsRawFd};
    let Ok(mut file) = std::fs::File::open("/proc/self/mountinfo") else {
        return;
    };
    let _ = thread::Builder::new()
        .name("file-minnow-mounts".into())
        .spawn(move || {
            let mut poll = libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLPRI,
                revents: 0,
            };
            let mut sink = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: `poll` is one initialized pollfd.
                let ready = unsafe { libc::poll(&mut poll, 1, 2000) };
                if ready > 0 && poll.revents != 0 {
                    // Reading re-arms the notification.
                    use std::io::Seek;
                    let _ = file.seek(std::io::SeekFrom::Start(0));
                    sink.clear();
                    let _ = file.read_to_end(&mut sink);
                    if tx.try_send(Command::Mounts).is_ok() {
                        wake.unpark();
                    }
                }
            }
        });
}
impl Worker {
    /// Adds newly plugged removable drives as roots (when enabled) and
    /// rescans roots that came back. Returns Some when the roots changed.
    fn mounts_changed(
        &self,
        config: &mut IndexConfig,
        changed: &mut BTreeSet<PathBuf>,
    ) -> Option<()> {
        let mounts = mounts::current();
        let was_offline = self.state.read().unwrap().status.offline.clone();
        for root in config.roots.iter().filter(|r| r.enabled && r.keep_offline) {
            let path = root.path.path();
            if was_offline.contains(&root.path.display()) && !index::is_offline(&path) {
                changed.insert(path);
            }
        }
        if !config.removable_drives {
            return None;
        }
        let mut settings = self.settings.read().unwrap().clone();
        let mut added = Vec::new();
        for point in mounts::removable(&mounts) {
            let covered = settings
                .index
                .roots
                .iter()
                .any(|r| point.starts_with(r.path.path()) || r.path.path().starts_with(&point));
            if covered || index::excluded(&point, &self.dir) {
                continue;
            }
            settings.index.roots.push(RootConfig {
                keep_offline: true,
                removable: true,
                ..RootConfig::new(&point)
            });
            added.push(point);
        }
        if added.is_empty() || settings.validate().is_err() {
            return None;
        }
        match self.config.save(settings.revision, settings) {
            Ok(saved) => {
                *self.settings.write().unwrap() = saved;
                *config = self.settings.read().unwrap().index.clone();
                changed.extend(added);
                Some(())
            }
            Err(e) => {
                self.state.write().unwrap().status.message =
                    format!("Cannot add removable drive: {e}");
                None
            }
        }
    }
}
/// When `next` only adds roots, excluded paths or exception folders to
/// `config`, the paths whose index entries change; otherwise none.
fn added_paths(config: &IndexConfig, next: &IndexConfig) -> Vec<PathBuf> {
    let mut trimmed = next.clone();
    trimmed.roots.retain(|r| config.roots.contains(r));
    trimmed
        .excluded_folders
        .retain(|p| config.excluded_folders.contains(p));
    trimmed
        .included_folders
        .retain(|p| config.included_folders.contains(p));
    if trimmed != *config {
        return Vec::new();
    }
    let roots = next
        .roots
        .iter()
        .filter(|r| r.enabled && !config.roots.contains(r))
        .map(|r| r.path.path());
    let excluded = next
        .excluded_folders
        .iter()
        .filter(|p| !config.excluded_folders.contains(p))
        .map(RawPath::path);
    let included = next
        .included_folders
        .iter()
        .filter(|p| !config.included_folders.contains(p))
        .map(RawPath::path);
    roots.chain(excluded).chain(included).collect()
}
fn config_monitors(config: &IndexConfig) -> bool {
    config.roots.iter().any(|r| r.enabled && r.monitor)
}
fn reset_deadlines(config: &IndexConfig, deadlines: &mut BTreeMap<PathBuf, u64>) {
    deadlines.clear();
    for root in config.roots.iter().filter(|r| r.enabled) {
        if let Some(next) = root.schedule.next_after(index::now()) {
            deadlines.insert(root.path.path(), next);
        }
    }
}
/// After a start-up catch-up, how long a scheduled rescan that was missed
/// while File Minnow was not running waits, so it does not compete with
/// the rest of the login.
const MISSED_RESCAN_DELAY: u64 = 600;
fn catch_up_missed_rescans(
    config: &IndexConfig,
    snapshot: &Snapshot,
    deadlines: &mut BTreeMap<PathBuf, u64>,
) {
    let time = index::now();
    for root in config.roots.iter().filter(|r| r.enabled) {
        let walked = snapshot.walked_at(&root.path.0);
        if root
            .schedule
            .next_after(walked)
            .is_some_and(|due| due <= time)
        {
            deadlines.insert(root.path.path(), time + MISSED_RESCAN_DELAY);
        }
    }
}
/// How long small updates may stay unsaved. After a crash, start-up finds
/// them again through their folders' changed times (and files changed in
/// place at the next full walk), so they are delayed, never lost.
const SAVE_DELAY: Duration = Duration::from_secs(300);
struct Worker {
    dir: PathBuf,
    state: Shared,
    settings: Arc<RwLock<Settings>>,
    config: ConfigStore,
    signals: Arc<Signals>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    interrupt: Arc<AtomicBool>,
    rx: mpsc::Receiver<Command>,
    tx: mpsc::SyncSender<Command>,
}
impl Worker {
    fn run(&mut self) -> Result<()> {
        let dir = &self.dir;
        let state = &self.state;
        let signals = &self.signals;
        let mut config = self.settings.read().unwrap().index.clone();
        if store::remove_legacy(dir).unwrap_or(false) {
            state.write().unwrap().status.message = "Upgrading the index format…".into();
        }
        let mut current = Arc::new(match store::load(dir) {
            Ok(s) => s,
            Err(e) => {
                store::quarantine(dir).context("Cannot preserve unreadable cache")?;
                state.write().unwrap().status.message = format!("Rebuilding unreadable cache: {e}");
                Snapshot::default()
            }
        });
        if index::root_paths(&current.roots) == config.paths() {
            let mut s = state.write().unwrap();
            s.snapshot = current.clone();
            s.status.entries = current.entries.len();
            s.status.generation = current.generation;
        }
        let overflow = Arc::new(AtomicBool::new(false));
        let over = overflow.clone();
        let wake = thread::current();
        let data = dir.to_path_buf();
        let tx = self.tx.clone();
        let event_policy = Arc::new(RwLock::new(EventPolicy::new(&config)?));
        let policy = event_policy.clone();
        let watcher = Watcher::new(move |signal| {
            match signal {
                Signal::Overflow => over.store(true, Ordering::Relaxed),
                Signal::Changed(paths) => {
                    let policy = policy.read().unwrap();
                    let paths: Vec<_> = paths
                        .into_iter()
                        .filter(|p| policy.accepts(p, &data))
                        .collect();
                    if paths.is_empty() {
                        return;
                    }
                    if tx.try_send(Command::Event(paths)).is_err() {
                        over.store(true, Ordering::Relaxed);
                    }
                }
            }
            wake.unpark();
        })
        .ok();
        if watcher.is_none() && config_monitors(&config) {
            state.write().unwrap().status.watch_errors = vec![
                "Native monitoring unavailable; periodic fallback scans are active".to_owned(),
            ];
        }
        watch_mounts(self.tx.clone(), thread::current(), self.stop.clone());
        let journal = crate::journal::Journal::new(dir);
        let _ = journal.trim(index::now() as i64);
        state.write().unwrap().status.offline = index::offline_roots(&config);
        let mut sweep = 0_u32;
        let mut deadlines = BTreeMap::new();
        reset_deadlines(&config, &mut deadlines);
        // A saved index made with the same settings only needs its changed
        // folders read again (`index::quick_scan`); otherwise walk everything.
        let mut quick = index::root_paths(&current.roots) == config.paths()
            && !current.entries.is_empty()
            && current.saved_at > 0
            && current.config == config.fingerprint();
        let mut full = !quick;
        let mut changed = BTreeSet::new();
        let mut dirty: Option<Instant> = None;
        // Changes since the last save. Full scans save at once; smaller
        // updates are batched (like Everything) and saved on exit.
        let mut unsaved: Option<Instant> = None;
        // Cost of the last incremental update. During bursts of changes the
        // next update waits ten times as long, keeping background CPU low.
        let mut update_cost = Duration::ZERO;
        let mut fallback = index::now() + config.fallback_rescan_seconds;
        'run: while !self.stop.load(Ordering::Relaxed) {
            for command in self.rx.try_iter() {
                match command {
                    Command::Stop => break 'run,
                    Command::Settings(next) => {
                        let index_changed = next.index != config;
                        // Added roots, exclusions and exceptions only change
                        // their own paths, which are scanned on their own;
                        // any other change re-walks everything.
                        let added = added_paths(&config, &next.index);
                        let only_added = !added.is_empty();
                        *self.settings.write().unwrap() = *next;
                        if index_changed {
                            config = self.settings.read().unwrap().index.clone();
                            *event_policy.write().unwrap() = EventPolicy::new(&config)?;
                            reset_deadlines(&config, &mut deadlines);
                            if only_added && !current.entries.is_empty() {
                                changed.extend(added);
                                dirty = Some(Instant::now() - Duration::from_secs(60));
                            } else {
                                // The full scan re-watches folders and drops the rest.
                                full = true;
                            }
                            state.write().unwrap().status.offline = index::offline_roots(&config);
                        }
                        signals.notify(state);
                    }
                    Command::Mounts => {
                        let outcome = self.mounts_changed(&mut config, &mut changed);
                        if outcome.is_some() {
                            *event_policy.write().unwrap() = EventPolicy::new(&config)?;
                            reset_deadlines(&config, &mut deadlines);
                        }
                        if !changed.is_empty() {
                            dirty = Some(Instant::now() - Duration::from_secs(60));
                        }
                        state.write().unwrap().status.offline = index::offline_roots(&config);
                        signals.notify(state);
                    }
                    Command::Rescan(root) => match root {
                        Some(path) => {
                            if config.paths().contains(&path) {
                                changed.insert(path);
                                dirty = Some(Instant::now() - Duration::from_secs(60));
                            }
                        }
                        None => full = true,
                    },
                    Command::Pause(value) => {
                        let mut s = state.write().unwrap();
                        s.status.paused = value;
                        s.status.phase = if value { "paused" } else { "scanning" }.into();
                        s.status.scanning = false;
                        s.status.message = if value {
                            "Indexing paused"
                        } else {
                            "Resuming and reconciling…"
                        }
                        .into();
                        drop(s);
                        if !value {
                            full = true;
                        }
                        signals.notify(state);
                    }
                    Command::Event(paths) => {
                        changed.extend(paths);
                        dirty.get_or_insert_with(Instant::now);
                        if changed.len() > 50_000 {
                            full = true;
                            changed.clear();
                        }
                    }
                }
            }
            self.interrupt.store(false, Ordering::Relaxed);
            if self.paused.load(Ordering::Relaxed) {
                thread::park();
                continue;
            }
            let time = index::now();
            for (root, due) in deadlines.iter_mut() {
                if *due <= time {
                    changed.insert(root.clone());
                    dirty = Some(Instant::now() - Duration::from_secs(60));
                    if let Some(policy) = config.roots.iter().find(|r| r.path.path() == *root) {
                        *due = policy.schedule.next_after(time).unwrap_or(u64::MAX);
                    }
                }
            }
            let degraded = !state.read().unwrap().status.watch_errors.is_empty();
            if overflow.swap(false, Ordering::Relaxed) || (degraded && time >= fallback) {
                full = true;
                fallback = time + config.fallback_rescan_seconds;
            }
            let wait = Duration::from_millis(config.event_debounce_ms).max(update_cost * 10);
            let ready = dirty.is_some_and(|t| t.elapsed() >= wait);
            if full || quick || ready {
                quick &= !full;
                {
                    let mut s = state.write().unwrap();
                    s.status.scanning = true;
                    s.status.phase = "scanning".into();
                    s.status.visited = 0;
                    s.status.message = if full {
                        "Reconciling folders…"
                    } else if quick {
                        "Checking folders for changes…"
                    } else {
                        "Updating changed paths…"
                    }
                    .into();
                }
                signals.notify(state);
                let cancel = || {
                    self.stop.load(Ordering::Relaxed)
                        || self.paused.load(Ordering::Relaxed)
                        || self.interrupt.load(Ordering::Relaxed)
                };
                let last = Mutex::new(Instant::now());
                let progress = |visited| {
                    let mut last = last.lock().unwrap();
                    if last.elapsed() >= Duration::from_millis(150) {
                        *last = Instant::now();
                        state.write().unwrap().status.visited = visited;
                        signals.notify(state);
                    }
                };
                let update_started = Instant::now();
                if (full || quick)
                    && let Some(watcher) = &watcher
                {
                    sweep = watcher.begin_sweep();
                }
                let add_watch = |folder: &Path| {
                    if let Some(watcher) = &watcher {
                        watcher.add(folder, sweep);
                    }
                };
                let hook: index::WatchHook = watcher.as_ref().map(|_| &add_watch as &dyn Fn(&Path));
                let next = if full {
                    index::scan_config_watched(
                        &config,
                        dir,
                        current.generation + 1,
                        None,
                        Some(&current.entries),
                        &cancel,
                        &progress,
                        hook,
                    )?
                } else if quick {
                    index::quick_scan(&config, dir, &current, &cancel, &progress, hook)?
                } else {
                    crate::service::reconcile_watched(
                        &current, &changed, &config, dir, &cancel, &progress, hook,
                    )?
                };
                if let Some(watcher) = &watcher {
                    if (full || quick) && next.is_some() {
                        watcher.finish_sweep(sweep);
                    }
                    let (mut errors, failed) = watcher.errors();
                    if failed > errors.len() {
                        errors.push(format!("{failed} folders are not monitored"));
                    }
                    let mut s = state.write().unwrap();
                    s.status.watch_errors = errors;
                    s.status.watched = watcher.count();
                }
                if !full {
                    update_cost = update_started.elapsed().min(Duration::from_secs(3));
                }
                if let Some(next) = next {
                    // What changed on disk goes to the history; an update
                    // caused by other settings (roots, exclusions) does not.
                    if next.config == current.config
                        && next.roots == current.roots
                        && !current.entries.is_empty()
                    {
                        let targets: Option<Vec<PathBuf>> =
                            (!full && !quick).then(|| changed.iter().cloned().collect());
                        let changes = crate::journal::diff(
                            &current.entries,
                            &next.entries,
                            targets.as_deref(),
                            index::now() as i64,
                        );
                        if let Err(e) = journal.append(&changes) {
                            state.write().unwrap().status.message =
                                format!("Cannot record changes: {e}");
                        }
                    }
                    let persisted = if full || quick {
                        {
                            let mut s = state.write().unwrap();
                            s.status.phase = "saving".into();
                            s.status.visited = next.entries.len();
                            s.status.message = "Saving index…".into();
                        }
                        signals.notify(state);
                        let result = store::save(dir, &next);
                        unsaved = if result.is_ok() {
                            None
                        } else {
                            Some(Instant::now())
                        };
                        result
                    } else {
                        unsaved.get_or_insert_with(Instant::now);
                        Ok(())
                    };
                    current = Arc::new(next);
                    let mut s = state.write().unwrap();
                    s.snapshot = current.clone();
                    s.status.scanning = false;
                    s.status.phase = "idle".into();
                    s.status.entries = current.entries.len();
                    s.status.generation = current.generation;
                    s.status.last_scan = current.scanned_at;
                    s.status.errors = current.errors.clone();
                    s.status.error_count = current.error_count;
                    s.status.offline = index::offline_roots(&config);
                    s.status.message = match persisted {
                        Ok(()) => if config.paths().is_empty() {
                            "Add a folder to start indexing"
                        } else {
                            "Index is up to date"
                        }
                        .into(),
                        Err(e) => format!("Index in memory; save failed: {e}"),
                    };
                    drop(s);
                    signals.notify(state);
                    // The previous index was just released; hand freed pages back.
                    release_memory();
                    if full {
                        reset_deadlines(&config, &mut deadlines);
                    }
                    if quick {
                        catch_up_missed_rescans(&config, &current, &mut deadlines);
                    }
                    full = false;
                    quick = false;
                    changed.clear();
                    dirty = None;
                } else {
                    full = true;
                    state.write().unwrap().status.scanning = false;
                    signals.notify(state);
                    continue;
                }
            }
            if unsaved.is_some_and(|since| since.elapsed() >= SAVE_DELAY) {
                match store::save(dir, &current) {
                    Ok(()) => unsaved = None,
                    Err(e) => {
                        unsaved = Some(Instant::now());
                        state.write().unwrap().status.message =
                            format!("Index in memory; save failed: {e}");
                        signals.notify(state);
                    }
                }
            }
            let deadline = deadlines
                .values()
                .copied()
                .min()
                .unwrap_or(u64::MAX)
                .min(if degraded { fallback } else { u64::MAX });
            let mut scheduled = Duration::from_secs(deadline.saturating_sub(index::now()))
                .min(Duration::from_secs(86400));
            if let Some(since) = unsaved {
                scheduled = scheduled.min(SAVE_DELAY.saturating_sub(since.elapsed()));
            }
            let wait = Duration::from_millis(config.event_debounce_ms).max(update_cost * 10);
            let delay = dirty
                .map(|t| wait.saturating_sub(t.elapsed()))
                .unwrap_or(scheduled)
                .min(scheduled);
            thread::park_timeout(delay.max(Duration::from_millis(1)));
        }
        // Changes that arrived just before quitting are applied first, so
        // the saved index does not miss them.
        if !changed.is_empty()
            && changed.len() <= 1000
            && let Ok(Some(next)) = crate::service::reconcile_watched(
                &current,
                &changed,
                &config,
                dir,
                &|| false,
                &|_| {},
                None,
            )
        {
            current = Arc::new(next);
            unsaved.get_or_insert_with(Instant::now);
        }
        if unsaved.is_some() {
            store::save(dir, &current)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missed_rescans_wait_after_start_up() {
        let mut config = IndexConfig::from_roots(&[PathBuf::from("/a"), PathBuf::from("/b")]);
        config.roots[0].schedule = crate::config::Schedule::Interval { seconds: 3600 };
        config.roots[1].schedule = crate::config::Schedule::Interval { seconds: 3600 };
        let mut snapshot = Snapshot {
            roots: vec![b"/a".to_vec(), b"/b".to_vec()],
            ..Snapshot::default()
        };
        snapshot.walked = vec![
            (b"/a".to_vec(), index::now() - 7200),
            (b"/b".to_vec(), index::now()),
        ];
        let mut deadlines = BTreeMap::new();
        reset_deadlines(&config, &mut deadlines);
        catch_up_missed_rescans(&config, &snapshot, &mut deadlines);
        let now = index::now();
        // /a missed its hourly rescan: it runs ten minutes from now, not at once.
        assert!((now + MISSED_RESCAN_DELAY).abs_diff(deadlines[&PathBuf::from("/a")]) <= 2);
        // /b was walked just now: its next rescan is an hour away.
        assert!(deadlines[&PathBuf::from("/b")] >= now + 3500);
    }
}
