use crate::{
    config::{ConfigStore, IndexConfig, PatternRules, Settings},
    index::{self, Snapshot},
    store,
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
    Stop,
}
#[derive(Clone)]
pub struct Handle {
    pub state: Shared,
    settings: Arc<RwLock<Settings>>,
    config: ConfigStore,
    commands: mpsc::SyncSender<Command>,
    wake: thread::Thread,
    signals: Arc<Signals>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    interrupt: Arc<AtomicBool>,
}
impl Handle {
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
        let handle = Handle {
            state: state.clone(),
            settings,
            config,
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
/// How long small updates may stay unsaved. Startup always rescans, so a
/// crash only delays (never loses) those changes.
const SAVE_DELAY: Duration = Duration::from_secs(300);
struct Worker {
    dir: PathBuf,
    state: Shared,
    settings: Arc<RwLock<Settings>>,
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
        let mut sweep = 0_u32;
        let mut deadlines = BTreeMap::new();
        reset_deadlines(&config, &mut deadlines);
        let mut full = true;
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
                        *self.settings.write().unwrap() = *next;
                        if index_changed {
                            config = self.settings.read().unwrap().index.clone();
                            *event_policy.write().unwrap() = EventPolicy::new(&config)?;
                            reset_deadlines(&config, &mut deadlines);
                            // The full scan re-watches folders and drops the rest.
                            full = true;
                        }
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
            if full || ready {
                {
                    let mut s = state.write().unwrap();
                    s.status.scanning = true;
                    s.status.phase = "scanning".into();
                    s.status.visited = 0;
                    s.status.message = if full {
                        "Reconciling folders…"
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
                if full && let Some(watcher) = &watcher {
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
                        &cancel,
                        &progress,
                        hook,
                    )?
                } else {
                    crate::service::reconcile_watched(
                        &current, &changed, &config, dir, &cancel, &progress, hook,
                    )?
                };
                if let Some(watcher) = &watcher {
                    if full && next.is_some() {
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
                    let persisted = if full {
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
                    full = false;
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
        if unsaved.is_some() {
            store::save(dir, &current)?;
        }
        Ok(())
    }
}
