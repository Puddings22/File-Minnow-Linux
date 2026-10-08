pub use crate::runtime::{Handle, Runtime, Shared, State, Status};
use crate::{
    config::{IndexConfig, Settings},
    index::{self, Snapshot},
    query::{self, SearchRequest, SearchResponse},
    table::{Builder, path_cmp},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Duration,
};

// Reconcile final filesystem state rather than assuming rename event ordering.
// Periodic full scans repair gaps, watcher overflow, missed remote changes and stale errors.
pub fn reconcile(
    snapshot: &Snapshot,
    changed: &BTreeSet<PathBuf>,
    roots: &[PathBuf],
    dir: &Path,
) -> Snapshot {
    reconcile_config(
        snapshot,
        changed,
        &IndexConfig::from_roots(roots),
        dir,
        &|| false,
        &|_| {},
    )
    .expect("default configuration")
    .expect("uncancellable reconciliation")
}
pub fn reconcile_config(
    snapshot: &Snapshot,
    changed: &BTreeSet<PathBuf>,
    config: &IndexConfig,
    dir: &Path,
    cancel: &dyn Fn() -> bool,
    progress: &dyn Fn(usize),
) -> Result<Option<Snapshot>> {
    reconcile_watched(snapshot, changed, config, dir, cancel, progress, None)
}

/// `reconcile_config` that also watches folders found in rescanned subtrees.
pub fn reconcile_watched(
    snapshot: &Snapshot,
    changed: &BTreeSet<PathBuf>,
    config: &IndexConfig,
    dir: &Path,
    cancel: &dyn Fn() -> bool,
    progress: &dyn Fn(usize),
    watch: index::WatchHook,
) -> Result<Option<Snapshot>> {
    // Changed paths inside enabled roots, in component order, with paths
    // already covered by an earlier (ancestor) target removed.
    let roots = config.paths();
    let mut targets: Vec<PathBuf> = changed
        .iter()
        .filter(|path| !index::excluded(path, dir) && roots.iter().any(|r| path.starts_with(r)))
        .cloned()
        .collect();
    targets.sort_by(|a, b| path_cmp(a.as_os_str().as_bytes(), b.as_os_str().as_bytes()));
    let mut kept: Vec<PathBuf> = Vec::with_capacity(targets.len());
    for target in targets {
        if kept.last().is_none_or(|last| !target.starts_with(last)) {
            kept.push(target);
        }
    }
    let mut errors = snapshot.errors.clone();
    let mut error_count = snapshot.error_count;
    let mut visited = 0;
    let old = &snapshot.entries;
    let mut builder = Builder::with_capacity(old.len());
    let mut cursor = 0;
    // Unchanged ranges: (old start, old end, new start), for the name order.
    let mut unchanged = Vec::new();
    let mut copy = |builder: &mut Builder, from: usize, to: usize| -> bool {
        if cancel() {
            return false;
        }
        if from < to {
            unchanged.push((from, to, builder.len()));
            builder.copy_range(old, from, to);
        }
        true
    };
    for target in kept {
        let bytes = target.as_os_str().as_bytes();
        // Unchanged rows before this subtree are copied without rescanning.
        let start = old.lower_bound(bytes).max(cursor);
        let end = old.subtree_end(start, bytes);
        if !copy(&mut builder, cursor, start) {
            return Ok(None);
        }
        cursor = end;
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                let mut log = index::ScanLog {
                    errors: &mut errors,
                    count: &mut error_count,
                    visited: &mut visited,
                };
                let targets = [target];
                if !index::scan_into(
                    config,
                    dir,
                    Some(&targets),
                    &mut builder,
                    &mut log,
                    cancel,
                    progress,
                    watch,
                )? {
                    return Ok(None);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => index::record_error(
                &mut errors,
                &mut error_count,
                format!("{}: {e}", target.display()),
            ),
        }
    }
    if !copy(&mut builder, cursor, old.len()) {
        return Ok(None);
    }
    let entries = builder.finish();
    entries.splice_name_order(old, &unchanged);
    Ok(Some(Snapshot {
        roots: config
            .roots
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.0.clone())
            .collect(),
        entries,
        generation: snapshot.generation + 1,
        scanned_at: index::now(),
        errors,
        error_count,
    }))
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Search(SearchRequest),
    Status,
    Rescan,
    RescanRoot { path: crate::config::RawPath },
    Pause { paused: bool },
    Settings,
    SetSettings { settings: Box<Settings> },
    Watch { after: u64 },
    Shutdown,
    Pulse,
}
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub search: Option<SearchResponse>,
    pub status: Status,
    pub error: Option<String>,
    #[serde(default)]
    pub settings: Option<Settings>,
}
pub fn request(dir: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(crate::store::socket_path(dir, "search.sock"))
        .context("No index service is running")?;
    stream.set_read_timeout(Some(Duration::from_secs(65)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(64 * 1024 * 1024)
        .read_line(&mut response)?;
    Ok(serde_json::from_str(&response)?)
}
fn handle_client(mut stream: UnixStream, handle: &Handle) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    if BufReader::new(&stream)
        .take(4 * 1024 * 1024 + 1)
        .read_line(&mut line)
        .is_err()
    {
        return;
    }
    let mut search = None;
    let mut settings = None;
    let mut run = || -> Result<()> {
        if line.len() > 4 * 1024 * 1024 {
            bail!("Request exceeds 4 MiB");
        }
        match serde_json::from_str::<Request>(&line)? {
            Request::Search(r) => {
                let snapshot = handle.state.read().unwrap().snapshot.clone();
                search = Some(query::search(&snapshot, &r, None)?);
            }
            Request::Status => {}
            Request::Rescan => handle.rescan(None)?,
            Request::RescanRoot { path } => handle.rescan(Some(path.path()))?,
            Request::Pause { paused } => handle.pause(paused)?,
            Request::Settings => settings = Some(handle.settings()),
            Request::SetSettings { settings: next } => {
                settings = Some(handle.apply_settings(*next)?)
            }
            Request::Watch { after } => {
                handle.wait_for_change(after, Duration::from_secs(55));
            }
            Request::Shutdown => handle.shutdown(),
            Request::Pulse => handle.pulse(),
        }
        Ok(())
    };
    let error = run().err().map(|e| e.to_string());
    let response = Response {
        search,
        status: handle.status(),
        error,
        settings,
    };
    if serde_json::to_writer(&mut stream, &response).is_ok() {
        let _ = stream.write_all(b"\n");
    }
}
fn listener(dir: &Path) -> Result<UnixListener> {
    let socket = crate::store::socket_path(dir, "search.sock");
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            bail!("Index service already listening");
        }
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
pub fn serve(runtime: &Runtime, dir: &Path) -> Result<()> {
    let server = Server::start(runtime.handle.clone(), dir)?;
    while !runtime.handle.stopped() {
        let revision = runtime.handle.status().revision;
        runtime
            .handle
            .wait_for_change(revision, Duration::from_secs(55));
    }
    drop(server);
    Ok(())
}
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};
/// Bounded request workers prevent a slow client from blocking the indexer.
pub struct Server {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Handle,
    join: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub fn start(handle: Handle, dir: &Path) -> Result<Self> {
        let listener = listener(dir)?;
        let socket = crate::store::socket_path(dir, "search.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let server_handle = handle.clone();
        let join = thread::spawn(move || {
            let (tx, rx) = mpsc::sync_channel::<UnixStream>(16);
            let rx = Arc::new(Mutex::new(rx));
            let mut workers = Vec::new();
            for _ in 0..4 {
                let rx = rx.clone();
                let handle = server_handle.clone();
                workers.push(thread::spawn(move || {
                    loop {
                        let stream = rx.lock().unwrap().recv();
                        match stream {
                            Ok(stream) => handle_client(stream, &handle),
                            Err(_) => break,
                        }
                    }
                }));
            }
            for stream in listener.incoming() {
                if ending.load(Ordering::Relaxed) || server_handle.stopped() {
                    break;
                }
                if let Ok(stream) = stream {
                    let _ = tx.try_send(stream);
                }
            }
            drop(tx);
            for worker in workers {
                let _ = worker.join();
            }
        });
        Ok(Self {
            socket,
            stop,
            handle,
            join: Some(join),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.pulse();
        let _ = UnixStream::connect(&self.socket);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}
