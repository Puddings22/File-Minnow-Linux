//! Fanotify folder monitoring for privileged daemons.
//!
//! One mark per filesystem replaces one inotify watch per folder. The daemon
//! must start as root to add the mark; it then drops to the invoking user.
//! File handles are resolved through `/proc/self/fd`; if a handle cannot be
//! opened, the event is reported as an overflow and a rescan repairs the gap.

use crate::watch::Signal;
use std::{
    ffi::CString,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};

const EVENT_LEN: usize = 24;
const INFO_FID: u8 = 1;
const INFO_DFID_NAME: u8 = 2;
const MAX_HANDLE: usize = 128;
const CAP_DAC_READ_SEARCH: u32 = 2;
const CAP_VERSION_3: u32 = 0x2008_0522;

const MASK: u64 = libc::FAN_CREATE
    | libc::FAN_DELETE
    | libc::FAN_MOVED_FROM
    | libc::FAN_MOVED_TO
    | libc::FAN_ATTRIB
    | libc::FAN_CLOSE_WRITE
    | libc::FAN_ONDIR;

pub struct FanotifyWatcher {
    fd: i32,
    wake: i32,
    stop: i32,
    thread: Option<JoinHandle<()>>,
    callback: Option<Arc<dyn Fn(Signal) + Send + Sync>>,
    mount_fds: Vec<i32>,
    folders: AtomicUsize,
    errors: Arc<Mutex<(Vec<String>, usize)>>,
}

impl FanotifyWatcher {
    pub fn new(
        on_signal: Arc<dyn Fn(Signal) + Send + Sync>,
        roots: &[PathBuf],
    ) -> io::Result<Self> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fanotify needs root",
            ));
        }
        let flags = libc::FAN_CLASS_NOTIF
            | libc::FAN_CLOEXEC
            | libc::FAN_NONBLOCK
            | libc::FAN_REPORT_FID
            | libc::FAN_REPORT_DFID_NAME;
        let event_flags = (libc::O_RDONLY | libc::O_LARGEFILE | libc::O_CLOEXEC) as u32;
        // SAFETY: the arguments are the documented fanotify_init flags.
        let fd = unsafe { libc::fanotify_init(flags, event_flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut mount_fds = Vec::new();
        let mut devices = std::collections::HashSet::new();
        for root in roots {
            let device = match fs::metadata(root) {
                Ok(metadata) => metadata.dev(),
                Err(error) => {
                    close_all(&mount_fds);
                    // SAFETY: closing our own fanotify fd.
                    unsafe { libc::close(fd) };
                    return Err(error);
                }
            };
            if !devices.insert(device) {
                continue;
            }
            let Ok(name) = CString::new(root.as_os_str().as_bytes()) else {
                continue;
            };
            // SAFETY: `name` is a NUL-terminated path and `fd` is our fanotify fd.
            let marked = unsafe {
                libc::fanotify_mark(
                    fd,
                    libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                    MASK,
                    libc::AT_FDCWD,
                    name.as_ptr(),
                )
            };
            if marked != 0 {
                let error = io::Error::last_os_error();
                close_all(&mount_fds);
                // SAFETY: closing our own fanotify fd.
                unsafe { libc::close(fd) };
                return Err(error);
            }
            // SAFETY: opening the root read-only; the descriptor is only a
            // reference for open_by_handle_at.
            let mount_fd = unsafe {
                libc::open(
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
                )
            };
            if mount_fd < 0 {
                let error = io::Error::last_os_error();
                close_all(&mount_fds);
                // SAFETY: closing our own fanotify fd.
                unsafe { libc::close(fd) };
                return Err(error);
            }
            mount_fds.push(mount_fd);
        }
        if mount_fds.is_empty() {
            // SAFETY: closing our own fanotify fd.
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no fanotify roots",
            ));
        }
        let mut pipe = [0; 2];
        // SAFETY: `pipe` has room for two file descriptors.
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            let error = io::Error::last_os_error();
            close_all(&mount_fds);
            // SAFETY: closing our own fanotify fd.
            unsafe { libc::close(fd) };
            return Err(error);
        }
        let (stop_read, stop_write) = (pipe[0], pipe[1]);
        Ok(Self {
            fd,
            wake: stop_write,
            stop: stop_read,
            thread: None,
            callback: Some(on_signal),
            mount_fds,
            folders: AtomicUsize::new(0),
            errors: Arc::new(Mutex::new((Vec::new(), 0))),
        })
    }

    /// Starts the event thread. Call this after dropping root so the new
    /// thread inherits the capability needed to resolve file handles.
    pub fn start(&mut self) {
        if self.thread.is_some() {
            return;
        }
        let Some(on_signal) = self.callback.take() else {
            return;
        };
        let mount_fds = std::mem::take(&mut self.mount_fds);
        let stop = self.stop;
        let fd = self.fd;
        let errors = self.errors.clone();
        match thread::Builder::new()
            .name("file-minnow-fanotify".into())
            .spawn(move || run(fd, stop, mount_fds, on_signal, errors))
        {
            Ok(thread) => self.thread = Some(thread),
            Err(error) => self.record_error(format!("cannot start fanotify thread: {error}")),
        }
    }

    pub fn begin_sweep(&self) -> u32 {
        self.folders.store(0, Ordering::Relaxed);
        0
    }
    pub fn add(&self, _folder: &Path, _sweep: u32) {
        self.folders.fetch_add(1, Ordering::Relaxed);
    }
    pub fn finish_sweep(&self, _sweep: u32) {}
    pub fn count(&self) -> usize {
        self.folders.load(Ordering::Relaxed)
    }
    pub fn errors(&self) -> (Vec<String>, usize) {
        // Resolution failures are reported in the message list; they do not
        // mean that folders are unmonitored (there is one mark per filesystem).
        (self.errors.lock().unwrap().0.clone(), 0)
    }
    pub fn record_error(&self, message: String) {
        let mut errors = self.errors.lock().unwrap();
        if errors.0.len() < 5 {
            errors.0.push(message);
        }
        errors.1 += 1;
    }
}

impl Drop for FanotifyWatcher {
    fn drop(&mut self) {
        // SAFETY: writing to and closing our own pipe end.
        unsafe {
            libc::write(self.wake, [1_u8].as_ptr().cast(), 1);
            libc::close(self.wake);
            libc::close(self.fd);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        } else {
            // The event thread was never started, so it did not close
            // these descriptors.
            // SAFETY: closing our own descriptors.
            unsafe { libc::close(self.stop) };
            close_all(&self.mount_fds);
        }
    }
}

fn close_all(fds: &[i32]) {
    for &fd in fds {
        // SAFETY: closing our own descriptor.
        unsafe { libc::close(fd) };
    }
}

fn run(
    fd: i32,
    stop: i32,
    mount_fds: Vec<i32>,
    on_signal: Arc<dyn Fn(Signal) + Send + Sync>,
    errors: Arc<Mutex<(Vec<String>, usize)>>,
) {
    let mut poll = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut unresolved = 0_u32;
    loop {
        // SAFETY: `poll` has two initialized entries.
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
            // SAFETY: reading into a live buffer from our fanotify fd.
            let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                on_signal(Signal::Overflow);
                break;
            }
            if read == 0 {
                break;
            }
            let mut changed = Vec::new();
            let mut overflow = false;
            let mut offset = 0;
            let len = read as usize;
            while offset + EVENT_LEN <= len {
                let event_len =
                    u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
                if event_len < EVENT_LEN || offset + event_len > len {
                    break;
                }
                let mask = u64::from_ne_bytes(buffer[offset + 8..offset + 16].try_into().unwrap());
                let event = &buffer[offset..offset + event_len];
                if mask & libc::FAN_Q_OVERFLOW != 0 {
                    overflow = true;
                } else {
                    match resolve_event(event, &mount_fds) {
                        Some(path) => {
                            unresolved = 0;
                            changed.push(path);
                        }
                        None => {
                            // A handle for a file that vanished (the index
                            // save renames a temporary file) is normal. Only
                            // report a run of failures, which means handle
                            // resolution is not working at all.
                            unresolved += 1;
                            if unresolved >= 8 {
                                let mut state = errors.lock().unwrap();
                                if state.0.is_empty() {
                                    state.0.push(
                                        "fanotify could not resolve events; periodic rescans will catch up"
                                            .into(),
                                    );
                                }
                                state.1 += 1;
                                unresolved = 0;
                            }
                        }
                    }
                }
                offset += event_len;
            }
            if overflow {
                on_signal(Signal::Overflow);
            }
            if !changed.is_empty() {
                on_signal(Signal::Changed(changed));
            }
        }
    }
    close_all(&mount_fds);
    // SAFETY: closing our own pipe end once, at thread exit.
    unsafe { libc::close(stop) };
}

#[repr(C, align(8))]
struct Handle {
    bytes: [u8; 8 + MAX_HANDLE],
}

impl Handle {
    fn new(handle_bytes: u32, handle_type: i32, data: &[u8]) -> Option<Self> {
        let len = handle_bytes as usize;
        if len > MAX_HANDLE || data.len() < len {
            return None;
        }
        let mut handle = Self {
            bytes: [0; 8 + MAX_HANDLE],
        };
        handle.bytes[0..4].copy_from_slice(&handle_bytes.to_ne_bytes());
        handle.bytes[4..8].copy_from_slice(&handle_type.to_ne_bytes());
        handle.bytes[8..8 + len].copy_from_slice(&data[..len]);
        Some(handle)
    }
    fn resolve(&mut self, mount_fds: &[i32]) -> Option<PathBuf> {
        for &mount_fd in mount_fds {
            // SAFETY: `bytes` starts with a valid file_handle header.
            let fd = unsafe {
                libc::open_by_handle_at(
                    mount_fd,
                    self.bytes.as_mut_ptr().cast::<libc::file_handle>(),
                    libc::O_PATH | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                continue;
            }
            let path = fs::read_link(format!("/proc/self/fd/{fd}")).ok();
            // SAFETY: closing the descriptor opened above.
            unsafe { libc::close(fd) };
            if let Some(path) = path {
                return Some(strip_deleted(path));
            }
        }
        None
    }
}

fn strip_deleted(path: PathBuf) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    if let Some(stripped) = bytes.strip_suffix(b" (deleted)") {
        use std::os::unix::ffi::OsStringExt;
        return PathBuf::from(std::ffi::OsString::from_vec(stripped.to_vec()));
    }
    path
}

fn resolve_event(event: &[u8], mount_fds: &[i32]) -> Option<PathBuf> {
    let metadata_len = u16::from_ne_bytes([event[6], event[7]]) as usize;
    let mut offset = if metadata_len >= EVENT_LEN {
        metadata_len
    } else {
        EVENT_LEN
    };
    let mut parent_only = None;
    while offset + 4 <= event.len() {
        let info_type = event[offset];
        let info_len = u16::from_ne_bytes([event[offset + 2], event[offset + 3]]) as usize;
        if info_len < 4 || offset + info_len > event.len() {
            break;
        }
        let info = &event[offset..offset + info_len];
        match info_type {
            INFO_DFID_NAME => {
                if info.len() < 20 {
                    break;
                }
                let handle_bytes = u32::from_ne_bytes(info[12..16].try_into().unwrap());
                let handle_type = i32::from_ne_bytes(info[16..20].try_into().unwrap());
                let name_start = 20 + handle_bytes as usize;
                if name_start > info.len() {
                    break;
                }
                let Some(mut handle) =
                    Handle::new(handle_bytes, handle_type, &info[20..name_start])
                else {
                    break;
                };
                let Some(parent) = handle.resolve(mount_fds) else {
                    break;
                };
                let name = info[name_start..]
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default();
                if name.is_empty() {
                    parent_only = Some(parent);
                } else {
                    use std::os::unix::ffi::OsStrExt;
                    return Some(parent.join(std::ffi::OsStr::from_bytes(name)));
                }
            }
            INFO_FID => {
                if info.len() < 12 {
                    break;
                }
                let handle_bytes = u32::from_ne_bytes(info[4..8].try_into().unwrap());
                let handle_type = i32::from_ne_bytes(info[8..12].try_into().unwrap());
                let end = 12 + handle_bytes as usize;
                if end > info.len() {
                    break;
                }
                let Some(mut handle) = Handle::new(handle_bytes, handle_type, &info[12..end])
                else {
                    break;
                };
                if let Some(path) = handle.resolve(mount_fds) {
                    return Some(path);
                }
            }
            _ => {}
        }
        offset += info_len;
    }
    parent_only
}

/// Chowns the data directory and drops root, trying to keep only the
/// capability needed to resolve fanotify file handles.
pub fn drop_to_invoker(data_dir: &Path) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let uid = std::env::var("SUDO_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let gid = std::env::var("SUDO_GID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (uid, gid) else {
        return Ok(());
    };
    chown_tree(data_dir, uid, gid);
    // SAFETY: standard privilege drop sequence.
    unsafe {
        if libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        if libc::setgroups(0, std::ptr::null()) != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        if libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
    }
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    let mut data = [
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    data[0].effective = 1 << CAP_DAC_READ_SEARCH;
    data[0].permitted = 1 << CAP_DAC_READ_SEARCH;
    // SAFETY: capset reads the standard two-word capability structure.
    let result = unsafe { libc::syscall(libc::SYS_capset, &header, data.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn chown_tree(path: &Path, uid: u32, gid: u32) {
    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: `name` is a valid path.
    unsafe { libc::chown(name.as_ptr(), uid, gid) };
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if let Ok(metadata) = entry.metadata()
            && metadata.is_dir()
        {
            chown_tree(&child, uid, gid);
            continue;
        }
        if let Ok(name) = CString::new(child.as_os_str().as_bytes()) {
            // SAFETY: `name` is a valid path.
            unsafe { libc::chown(name.as_ptr(), uid, gid) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_dfid_name_event() {
        // One DFID_NAME info record with a 4-byte handle and the name "x".
        let handle_bytes = 4_u32;
        let info_len = 20 + handle_bytes as usize + 2;
        let event_len = EVENT_LEN + info_len;
        let mut event = vec![0_u8; event_len];
        event[0..4].copy_from_slice(&(event_len as u32).to_ne_bytes());
        event[6..8].copy_from_slice(&(EVENT_LEN as u16).to_ne_bytes());
        event[8..16].copy_from_slice(&libc::FAN_CREATE.to_ne_bytes());
        let info = EVENT_LEN;
        event[info] = INFO_DFID_NAME;
        event[info + 2..info + 4].copy_from_slice(&(info_len as u16).to_ne_bytes());
        event[info + 12..info + 16].copy_from_slice(&handle_bytes.to_ne_bytes());
        event[info + 16..info + 20].copy_from_slice(&0_i32.to_ne_bytes());
        event[info + 20..info + 24].copy_from_slice(&[1, 2, 3, 4]);
        event[info + 24] = b'x';
        event[info + 25] = 0;

        // Without a mount fd the handle cannot be resolved, but the parser
        // must find the record and stop cleanly.
        assert!(resolve_event(&event, &[]).is_none());
    }
}
