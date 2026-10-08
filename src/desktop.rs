//! Linux desktop integration. No event polling and no shell command interpolation.
use crate::{config::UiConfig, shortcuts::Chord};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WindowAction {
    Show { query: Option<String> },
    Toggle,
    Hide,
    Settings,
    Rescan,
    Pause,
    Quit,
    Status,
}
#[derive(Clone, Debug)]
pub enum Event {
    Action(WindowAction),
    TrayAvailable(bool),
    IntegrationError(String),
    HotkeyStatus(String),
    PortalActivation(Option<String>),
}
#[derive(Clone)]
pub struct Emitter {
    send: mpsc::Sender<Event>,
    wake: WakeCallback,
}
type WakeCallback = Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>;
impl Emitter {
    pub fn channel() -> (Self, mpsc::Receiver<Event>) {
        let (send, rx) = mpsc::channel();
        (
            Self {
                send,
                wake: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }
    pub fn set_waker(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.wake.lock().unwrap() = Some(Box::new(wake));
    }
    pub fn emit(&self, event: Event) {
        let _ = self.send.send(event);
        if let Some(w) = self.wake.lock().unwrap().as_ref() {
            w();
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct WindowState {
    #[serde(default)]
    pub toolkit: String,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub selected_count: usize,
    /// Active type filter macro, empty for all.
    #[serde(default)]
    pub filter: String,
    pub visible: bool,
    pub tray_available: bool,
    pub hotkey_status: String,
    pub query: String,
    pub matches: usize,
    pub settings_open: bool,
    pub owns_index: bool,
}
pub type SharedWindowState = Arc<Mutex<WindowState>>;
pub fn send_window(dir: &Path, action: &WindowAction) -> Result<WindowState> {
    let mut stream = UnixStream::connect(crate::store::socket_path(dir, "ui.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    serde_json::to_writer(&mut stream, action)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).take(65536).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}
pub struct Instance {
    _lock: File,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Instance {
    /// None means an existing window accepted activation.
    pub fn claim(
        dir: &Path,
        action: WindowAction,
        emitter: Emitter,
        state: SharedWindowState,
    ) -> Result<Option<Self>> {
        if send_window(dir, &action).is_ok() {
            return Ok(None);
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("ui.lock"))?;
        if lock.try_lock_exclusive().is_err() {
            for _ in 0..50 {
                if send_window(dir, &action).is_ok() {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(40));
            }
            bail!("Another window is starting but did not respond");
        }
        let socket = crate::store::socket_path(dir, "ui.sock");
        if socket.exists() {
            fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if ending.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let mut line = String::new();
                if BufReader::new(&stream)
                    .take(8193)
                    .read_line(&mut line)
                    .is_err()
                    || line.len() > 8192
                {
                    continue;
                }
                if let Ok(action) = serde_json::from_str::<WindowAction>(&line) {
                    if !matches!(action, WindowAction::Status) {
                        emitter.emit(Event::Action(action));
                    }
                    let current = state.lock().unwrap().clone();
                    if serde_json::to_writer(&mut stream, &current).is_ok() {
                        let _ = stream.write_all(b"\n");
                    }
                }
            }
        });
        Ok(Some(Self {
            _lock: lock,
            socket,
            stop,
            thread: Some(worker),
        }))
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.socket);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

/// The application icon, rendered from `packaging/file-minnow.svg` at the
/// usual icon sizes. PNG decoding is built into gdk-pixbuf; SVG would need
/// librsvg, which not every distribution installs.
const ICONS: [&[u8]; 8] = [
    include_bytes!("../packaging/icons/file-minnow-16.png"),
    include_bytes!("../packaging/icons/file-minnow-22.png"),
    include_bytes!("../packaging/icons/file-minnow-24.png"),
    include_bytes!("../packaging/icons/file-minnow-32.png"),
    include_bytes!("../packaging/icons/file-minnow-48.png"),
    include_bytes!("../packaging/icons/file-minnow-64.png"),
    include_bytes!("../packaging/icons/file-minnow-128.png"),
    include_bytes!("../packaging/icons/file-minnow-256.png"),
];
/// The icon at each size, for window icons.
pub fn icons() -> Vec<gtk::gdk_pixbuf::Pixbuf> {
    ICONS
        .iter()
        .filter_map(|png| gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(*png)).ok())
        .collect()
}
/// Tray icons up to 64 px as (size, ARGB32 in network byte order).
fn tray_icons() -> Vec<ksni::Icon> {
    icons()
        .into_iter()
        .filter(|icon| icon.width() <= 64 && icon.n_channels() == 4)
        .map(|icon| {
            let (width, height) = (icon.width(), icon.height());
            let stride = icon.rowstride() as usize;
            let bytes = icon.read_pixel_bytes();
            let mut data = Vec::with_capacity((width * height * 4) as usize);
            for row in bytes.chunks(stride).take(height as usize) {
                for rgba in row[..width as usize * 4].chunks_exact(4) {
                    data.extend_from_slice(&[rgba[3], rgba[0], rgba[1], rgba[2]]);
                }
            }
            ksni::Icon {
                width,
                height,
                data,
            }
        })
        .collect()
}
struct Tray {
    emitter: Emitter,
    paused: bool,
    icons: Vec<ksni::Icon>,
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "file-minnow".into()
    }
    fn title(&self) -> String {
        "File Minnow".into()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.emitter
            .emit(Event::Action(WindowAction::Show { query: None }));
    }
    fn icon_name(&self) -> String {
        // Used when installed (hicolor theme); the pixmaps cover portable runs.
        "file-minnow".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let item = |label: &str, action: WindowAction| {
            StandardItem {
                label: label.into(),
                activate: Box::new(move |tray: &mut Self| {
                    tray.emitter.emit(Event::Action(action.clone()))
                }),
                ..Default::default()
            }
            .into()
        };
        vec![
            item("Show File Minnow", WindowAction::Show { query: None }),
            item("Hide window", WindowAction::Hide),
            ksni::MenuItem::Separator,
            item(
                if self.paused {
                    "Resume indexing"
                } else {
                    "Pause indexing"
                },
                WindowAction::Pause,
            ),
            item("Rescan folders", WindowAction::Rescan),
            item("Settings", WindowAction::Settings),
            ksni::MenuItem::Separator,
            item("Quit File Minnow", WindowAction::Quit),
        ]
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.emitter.emit(Event::TrayAvailable(false));
        true
    }
    fn watcher_online(&self) {
        self.emitter.emit(Event::TrayAvailable(true));
    }
}
pub struct Integration {
    tray: Option<ksni::blocking::Handle<Tray>>,
    hotkey: Option<X11Hotkey>,
    portal: Option<PortalHotkey>,
    emitter: Emitter,
    last: Option<(bool, String)>,
    paused: bool,
}
impl Integration {
    pub fn new(emitter: Emitter) -> Self {
        Self {
            tray: None,
            hotkey: None,
            portal: None,
            emitter,
            last: None,
            paused: false,
        }
    }
    pub fn configure(&mut self, settings: &UiConfig) {
        let key = (settings.tray_enabled, settings.global_shortcut.clone());
        if self.last.as_ref() == Some(&key) {
            return;
        }
        if self.last.as_ref().map(|s| s.0) != Some(settings.tray_enabled) {
            if let Some(tray) = self.tray.take() {
                tray.shutdown();
            }
            self.emitter.emit(Event::TrayAvailable(false));
            if settings.tray_enabled {
                use ksni::blocking::TrayMethods;
                match (Tray {
                    emitter: self.emitter.clone(),
                    paused: self.paused,
                    icons: tray_icons(),
                })
                .spawn()
                {
                    Ok(tray) => {
                        self.tray = Some(tray);
                        self.emitter.emit(Event::TrayAvailable(true));
                    }
                    Err(error) => self.emitter.emit(Event::IntegrationError(format!(
                        "Tray unavailable: {error}. Closing the window will quit instead of hiding."
                    ))),
                }
            }
        }
        if self.last.as_ref().map(|s| s.1.as_str()) != Some(settings.global_shortcut.as_str()) {
            self.hotkey.take();
            self.portal.take();
            if settings.global_shortcut.trim().is_empty() {
                self.emitter
                    .emit(Event::HotkeyStatus("Global shortcut disabled".into()));
            } else if is_wayland() {
                match PortalHotkey::start(&settings.global_shortcut, self.emitter.clone()) {
                    Ok(h) => self.portal = Some(h),
                    Err(e) => self.emitter.emit(Event::HotkeyStatus(format!(
                        "Global shortcut unavailable: {e}"
                    ))),
                }
            } else {
                match X11Hotkey::start(&settings.global_shortcut,self.emitter.clone()){Ok(h)=>{self.hotkey=Some(h);self.emitter.emit(Event::HotkeyStatus(format!("{} (X11)",settings.global_shortcut)));},Err(e)=>self.emitter.emit(Event::HotkeyStatus(format!("Global shortcut unavailable: {e}. Choose another binding or assign 'file-minnow toggle' in your desktop keyboard settings.")))}
            }
        }
        self.last = Some(key);
    }
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused != paused {
            self.paused = paused;
            if let Some(tray) = &self.tray {
                tray.update(|t| t.paused = paused);
            }
        }
    }
}
impl Drop for Integration {
    fn drop(&mut self) {
        if let Some(tray) = self.tray.take() {
            tray.shutdown();
        }
    }
}
pub fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|s| s == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

use x11rb::{
    connection::Connection,
    protocol::{
        Event as XEvent, xkb,
        xproto::{
            AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, GrabMode,
            ModMask, WindowClass,
        },
    },
    rust_connection::RustConnection,
};
/// Whether one of this process's windows is the X11 active/focused window.
///
/// A key grab (including the global shortcut itself) sends GTK a synthetic
/// focus-out, so `gtk::Window::is_active` is false while the shortcut is held.
/// Grabs change neither `_NET_ACTIVE_WINDOW` nor the X input focus, so ask X.
/// Returns None when X11 cannot be queried; callers fall back to GTK state.
pub fn x11_focus_is_ours() -> Option<bool> {
    let (conn, screen) = RustConnection::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let atom = |name: &[u8]| -> Option<u32> {
        Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
    };
    let pid_atom = atom(b"_NET_WM_PID")?;
    let me = std::process::id();
    let owned = |window: u32| -> bool {
        conn.get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().and_then(|mut values| values.next()))
            == Some(me)
    };
    // EWMH window managers (Cinnamon/Muffin, Xfwm, KWin...) publish the active window.
    if let Some(active_atom) = atom(b"_NET_ACTIVE_WINDOW")
        && let Some(active) = conn
            .get_property(false, root, active_atom, AtomEnum::WINDOW, 0, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().and_then(|mut values| values.next()))
        && active != x11rb::NONE
    {
        return Some(owned(active));
    }
    // Without a window manager, walk from the input focus to the root.
    let mut window = conn.get_input_focus().ok()?.reply().ok()?.focus;
    for _ in 0..64 {
        // 0 is None and 1 is PointerRoot: nothing of ours is focused.
        if window <= 1 || window == root {
            return Some(false);
        }
        if owned(window) {
            return Some(true);
        }
        let parent = conn.query_tree(window).ok()?.reply().ok()?.parent;
        if parent == x11rb::NONE {
            return Some(false);
        }
        window = parent;
    }
    Some(false)
}
/// Asks the window manager to raise and focus `window` (an X11 window id).
///
/// GTK's own activation request needs the timestamp of the user's input. A
/// global shortcut, tray click or second launch is not a GTK input event, so
/// GTK sends none and focus-stealing prevention (Muffin, Mutter, KWin, Xfwm)
/// then only flags the window instead of focusing it. Task bars and pagers
/// send source indication 2 for a user's click; window managers honour that
/// without the timestamp check, which is the case here: the user asked for
/// the window through a shortcut or tray icon.
pub fn x11_activate(window: u32) -> Result<()> {
    let (conn, screen) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let atom = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    // data: source indication (2 = pager), timestamp (0 = current), requestor.
    let event = ClientMessageEvent::new(32, window, atom, [2_u32, 0, 0, 0, 0]);
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?
    .check()?;
    Ok(())
}
struct X11Hotkey {
    conn: Arc<RustConnection>,
    window: u32,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl X11Hotkey {
    fn start(binding: &str, emitter: Emitter) -> Result<Self> {
        let chord = Chord::parse(binding)?;
        let (conn, screen) = RustConnection::connect(None).context("Cannot connect to X11")?;
        let root = conn.setup().roots[screen].root;
        let _ = xkb::ConnectionExt::xkb_use_extension(&conn, 1, 0)?.reply()?;
        xkb::ConnectionExt::xkb_per_client_flags(
            &conn,
            xkb::ID::USE_CORE_KBD.into(),
            xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            Default::default(),
            Default::default(),
            Default::default(),
        )?
        .reply()?;
        let window = conn.generate_id()?;
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )?
        .check()?;
        let install = move |conn: &RustConnection| -> Result<u8> {
            let min = conn.setup().min_keycode;
            let count = conn.setup().max_keycode - min + 1;
            let mapping = conn.get_keyboard_mapping(min, count)?.reply()?;
            let code = mapping
                .keysyms
                .chunks(mapping.keysyms_per_keycode as usize)
                .position(|row| row.contains(&chord.keysym()))
                .map(|i| min + i as u8)
                .context("Shortcut key is absent from this keyboard layout")?;
            let mut mods = ModMask::default();
            if chord.ctrl {
                mods |= ModMask::CONTROL;
            }
            if chord.alt {
                mods |= ModMask::M1;
            }
            if chord.shift {
                mods |= ModMask::SHIFT;
            }
            if chord.super_key {
                mods |= ModMask::M4;
            }
            for locks in [
                ModMask::default(),
                ModMask::LOCK,
                ModMask::M2,
                ModMask::LOCK | ModMask::M2,
            ] {
                conn.grab_key(
                    false,
                    root,
                    mods | locks,
                    code,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?
                .check()
                .context("Shortcut is already in use")?;
            }
            conn.flush()?;
            Ok(code)
        };
        let mut code = install(&conn)?;
        let conn = Arc::new(conn);
        let connection = conn.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let thread = thread::spawn(move || {
            let mut pressed = false;
            while !ending.load(Ordering::Relaxed) {
                match connection.wait_for_event() {
                    Ok(XEvent::KeyPress(e)) if e.detail == code => {
                        if !pressed {
                            pressed = true;
                            emitter.emit(Event::Action(WindowAction::Toggle));
                        }
                    }
                    Ok(XEvent::KeyRelease(e)) if e.detail == code => pressed = false,
                    Ok(XEvent::MappingNotify(_)) => {
                        let _ = connection.ungrab_key(x11rb::NONE as u8, root, ModMask::ANY);
                        if let Ok(new) = install(&connection) {
                            code = new;
                        } else {
                            emitter.emit(Event::HotkeyStatus(
                                "Keyboard layout changed; reselect the global shortcut".into(),
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = connection.destroy_window(window);
            let _ = connection.flush();
        });
        Ok(Self {
            conn,
            window,
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for X11Hotkey {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let event = ClientMessageEvent::new(32, self.window, 0_u32, [0_u32; 5]);
        let _ = self
            .conn
            .send_event(false, self.window, EventMask::NO_EVENT, event);
        let _ = self.conn.flush();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct PortalHotkey {
    stop: async_channel::Sender<()>,
}
impl PortalHotkey {
    fn start(binding: &str, emitter: Emitter) -> Result<Self> {
        let chord = Chord::parse(binding)?;
        let trigger = chord.portal_trigger();
        let (stop, ending) = async_channel::bounded(1);
        thread::spawn(move || {
            let result: Result<()> = async_io::block_on(async {
                use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
                use futures_lite::StreamExt;
                let connection = ashpd::zbus::Connection::session().await?;
                let portal = GlobalShortcuts::with_connection(connection).await?;
                let session = portal.create_session(Default::default()).await?;
                let result = futures_lite::future::race(
                    async {
                        let mut events = portal.receive_activated().await?;
                        portal
                            .bind_shortcuts(
                                &session,
                                &[NewShortcut::new("toggle", "Show or hide File Minnow")
                                    .preferred_trigger(Some(trigger.as_str()))],
                                None,
                                Default::default(),
                            )
                            .await?
                            .response()?;
                        emitter.emit(Event::HotkeyStatus(
                            "Global shortcut bound through the desktop portal".into(),
                        ));
                        while let Some(event) = events.next().await {
                            if event.shortcut_id() == "toggle" {
                                let token = event
                                    .options()
                                    .get("activation_token")
                                    .and_then(|v| <&str>::try_from(v).ok())
                                    .filter(|s| s.len() < 4096 && !s.contains('\0'))
                                    .map(str::to_owned);
                                emitter.emit(Event::PortalActivation(token));
                            }
                        }
                        Ok::<(), anyhow::Error>(())
                    },
                    async {
                        let _ = ending.recv().await;
                        Ok(())
                    },
                )
                .await;
                let _ = session.close().await;
                result
            });
            if let Err(e) = result {
                emitter.emit(Event::HotkeyStatus(format!("Wayland shortcut portal unavailable: {e}. Assign 'file-minnow toggle' in desktop keyboard settings.")));
            }
        });
        Ok(Self { stop })
    }
}
impl Drop for PortalHotkey {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
    }
}
