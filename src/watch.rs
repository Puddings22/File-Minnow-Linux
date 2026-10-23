//! Folder monitoring on Linux inotify.
//!
//! Only real changes are subscribed to: entries created, deleted or renamed,
//! files closed after writing, and attribute changes. Opening or listing a
//! folder (by our own scans, a file manager or `ls`) produces no event, so
//! rescans cannot feed back into more rescans. Watches are added by the
//! scanner as it reaches each folder, before that folder is read, so no
//! change made during a scan is missed and no separate walk is needed.
//!
//! Each watch costs the kernel a small amount of memory (on the order of
//! 1 KB), bounded by `fs.inotify.max_user_watches`. When the limit is hit the
//! failure is reported and periodic rescans cover the rest.
use crate::{config::WatchMode, fanotify::FanotifyWatcher};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use std::{
    collections::HashMap,
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

/// What the watcher reports.
pub enum Signal {
    /// Paths that changed (an entry, or a folder whose contents changed).
    Changed(Vec<PathBuf>),
    /// The kernel dropped events; everything should be reconciled.
    Overflow,
}

const MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::ATTRIB)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF)
    .union(WatchMask::ONLYDIR)
    .union(WatchMask::DONT_FOLLOW)
    .union(WatchMask::EXCL_UNLINK);

struct WatchEntry {
    path: Box<Path>,
    wd: WatchDescriptor,
    sweep: u32,
}

#[derive(Default)]
struct Map {
    /// In the order the scanner reaches folders, which is path order.
    entries: Vec<WatchEntry>,
    by_wd: HashMap<WatchDescriptor, u32>,
    sweep: u32,
    errors: Vec<String>,
    failed: usize,
}
impl Map {
    fn remove(&mut self, watches: &mut Watches, index: usize) {
        let entry = self.entries.swap_remove(index);
        self.by_wd.remove(&entry.wd);
        let _ = watches.remove(entry.wd);
        if let Some(moved) = self.entries.get(index) {
            self.by_wd.insert(moved.wd.clone(), index as u32);
        }
    }
    fn forget(&mut self, watches: &mut Watches, root: &Path) {
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].path.starts_with(root) {
                self.remove(watches, index);
            } else {
                index += 1;
            }
        }
    }
}

pub struct InotifyWatcher {
    watches: Watches,
    map: Arc<Mutex<Map>>,
    wake: i32,
    thread: Option<thread::JoinHandle<()>>,
}

impl InotifyWatcher {
    pub fn new(on_signal: impl Fn(Signal) + Send + 'static) -> io::Result<Self> {
        let mut inotify = Inotify::init()?;
        let watches = inotify.watches();
        let map = Arc::new(Mutex::new(Map::default()));
        let mut fds = [0; 2];
        // SAFETY: `fds` is a valid two-element array for pipe2 to fill.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let (stop_read, stop_write) = (fds[0], fds[1]);
        let events_map = map.clone();
        let mut event_watches = watches.clone();
        let thread = thread::Builder::new()
            .name("file-minnow-watch".into())
            .spawn(move || {
                let mut buffer = vec![0_u8; 64 * 1024];
                let mut poll = [
                    libc::pollfd {
                        fd: inotify.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: stop_read,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                loop {
                    // Sleep until the kernel has events or we are asked to stop.
                    // SAFETY: `poll` holds two initialized pollfd structures.
                    let ready = unsafe { libc::poll(poll.as_mut_ptr(), 2, -1) };
                    if ready < 0 {
                        if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                    if poll[1].revents != 0 {
                        break;
                    }
                    loop {
                        let events = match inotify.read_events(&mut buffer) {
                            Ok(events) => events,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(_) => {
                                on_signal(Signal::Overflow);
                                break;
                            }
                        };
                        let mut changed = Vec::new();
                        let mut overflow = false;
                        {
                            let mut map = events_map.lock().unwrap();
                            for event in events {
                                if event.mask.contains(EventMask::Q_OVERFLOW) {
                                    overflow = true;
                                    continue;
                                }
                                let Some(&index) = map.by_wd.get(&event.wd) else {
                                    continue;
                                };
                                let Some(entry) = map.entries.get(index as usize) else {
                                    continue;
                                };
                                let dir = entry.path.clone();
                                if event.mask.contains(EventMask::IGNORED) {
                                    // The kernel removed the watch (folder deleted or unmounted).
                                    map.remove(&mut event_watches, index as usize);
                                    continue;
                                }
                                let path = match event.name {
                                    Some(name) if !name.is_empty() => dir.join(name),
                                    _ => dir.to_path_buf(),
                                };
                                // A folder that left this place keeps its watches under the
                                // old path; drop them. Its new location is rescanned (and
                                // watched) through the event for the destination.
                                if event.mask.contains(EventMask::ISDIR)
                                    && event
                                        .mask
                                        .intersects(EventMask::MOVED_FROM | EventMask::DELETE)
                                {
                                    map.forget(&mut event_watches, &path);
                                }
                                if event.mask.contains(EventMask::MOVE_SELF) {
                                    map.forget(&mut event_watches, dir.as_ref());
                                }
                                changed.push(path);
                            }
                        }
                        if overflow {
                            on_signal(Signal::Overflow);
                        }
                        if !changed.is_empty() {
                            on_signal(Signal::Changed(changed));
                        }
                    }
                }
                // SAFETY: closing our own pipe end once, at thread exit.
                unsafe { libc::close(stop_read) };
            })?;
        Ok(Self {
            watches,
            map,
            wake: stop_write,
            thread: Some(thread),
        })
    }

    /// Starts a full sweep. Folders added with the returned number are kept;
    /// `finish_sweep` drops every watch the sweep did not reach.
    pub fn begin_sweep(&self) -> u32 {
        let mut map = self.map.lock().unwrap();
        map.sweep = map.sweep.wrapping_add(1);
        map.errors.clear();
        map.failed = 0;
        map.sweep
    }

    /// Watches one folder (not its subfolders). Failures, typically the
    /// watch limit, are counted and reported by `errors`.
    pub fn add(&self, folder: &Path, sweep: u32) {
        let mut watches = self.watches.clone();
        let result = watches.add(folder, MASK);
        let mut map = self.map.lock().unwrap();
        match result {
            Ok(wd) => {
                if let Some(&index) = map.by_wd.get(&wd) {
                    // Same folder already watched (perhaps under an older path).
                    let entry = &mut map.entries[index as usize];
                    if entry.path.as_ref() != folder {
                        entry.path = folder.into();
                    }
                    entry.sweep = sweep;
                } else {
                    let index = map.entries.len() as u32;
                    map.by_wd.insert(wd.clone(), index);
                    map.entries.push(WatchEntry {
                        path: folder.into(),
                        wd,
                        sweep,
                    });
                }
            }
            Err(e) => {
                map.failed += 1;
                if map.errors.len() < 5 {
                    let hint = if e.raw_os_error() == Some(libc::ENOSPC) {
                        " (folder watch limit reached; raise fs.inotify.max_user_watches)"
                    } else {
                        ""
                    };
                    map.errors.push(format!("{}: {e}{hint}", folder.display()));
                }
            }
        }
    }

    /// Drops watches that the sweep `sweep` did not reach.
    pub fn finish_sweep(&self, sweep: u32) {
        let mut map = self.map.lock().unwrap();
        let mut watches = self.watches.clone();
        let mut index = 0;
        while index < map.entries.len() {
            if map.entries[index].sweep != sweep {
                map.remove(&mut watches, index);
            } else {
                index += 1;
            }
        }
    }

    pub fn count(&self) -> usize {
        self.map.lock().unwrap().entries.len()
    }

    pub fn record_error(&self, message: String) {
        let mut map = self.map.lock().unwrap();
        if map.errors.len() < 5 {
            map.errors.push(message);
        }
        map.failed += 1;
    }

    /// Watch failures since the last sweep began, with a total count.
    pub fn errors(&self) -> (Vec<String>, usize) {
        let map = self.map.lock().unwrap();
        (map.errors.clone(), map.failed)
    }
}

impl Drop for InotifyWatcher {
    fn drop(&mut self) {
        // SAFETY: writing one byte to, then closing, our own pipe end.
        unsafe {
            libc::write(self.wake, [1_u8].as_ptr().cast(), 1);
            libc::close(self.wake);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum Backend {
    Inotify(InotifyWatcher),
    Fanotify(FanotifyWatcher),
}

/// Folder monitoring, either one inotify watch per folder or one fanotify
/// mark per filesystem when the daemon starts as root.
pub struct Watcher {
    backend: Backend,
    fallback: Option<String>,
}
impl Watcher {
    /// The unprivileged inotify watcher. Used by tests and small tools.
    pub fn new(on_signal: impl Fn(Signal) + Send + 'static) -> io::Result<Self> {
        Ok(Self {
            backend: Backend::Inotify(InotifyWatcher::new(on_signal)?),
            fallback: None,
        })
    }
    pub fn new_with_mode(
        on_signal: impl Fn(Signal) + Send + Sync + 'static,
        mode: WatchMode,
        roots: &[PathBuf],
    ) -> io::Result<Self> {
        let callback: Arc<dyn Fn(Signal) + Send + Sync> = Arc::new(on_signal);
        let mut fallback = None;
        let try_fanotify = match mode {
            WatchMode::Fanotify => true,
            // SAFETY: geteuid has no preconditions.
            WatchMode::Auto => unsafe { libc::geteuid() == 0 },
            WatchMode::Inotify => false,
        };
        if try_fanotify {
            match FanotifyWatcher::new(callback.clone(), roots) {
                Ok(watcher) => {
                    return Ok(Self {
                        backend: Backend::Fanotify(watcher),
                        fallback: None,
                    });
                }
                Err(error) => fallback = Some(format!("fanotify unavailable: {error}")),
            }
        }
        let callback = callback;
        Ok(Self {
            backend: Backend::Inotify(InotifyWatcher::new(move |signal| callback(signal))?),
            fallback,
        })
    }
    pub fn begin_sweep(&self) -> u32 {
        match &self.backend {
            Backend::Inotify(watcher) => watcher.begin_sweep(),
            Backend::Fanotify(watcher) => watcher.begin_sweep(),
        }
    }
    pub fn add(&self, folder: &Path, sweep: u32) {
        match &self.backend {
            Backend::Inotify(watcher) => watcher.add(folder, sweep),
            Backend::Fanotify(watcher) => watcher.add(folder, sweep),
        }
    }
    pub fn finish_sweep(&self, sweep: u32) {
        match &self.backend {
            Backend::Inotify(watcher) => watcher.finish_sweep(sweep),
            Backend::Fanotify(watcher) => watcher.finish_sweep(sweep),
        }
    }
    pub fn count(&self) -> usize {
        match &self.backend {
            Backend::Inotify(watcher) => watcher.count(),
            Backend::Fanotify(watcher) => watcher.count(),
        }
    }
    pub fn errors(&self) -> (Vec<String>, usize) {
        let (mut errors, failed) = match &self.backend {
            Backend::Inotify(watcher) => watcher.errors(),
            Backend::Fanotify(watcher) => watcher.errors(),
        };
        if let Some(fallback) = &self.fallback {
            errors.insert(0, fallback.clone());
            (errors, failed + 1)
        } else {
            (errors, failed)
        }
    }
    pub fn is_fanotify(&self) -> bool {
        matches!(self.backend, Backend::Fanotify(_))
    }
    pub fn record_error(&self, message: String) {
        match &self.backend {
            Backend::Inotify(watcher) => watcher.record_error(message),
            Backend::Fanotify(watcher) => watcher.record_error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::mpsc, time::Duration};

    /// Waits until `path` is reported (events may arrive in several batches).
    fn expect(rx: &mpsc::Receiver<Signal>, path: &Path) {
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).expect("event") {
                Signal::Changed(paths) if paths.iter().any(|p| p == path) => return,
                Signal::Changed(_) => {}
                Signal::Overflow => panic!("unexpected overflow"),
            }
        }
    }

    #[test]
    fn reports_changes_but_not_reads_and_follows_moves() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("sub")).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = InotifyWatcher::new(move |s| {
            let _ = tx.send(s);
        })
        .unwrap();
        let sweep = watcher.begin_sweep();
        watcher.add(&root, sweep);
        watcher.add(&root.join("sub"), sweep);
        assert_eq!(watcher.count(), 2);
        // Reading never wakes the watcher.
        for _ in 0..50 {
            let _ = fs::read_dir(&root).unwrap().count();
            let _ = fs::read_dir(root.join("sub")).unwrap().count();
        }
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        fs::write(root.join("sub/new.txt"), b"x").unwrap();
        expect(&rx, &root.join("sub/new.txt"));
        // Moving a watched folder away drops its watch; the parent reports it.
        fs::rename(root.join("sub"), temp.path().join("elsewhere")).unwrap();
        expect(&rx, &root.join("sub"));
        assert_eq!(watcher.count(), 1);
        // A sweep that only reaches the root keeps only the root.
        let sweep = watcher.begin_sweep();
        watcher.add(&root, sweep);
        watcher.finish_sweep(sweep);
        assert_eq!(watcher.count(), 1);
        drop(watcher);
    }

    #[test]
    fn forget_keeps_watch_indices_valid() {
        let temp = tempfile::tempdir().unwrap();
        let inotify = Inotify::init().unwrap();
        let mut watches = inotify.watches();
        let mut map = Map::default();
        for relative in ["a/1", "a/2", "b/1", "b/2"] {
            let dir = temp.path().join(relative);
            fs::create_dir_all(&dir).unwrap();
            let wd = watches.add(&dir, MASK).unwrap();
            let index = map.entries.len() as u32;
            map.by_wd.insert(wd.clone(), index);
            map.entries.push(WatchEntry {
                path: dir.into_boxed_path(),
                wd,
                sweep: 1,
            });
        }
        map.forget(&mut watches, &temp.path().join("a"));
        assert_eq!(map.entries.len(), 2);
        for (index, entry) in map.entries.iter().enumerate() {
            assert_eq!(map.by_wd.get(&entry.wd), Some(&(index as u32)));
        }
    }
}
