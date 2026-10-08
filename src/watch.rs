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
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use std::{
    collections::{BTreeMap, HashMap},
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

#[derive(Default)]
struct Map {
    by_wd: HashMap<WatchDescriptor, Arc<Path>>,
    /// Ordered by path so a folder's whole subtree is one range.
    by_path: BTreeMap<Arc<Path>, (WatchDescriptor, u32)>,
    sweep: u32,
    errors: Vec<String>,
    failed: usize,
}
impl Map {
    fn forget(&mut self, watches: &mut Watches, root: &Path) {
        let doomed: Vec<Arc<Path>> = self
            .by_path
            .range::<Path, _>((std::ops::Bound::Included(root), std::ops::Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(root))
            .map(|(path, _)| path.clone())
            .collect();
        for path in doomed {
            if let Some((wd, _)) = self.by_path.remove(&path) {
                self.by_wd.remove(&wd);
                let _ = watches.remove(wd);
            }
        }
    }
}

pub struct Watcher {
    watches: Watches,
    map: Arc<Mutex<Map>>,
    wake: i32,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watcher {
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
                                let Some(dir) = map.by_wd.get(&event.wd).cloned() else {
                                    continue;
                                };
                                if event.mask.contains(EventMask::IGNORED) {
                                    // The kernel removed the watch (folder deleted or unmounted).
                                    map.by_wd.remove(&event.wd);
                                    if map.by_path.get(&dir).is_some_and(|(wd, _)| *wd == event.wd)
                                    {
                                        map.by_path.remove(&dir);
                                    }
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
                                    map.forget(&mut event_watches, &dir);
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
                let path: Arc<Path> = match map.by_wd.get(&wd) {
                    // Same folder already watched (perhaps under an older path).
                    Some(old) if **old == *folder => old.clone(),
                    Some(old) => {
                        let old = old.clone();
                        map.by_path.remove(&old);
                        folder.into()
                    }
                    None => folder.into(),
                };
                map.by_wd.insert(wd.clone(), path.clone());
                map.by_path.insert(path, (wd, sweep));
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
        let stale: Vec<Arc<Path>> = map
            .by_path
            .iter()
            .filter(|(_, (_, seen))| *seen != sweep)
            .map(|(path, _)| path.clone())
            .collect();
        let mut watches = self.watches.clone();
        for path in stale {
            if let Some((wd, _)) = map.by_path.remove(&path) {
                map.by_wd.remove(&wd);
                let _ = watches.remove(wd);
            }
        }
    }

    pub fn count(&self) -> usize {
        self.map.lock().unwrap().by_path.len()
    }

    /// Watch failures since the last sweep began, with a total count.
    pub fn errors(&self) -> (Vec<String>, usize) {
        let map = self.map.lock().unwrap();
        (map.errors.clone(), map.failed)
    }
}

impl Drop for Watcher {
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
        let watcher = Watcher::new(move |s| {
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
}
