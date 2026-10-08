//! Compact GTK desktop frontend. GTK owns drawing, selection, focus and modality.
use crate::{
    actions::{self, Action, Item, Script},
    client::{Backend, Session, Subscription},
    config::{self, Settings},
    desktop::{self, Emitter, Event, Instance, Integration, WindowAction, WindowState},
    file_ops,
    index::{Entry, Kind},
    query::{SearchRequest, SearchResponse, Sort},
    settings_ui::OptionsWindow,
};
use anyhow::Result;
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::{Rc, Weak},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

/// Window size, position, maximized state, column widths and sort order.
/// Kept apart from the settings file: it changes often and must not race
/// with an open Options dialog.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct WindowMemory {
    width: i32,
    height: i32,
    x: Option<i32>,
    y: Option<i32>,
    maximized: bool,
    columns: Vec<i32>,
    sort: Option<Sort>,
    descending: bool,
    /// The type filter's search macro (`pic:`...), empty for all.
    filter: String,
}
impl WindowMemory {
    fn path(dir: &std::path::Path) -> PathBuf {
        config::ConfigStore::for_data_dir(dir)
            .path
            .with_file_name("window.json")
    }
    fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .filter(|bytes| bytes.len() < 64 * 1024)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    fn usable_size(&self) -> Option<(i32, i32)> {
        ((400..=16_384).contains(&self.width) && (300..=16_384).contains(&self.height))
            .then_some((self.width, self.height))
    }
}
/// The saved position, if part of the window would still be on a monitor
/// (a display may have been unplugged since).
fn on_screen(x: i32, y: i32, width: i32) -> bool {
    let Some(display) = gdk::Display::default() else {
        return false;
    };
    (0..display.n_monitors()).any(|i| {
        display.monitor(i).is_some_and(|m| {
            let g = m.geometry();
            let (px, py) = (x + width.min(200) / 2, y + 20);
            px >= g.x() && px < g.x() + g.width() && py >= g.y() && py < g.y() + g.height()
        })
    })
}

type AppRef = Rc<RefCell<App>>;
type AppWeak = Weak<RefCell<App>>;
#[derive(Default)]
pub struct LaunchOptions {
    pub query: Option<String>,
    pub toggle: bool,
    pub hidden: bool,
}
struct Job {
    id: u64,
    request: SearchRequest,
}
pub(crate) enum Message {
    Backend,
    Desktop,
    Search(u64, Result<SearchResponse, String>),
    Settings(Box<Result<Settings, String>>, bool),
    Error(String),
    SuspendShortcut(bool),
    SaveWindow,
}
pub(crate) struct App {
    _session: Session,
    backend: Backend,
    _instance: Instance,
    _subscription: Subscription,
    integration: Integration,
    desktop_events: mpsc::Receiver<Event>,
    window_state: desktop::SharedWindowState,
    window: gtk::Window,
    entry: gtk::SearchEntry,
    /// Type filter next to the search box; its id is the search macro.
    filter: gtk::ComboBoxText,
    tree: gtk::TreeView,
    scroller: gtk::ScrolledWindow,
    data: Rc<RefCell<Option<SearchResponse>>>,
    status: gtk::Label,
    counts: gtk::Label,
    spinner: gtk::Spinner,
    pager: gtk::Box,
    previous: gtk::Button,
    next: gtk::Button,
    page_label: gtk::Label,
    jobs: mpsc::Sender<Job>,
    messages: async_channel::Sender<Message>,
    query_version: Arc<AtomicU64>,
    debounce: Option<(glib::SourceId, Rc<Cell<bool>>)>,
    options: Option<OptionsWindow>,
    applying: bool,
    pending: bool,
    sort: Sort,
    descending: bool,
    offset: usize,
    last_generation: u64,
    tray_available: bool,
    quitting: bool,
    hide_on_ready: bool,
    shortcut_suspended: bool,
    hotkey_status: String,
    system_theme: String,
    system_dark: bool,
    memory: WindowMemory,
    memory_path: PathBuf,
    /// Data directory, for the login entry when it is not the default.
    data_dir: PathBuf,
    memory_timer: Option<glib::SourceId>,
    /// Set when the window is shown from hidden; the map handler then asks
    /// the window manager for focus (see `desktop::x11_activate`).
    activate_on_map: Rc<Cell<bool>>,
    /// The right-click menu, rebuilt for each selection.
    context_menu: Option<gtk::Menu>,
    /// The request whose results are displayed, and the one in flight; when
    /// they match, a refresh keeps the selection and scroll position.
    shown_key: Option<(String, Sort, bool, usize)>,
    requested_key: (String, Sort, bool, usize),
}
impl Drop for App {
    fn drop(&mut self) {
        self.query_version.fetch_add(1, Ordering::Relaxed);
        if let Some((timer, fired)) = self.debounce.take()
            && !fired.get()
        {
            timer.remove();
        }
    }
}
fn with_app(weak: &AppWeak, f: impl FnOnce(&mut App)) {
    if let Some(app) = weak.upgrade()
        && let Ok(mut app) = app.try_borrow_mut()
    {
        f(&mut app);
    }
}
fn count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
fn size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1048576.)
    } else {
        format!("{:.1} GB", n as f64 / 1073741824.)
    }
}
fn modified(e: &Entry) -> String {
    chrono::DateTime::from_timestamp(e.modified, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn run(roots: Vec<PathBuf>, dir: PathBuf) -> Result<()> {
    run_with_options(roots, dir, LaunchOptions::default())
}
pub fn run_with_options(roots: Vec<PathBuf>, dir: PathBuf, launch: LaunchOptions) -> Result<()> {
    let (emitter, desktop_events) = Emitter::channel();
    let window_state = Arc::new(Mutex::new(WindowState {
        toolkit: "gtk3".into(),
        ..Default::default()
    }));
    let action = if launch.toggle {
        WindowAction::Toggle
    } else {
        WindowAction::Show {
            query: launch.query.clone(),
        }
    };
    let Some(instance) = Instance::claim(&dir, action, emitter.clone(), window_state.clone())?
    else {
        return Ok(());
    };
    gtk::init().map_err(|e| anyhow::anyhow!("Cannot open the desktop display: {e}"))?;
    glib::set_application_name("File Minnow");
    let application = gtk::Application::new(
        Some("org.fileminnow.FileMinnow"),
        gio::ApplicationFlags::NON_UNIQUE,
    );
    application.register(None::<&gio::Cancellable>)?;
    gdk::set_program_class("FileMinnow");
    let gtk_settings = gtk::Settings::default().expect("GTK settings");
    let system_theme = gtk_settings.property::<String>("gtk-theme-name");
    let system_dark = gtk_settings.property::<bool>("gtk-application-prefer-dark-theme");
    gtk_settings.set_property("gtk-enable-animations", false);
    let session = Session::connect(&roots, &dir)?;
    let backend = session.backend.clone();
    let settings = backend.settings();
    window_state.lock().unwrap().owns_index = session.owns_index();
    let (messages, receiver) = async_channel::unbounded();
    let sender = messages.clone();
    emitter.set_waker(move || {
        let _ = sender.try_send(Message::Desktop);
    });
    let sender = messages.clone();
    let subscription = backend.on_change(move || {
        let _ = sender.try_send(Message::Backend);
    });
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_application(Some(&application));
    window.set_title("File Minnow");
    window.set_default_size(960, 640);
    window.set_position(gtk::WindowPosition::Center);
    // Every window (Options, dialogs) uses the application icon.
    gtk::Window::set_default_icon_list(&desktop::icons());
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&layout);
    let menubar = gtk::MenuBar::new();
    layout.pack_start(&menubar, false, false, 0);
    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Search files and folders"));
    entry.set_margin_start(6);
    entry.set_margin_end(6);
    entry.set_margin_top(4);
    entry.set_margin_bottom(4);
    entry.set_hexpand(true);
    let filter = gtk::ComboBoxText::new();
    filter.append(Some(""), "All");
    for (label, macro_, _) in crate::query::TYPE_FILTERS.iter().take(3) {
        filter.append(Some(macro_), label);
    }
    filter.append(Some("folder:"), "Folder");
    for (label, macro_, _) in crate::query::TYPE_FILTERS.iter().skip(3) {
        filter.append(Some(macro_), label);
    }
    filter.set_active_id(Some(""));
    filter.set_tooltip_text(Some("Show only one type of file"));
    filter.set_focus_on_click(false);
    filter.set_margin_end(6);
    filter.set_margin_top(4);
    filter.set_margin_bottom(4);
    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    search_row.pack_start(&entry, true, true, 0);
    search_row.pack_start(&filter, false, false, 0);
    layout.pack_start(&search_row, false, false, 0);
    let empty_model = gtk::ListStore::new(&[u32::static_type()]);
    let tree = gtk::TreeView::with_model(&empty_model);
    tree.set_headers_visible(true);
    tree.set_enable_search(false);
    tree.set_fixed_height_mode(true);
    tree.set_rubber_banding(true);
    tree.selection().set_mode(gtk::SelectionMode::Multiple);
    tree.set_headers_clickable(true);
    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_shadow_type(gtk::ShadowType::In);
    scroller.set_kinetic_scrolling(false);
    scroller.set_overlay_scrolling(false);
    scroller.add(&tree);
    layout.pack_start(&scroller, true, true, 0);
    let pager = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    pager.set_margin_start(6);
    pager.set_margin_end(6);
    pager.set_margin_top(3);
    pager.set_margin_bottom(3);
    let previous = gtk::Button::with_label("Previous");
    let next = gtk::Button::with_label("Next");
    let page_label = gtk::Label::new(None);
    pager.pack_start(&previous, false, false, 0);
    pager.pack_start(&page_label, true, true, 0);
    pager.pack_end(&next, false, false, 0);
    layout.pack_start(&pager, false, false, 0);
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bottom.set_margin_start(6);
    bottom.set_margin_end(6);
    bottom.set_margin_top(3);
    bottom.set_margin_bottom(3);
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(16, 16);
    let status = gtk::Label::new(Some("Preparing index…"));
    status.set_xalign(0.);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let counts = gtk::Label::new(None);
    bottom.pack_start(&spinner, false, false, 0);
    bottom.pack_start(&status, true, true, 0);
    bottom.pack_end(&counts, false, false, 0);
    layout.pack_start(&bottom, false, false, 0);
    let data = Rc::new(RefCell::new(None));
    let query_version = Arc::new(AtomicU64::new(0));
    let (jobs, queue) = mpsc::channel::<Job>();
    let service = backend.clone();
    let version = query_version.clone();
    let sender = messages.clone();
    std::thread::spawn(move || {
        while let Ok(mut job) = queue.recv() {
            while let Ok(new) = queue.try_recv() {
                job = new;
            }
            let result = service
                .search(&job.request, Some((&version, job.id)))
                .map_err(|e| e.to_string());
            if sender
                .send_blocking(Message::Search(job.id, result))
                .is_err()
            {
                break;
            }
        }
    });
    let app = Rc::new(RefCell::new(App {
        _session: session,
        backend,
        _instance: instance,
        _subscription: subscription,
        integration: Integration::new(emitter),
        desktop_events,
        window_state,
        window: window.clone(),
        entry: entry.clone(),
        filter: filter.clone(),
        tree: tree.clone(),
        scroller,
        data,
        status,
        counts,
        spinner,
        pager,
        previous: previous.clone(),
        next: next.clone(),
        page_label,
        jobs,
        messages,
        query_version,
        debounce: None,
        options: None,
        applying: false,
        pending: false,
        sort: Sort::Name,
        descending: false,
        offset: 0,
        last_generation: u64::MAX,
        tray_available: false,
        quitting: false,
        hide_on_ready: launch.hidden || settings.ui.start_hidden,
        shortcut_suspended: false,
        hotkey_status: String::new(),
        system_theme,
        system_dark,
        memory: WindowMemory::default(),
        memory_path: WindowMemory::path(&dir),
        data_dir: dir.clone(),
        memory_timer: None,
        activate_on_map: Rc::new(Cell::new(false)),
        context_menu: None,
        shown_key: None,
        requested_key: (String::new(), Sort::Name, false, 0),
    }));
    let weak = Rc::downgrade(&app);
    build_columns(&app);
    build_menus(&app, &menubar);
    connect_results(&app);
    let target = weak.clone();
    entry.connect_changed(move |_| {
        with_app(&target, |a| {
            a.offset = 0;
            a.schedule();
        })
    });
    let target = weak.clone();
    filter.connect_changed(move |_| {
        with_app(&target, |a| {
            a.offset = 0;
            a.remember();
            a.schedule();
        })
    });
    let target = weak.clone();
    entry.connect_key_press_event(move |_, event| {
        if event.keyval() == gdk::keys::constants::Down {
            with_app(&target, |a| {
                a.tree.grab_focus();
                if a.tree.selection().selected_rows().0.is_empty() {
                    a.tree.set_cursor(
                        &gtk::TreePath::from_indicesv(&[0]),
                        None::<&gtk::TreeViewColumn>,
                        false,
                    );
                }
            });
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    let target = weak.clone();
    entry.connect_activate(move |_| with_app(&target, |a| a.open_selected(false)));
    let target = weak.clone();
    previous.connect_clicked(move |_| {
        with_app(&target, |a| {
            a.offset = a.offset.saturating_sub(10_000);
            a.schedule();
        })
    });
    let target = weak.clone();
    next.connect_clicked(move |_| {
        with_app(&target, |a| {
            a.offset += 10_000;
            a.schedule();
        })
    });
    let target = weak.clone();
    window.connect_delete_event(move |_, _| {
        let mut hide = false;
        with_app(&target, |a| {
            hide = !a.quitting && a.tray_available && a.backend.settings().ui.close_to_tray;
            if hide {
                a.hide();
            } else {
                a.quitting = true;
                gtk::main_quit();
            }
        });
        if hide {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    let pending = app.borrow().activate_on_map.clone();
    window.connect_map_event(move |window, _| {
        if pending.replace(false) {
            activate(window);
        }
        glib::Propagation::Proceed
    });
    let target = weak.clone();
    window.connect_key_press_event(move |window, event| {
        // Escape closes the window like its close button: to the tray when
        // the tray is in use, otherwise the application quits.
        if event.keyval() == gdk::keys::constants::Escape
            && !event.state().intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::MOD1_MASK
                    | gdk::ModifierType::SHIFT_MASK,
            )
        {
            window.close();
            return glib::Propagation::Stop;
        }
        if event.state().contains(gdk::ModifierType::CONTROL_MASK) {
            match event.keyval() {
                gdk::keys::constants::l | gdk::keys::constants::L => {
                    with_app(&target, |a| a.focus_search());
                    return glib::Propagation::Stop;
                }
                gdk::keys::constants::comma => {
                    if let Some(a) = target.upgrade() {
                        App::options(&a);
                    }
                    return glib::Propagation::Stop;
                }
                gdk::keys::constants::q | gdk::keys::constants::Q => {
                    with_app(&target, App::quit);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }
        if event.keyval() == gdk::keys::constants::F5 {
            with_app(&target, App::rescan);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    let target = weak.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(message) = receiver.recv().await {
            let Some(app) = target.upgrade() else {
                break;
            };
            App::message(&app, message);
        }
    });
    restore_window(&app);
    {
        let mut a = app.borrow_mut();
        a.apply_theme(&settings);
        a.integration.configure(&settings.ui);
        a.window.show_all();
        a.pager.hide();
        a.entry.set_text(launch.query.as_deref().unwrap_or(""));
        a.focus_search();
        a.backend_changed();
        a.schedule();
        a.publish();
    }
    gtk::main();
    {
        let mut a = app.borrow_mut();
        a.quitting = true;
        a.save_memory();
    }
    drop(app);
    Ok(())
}

/// Applies the saved window state and starts tracking changes to it.
fn restore_window(app: &AppRef) {
    let a = &mut *app.borrow_mut();
    let memory = WindowMemory::load(&a.memory_path);
    if let Some((width, height)) = memory.usable_size() {
        a.window.set_default_size(width, height);
        // Positions are only honored on X11; Wayland places windows itself.
        if let (Some(x), Some(y)) = (memory.x, memory.y)
            && on_screen(x, y, width)
        {
            a.window.set_position(gtk::WindowPosition::None);
            a.window.move_(x, y);
        }
    }
    if memory.maximized {
        a.window.maximize();
    }
    for (column, width) in a.tree.columns().iter().zip(&memory.columns) {
        if (30..=4000).contains(width) {
            column.set_fixed_width(*width);
        }
    }
    if let Some(sort) = memory.sort {
        a.sort = sort;
        a.descending = memory.descending;
        a.show_sort();
    }
    if !memory.filter.is_empty() {
        a.filter.set_active_id(Some(&memory.filter));
    }
    a.memory = memory;
    let weak = Rc::downgrade(app);
    a.window.connect_configure_event(move |window, _| {
        with_app(&weak, |a| {
            if !a.memory.maximized && window.is_visible() {
                let (width, height) = window.size();
                let (x, y) = window.position();
                a.memory.width = width;
                a.memory.height = height;
                a.memory.x = Some(x);
                a.memory.y = Some(y);
                a.remember();
            }
        });
        false
    });
    let weak = Rc::downgrade(app);
    a.window.connect_window_state_event(move |_, event| {
        with_app(&weak, |a| {
            a.memory.maximized = event
                .new_window_state()
                .contains(gdk::WindowState::MAXIMIZED);
            a.remember();
        });
        glib::Propagation::Proceed
    });
    for column in a.tree.columns() {
        let weak = Rc::downgrade(app);
        column.connect_width_notify(move |_| with_app(&weak, App::remember));
    }
}

/// The Type column text: the desktop's description of the file type,
/// computed once per extension and kept for the session.
fn type_name(cache: &RefCell<std::collections::HashMap<Vec<u8>, String>>, e: &Entry) -> String {
    match e.kind {
        Kind::Folder => return "Folder".into(),
        Kind::Link => return "Link".into(),
        _ => {}
    }
    let name = e.name();
    let ext = crate::query::extension(name).to_ascii_lowercase();
    if ext.is_empty() {
        return "File".into();
    }
    cache
        .borrow_mut()
        .entry(ext.clone())
        .or_insert_with(|| {
            let sample = format!("file.{}", String::from_utf8_lossy(&ext));
            // GIO marks name-only guesses uncertain when several types share
            // an extension (PNG/APNG); its first answer still names the type.
            match file_ops::guess_type(std::path::Path::new(&sample)) {
                (kind, _) if file_ops::known_type(&kind) => {
                    gio::content_type_get_description(&kind).to_string()
                }
                _ => format!("{} file", String::from_utf8_lossy(&ext).to_uppercase()),
            }
        })
        .clone()
}
fn build_columns(app: &AppRef) {
    let a = app.borrow();
    let types: Rc<RefCell<std::collections::HashMap<Vec<u8>, String>>> = Rc::default();
    for (number, title, width, sort) in [
        (0, "Name", 300, Sort::Name),
        (1, "Path", 400, Sort::Path),
        (2, "Size", 95, Sort::Size),
        (3, "Modified", 155, Sort::Modified),
        (4, "Type", 140, Sort::Type),
    ] {
        let column = gtk::TreeViewColumn::new();
        column.set_title(title);
        column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        column.set_fixed_width(width);
        column.set_resizable(true);
        column.set_clickable(true);
        column.set_min_width(50);
        if number == 0 {
            let icon = gtk::CellRendererPixbuf::new();
            icon.set_property("stock-size", i32::from(gtk::IconSize::Menu) as u32);
            gtk::prelude::TreeViewColumnExt::pack_start(&column, &icon, false);
            let rows = a.data.clone();
            gtk::prelude::TreeViewColumnExt::set_cell_data_func(
                &column,
                &icon,
                Some(Box::new(move |_, cell, model, iter| {
                    let Ok(i) = model.value(iter, 0).get::<u32>() else {
                        return;
                    };
                    let rows = rows.borrow();
                    let Some(e) = rows.as_ref().and_then(|r| r.entries.get(i as usize)) else {
                        return;
                    };
                    let icon = cell.downcast_ref::<gtk::CellRendererPixbuf>().unwrap();
                    let gicon: gio::Icon = if e.kind == "folder" {
                        gio::ThemedIcon::new("folder").upcast()
                    } else if e.kind == "link" {
                        gio::ThemedIcon::new("emblem-symbolic-link").upcast()
                    } else {
                        let (kind, _) = file_ops::guess_type(&e.path_buf());
                        gio::content_type_get_icon(&kind)
                    };
                    icon.set_property("gicon", gicon);
                })),
            );
        }
        let text = gtk::CellRendererText::new();
        text.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
        text.set_property("single-paragraph-mode", true);
        text.set_property("ypad", 1_u32);
        text.set_property("xpad", 3_u32);
        if number == 2 {
            text.set_property("xalign", 1_f32);
        }
        gtk::prelude::TreeViewColumnExt::pack_start(&column, &text, true);
        let rows = a.data.clone();
        let types = types.clone();
        gtk::prelude::TreeViewColumnExt::set_cell_data_func(
            &column,
            &text,
            Some(Box::new(move |_, cell, model, iter| {
                let Ok(i) = model.value(iter, 0).get::<u32>() else {
                    return;
                };
                let rows = rows.borrow();
                let Some(e) = rows.as_ref().and_then(|r| r.entries.get(i as usize)) else {
                    return;
                };
                let value = match number {
                    0 => e.display_name(),
                    1 => e
                        .path_buf()
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    2 => {
                        if e.kind == "folder" {
                            String::new()
                        } else {
                            size(e.size)
                        }
                    }
                    3 => modified(e),
                    _ => type_name(&types, e),
                };
                cell.set_property("text", value);
            })),
        );
        a.tree.append_column(&column);
        let weak = Rc::downgrade(app);
        column.connect_clicked(move |_| {
            with_app(&weak, |a| {
                if a.sort == sort {
                    a.descending = !a.descending;
                } else {
                    a.sort = sort;
                    a.descending = matches!(sort, Sort::Size | Sort::Modified);
                }
                a.offset = 0;
                a.show_sort();
                a.remember();
                a.schedule();
            })
        });
    }
}
/// Gives `window` the keyboard focus through the window manager on X11.
/// Without a user timestamp, `gtk::Window::present` alone is refused by
/// focus-stealing prevention when the request comes from a global shortcut,
/// the tray or a second launch.
fn activate(window: &gtk::Window) {
    if let Some(xid) = x11_id(window) {
        // Requests on our own GDK connection must reach the server first.
        window.display().flush();
        let _ = desktop::x11_activate(xid);
    }
}
/// The X11 window id of a realized window; None on Wayland.
fn x11_id(window: &gtk::Window) -> Option<u32> {
    use glib::translate::ToGlibPtr;
    unsafe extern "C" {
        fn gdk_x11_window_get_xid(window: *mut gdk::ffi::GdkWindow) -> std::os::raw::c_ulong;
    }
    let surface = window.window()?;
    if surface.display().type_().name() != "GdkX11Display" {
        return None;
    }
    // SAFETY: `surface` is a live GdkWindow of an X11 display (checked above).
    let xid = unsafe { gdk_x11_window_get_xid(surface.to_glib_none().0) };
    u32::try_from(xid).ok()
}
fn item(menu: &gtk::Menu, label: &str, action: impl Fn() + 'static) {
    let item = gtk::MenuItem::with_mnemonic(label);
    item.connect_activate(move |_| action());
    menu.append(&item);
}
fn build_menus(app: &AppRef, bar: &gtk::MenuBar) {
    for name in ["_File", "_Edit", "_Search", "_Tools", "_Help"] {
        let root = gtk::MenuItem::with_mnemonic(name);
        let menu = gtk::Menu::new();
        root.set_submenu(Some(&menu));
        bar.append(&root);
        let w = Rc::downgrade(app);
        match name {
            "_File" => {
                let t = w.clone();
                item(&menu, "_Add Folder…", move || {
                    if let Some(a) = t.upgrade() {
                        App::choose_folder(&a);
                    }
                });
                let t = w.clone();
                item(&menu, "_Open", move || {
                    with_app(&t, |a| a.open_selected(false))
                });
                let t = w.clone();
                item(&menu, "Open Containing _Folder", move || {
                    with_app(&t, |a| a.open_selected(true))
                });
                item(&menu, "_Quit", move || with_app(&w, App::quit));
            }
            "_Edit" => {
                let t = w.clone();
                item(&menu, "Copy Full _Path", move || {
                    with_app(&t, |a| a.copy_selected(false))
                });
                let t = w.clone();
                item(&menu, "Copy _Name", move || {
                    with_app(&t, |a| a.copy_selected(true))
                });
                item(&menu, "Select _All", move || {
                    with_app(&w, |a| a.tree.selection().select_all())
                });
            }
            "_Search" => {
                let t = w.clone();
                item(&menu, "_Focus Search", move || {
                    with_app(&t, App::focus_search)
                });
                item(&menu, "_Clear Search", move || {
                    with_app(&w, |a| {
                        a.entry.set_text("");
                        a.offset = 0;
                        a.schedule();
                        a.focus_search();
                    })
                });
            }
            "_Tools" => {
                let t = w.clone();
                item(&menu, "_Options…", move || {
                    if let Some(a) = t.upgrade() {
                        App::options(&a);
                    }
                });
                let t = w.clone();
                item(&menu, "_Rescan", move || with_app(&t, App::rescan));
                item(&menu, "_Pause / Resume Indexing", move || {
                    with_app(&w, App::pause)
                });
            }
            _ => {
                let t = w.clone();
                item(&menu, "Search _Syntax", move || {
                    with_app(&t, |a| {
                        a.info("Search syntax","invoice    Filename contains invoice\next:rs;toml    File extensions\nfile: / folder:    Entry type\npath:/Projects    Search full paths\ncase:README    Case-sensitive\nsize:>10mb    Size filter\ndm:today    Modified today\nregex:\"^report.*pdf$\"    Regular expression\n!draft    Exclude a term\n<apple banana> | orange    Grouping\n\nOR has precedence over AND.")
                    })
                });
                item(&menu, "_About", move || {
                    with_app(&w, |a| {
                        let d = gtk::AboutDialog::new();
                        d.set_transient_for(Some(&a.window));
                        d.set_modal(true);
                        d.set_program_name("File Minnow");
                        d.set_version(Some(env!("CARGO_PKG_VERSION")));
                        d.set_comments(Some("Fast local file search"));
                        d.connect_response(|d, _| d.close());
                        d.show();
                    })
                });
            }
        }
    }
}
fn connect_results(app: &AppRef) {
    let tree = app.borrow().tree.clone();
    let w = Rc::downgrade(app);
    tree.connect_row_activated(move |_, _, _| with_app(&w, |a| a.open_selected(false)));
    let w = Rc::downgrade(app);
    tree.selection()
        .connect_changed(move |_| with_app(&w, App::publish));
    let w = Rc::downgrade(app);
    tree.connect_key_press_event(move |_, e| {
        use gdk::keys::constants as key;
        let mods = e.state()
            & (gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::MOD1_MASK
                | gdk::ModifierType::SHIFT_MASK);
        let ctrl = mods == gdk::ModifierType::CONTROL_MASK;
        match e.keyval() {
            key::c | key::C if ctrl => with_app(&w, |a| a.clipboard(false)),
            key::x | key::X if ctrl => with_app(&w, |a| a.clipboard(true)),
            key::a | key::A if ctrl => with_app(&w, |a| a.tree.selection().select_all()),
            key::Delete | key::KP_Delete if mods.is_empty() => with_app(&w, App::trash_selected),
            key::F2 if mods.is_empty() => with_app(&w, App::rename_selected),
            key::Return | key::KP_Enter if mods == gdk::ModifierType::MOD1_MASK => {
                with_app(&w, App::properties)
            }
            key::Menu => popup(&w, None),
            key::F10 if mods == gdk::ModifierType::SHIFT_MASK => popup(&w, None),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    let selection = tree.selection();
    let w = Rc::downgrade(app);
    tree.connect_button_press_event(move |tree, e| {
        if e.button() == 3 {
            let (x, y) = e.position();
            if let Some((Some(path), _, _, _)) = tree.path_at_pos(x as i32, y as i32)
                && !selection.path_is_selected(&path)
            {
                selection.unselect_all();
                selection.select_path(&path);
            }
            popup(&w, Some(e));
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}
/// Adds a menu item with an optional shortcut hint (the shortcut itself is
/// handled by the result list).
fn menu_item(
    menu: &gtk::Menu,
    label: &str,
    accel: Option<(gdk::keys::Key, gdk::ModifierType)>,
    action: impl Fn() + 'static,
) -> gtk::MenuItem {
    use glib::translate::IntoGlib;
    let item = gtk::MenuItem::with_mnemonic(label);
    if let Some((key, mods)) = accel
        && let Some(label) = item
            .child()
            .and_then(|c| c.downcast::<gtk::AccelLabel>().ok())
    {
        label.set_accel(key.into_glib(), mods);
    }
    item.connect_activate(move |_| action());
    menu.append(&item);
    item
}
fn separator(menu: &gtk::Menu) {
    if menu
        .children()
        .last()
        .is_some_and(|w| !w.is::<gtk::SeparatorMenuItem>())
    {
        menu.append(&gtk::SeparatorMenuItem::new());
    }
}
/// The right-click menu: File Minnow's own items, the applications that
/// open the selection, the usual file manager operations, then the
/// file manager's actions and scripts (see `actions`), as in Nemo.
fn popup(w: &AppWeak, event: Option<&gdk::EventButton>) {
    let Some(app) = w.upgrade() else {
        return;
    };
    let items = app.borrow().selected_items();
    if items.is_empty() {
        return;
    }
    let menu = gtk::Menu::new();
    let none = gdk::ModifierType::empty();
    let ctrl = gdk::ModifierType::CONTROL_MASK;
    use gdk::keys::constants as key;
    let on = |f: fn(&mut App)| {
        let w = w.clone();
        move || with_app(&w, f)
    };
    menu_item(
        &menu,
        "_Open",
        Some((key::Return, none)),
        on(|a| a.open_selected(false)),
    );
    // Open With: the desktop's applications for this type, default first.
    let kind = items[0].mime.clone();
    if items.iter().all(|i| i.mime == kind) {
        let submenu = gtk::Menu::new();
        let default = gio::AppInfo::default_for_type(&kind, false);
        let mut apps: Vec<gio::AppInfo> = default.iter().cloned().collect();
        for app in gio::AppInfo::all_for_type(&kind) {
            if app.should_show() && !apps.iter().any(|a| a.id() == app.id()) {
                apps.push(app);
            }
        }
        for info in apps {
            let w = w.clone();
            let label = info.display_name().replace('_', "__");
            menu_item(&submenu, &label, None, move || {
                with_app(&w, |a| a.open_with(&info))
            });
        }
        if items.len() == 1 {
            separator(&submenu);
            menu_item(
                &submenu,
                "Other _Application…",
                None,
                on(App::choose_application),
            );
        }
        if !submenu.children().is_empty() {
            let open_with = gtk::MenuItem::with_mnemonic("Open _With");
            open_with.set_submenu(Some(&submenu));
            menu.append(&open_with);
        }
    }
    menu_item(
        &menu,
        "Open Containing _Folder",
        None,
        on(|a| a.show_in_folder()),
    );
    menu_item(&menu, "Open in Ter_minal", None, on(App::terminal_here));
    separator(&menu);
    menu_item(
        &menu,
        "Cu_t",
        Some((key::x, ctrl)),
        on(|a| a.clipboard(true)),
    );
    menu_item(
        &menu,
        "_Copy",
        Some((key::c, ctrl)),
        on(|a| a.clipboard(false)),
    );
    menu_item(
        &menu,
        "Copy Full _Path",
        None,
        on(|a| a.copy_selected(false)),
    );
    menu_item(&menu, "Copy _Name", None, on(|a| a.copy_selected(true)));
    separator(&menu);
    if items.len() == 1 {
        menu_item(
            &menu,
            "_Rename…",
            Some((key::F2, none)),
            on(App::rename_selected),
        );
    }
    menu_item(
        &menu,
        "Mo_ve to Trash",
        Some((key::Delete, none)),
        on(App::trash_selected),
    );
    separator(&menu);
    let window = app.borrow().window.clone();
    // The default file manager decides whose actions and scripts appear.
    let manager = actions::Manager::from_desktop_id(
        gio::AppInfo::default_for_type("inode/directory", false)
            .and_then(|a| a.id())
            .as_deref(),
    );
    let mut submenus: Vec<(String, gtk::Menu)> = Vec::new();
    for action in actions::load(manager) {
        if !action.applies(&items, &file_ops::bus_has_owner) {
            continue;
        }
        let target = match &action.submenu {
            None => menu.clone(),
            Some(label) => match submenus.iter().find(|(l, _)| l == label) {
                Some((_, submenu)) => submenu.clone(),
                None => {
                    let submenu = gtk::Menu::new();
                    let item = gtk::MenuItem::with_label(label);
                    item.set_submenu(Some(&submenu));
                    menu.append(&item);
                    submenus.push((label.clone(), submenu.clone()));
                    submenu
                }
            },
        };
        let w = w.clone();
        let tip = action.comment.clone();
        let item = menu_item(&target, &action.label(&items), None, {
            let window = window.clone();
            move || with_app(&w, |a| a.run_action(&action, x11_id(&window)))
        });
        if !tip.is_empty() {
            item.set_tooltip_text(Some(&tip));
        }
    }
    let scripts = actions::scripts(manager);
    if !scripts.is_empty() {
        let submenu = gtk::Menu::new();
        for script in scripts {
            let w = w.clone();
            menu_item(&submenu, &script.name.clone(), None, move || {
                with_app(&w, |a| a.run_script(&script))
            });
        }
        let item = gtk::MenuItem::with_mnemonic("_Scripts");
        item.set_submenu(Some(&submenu));
        menu.append(&item);
    }
    separator(&menu);
    menu_item(
        &menu,
        "Prop_erties",
        Some((key::Return, gdk::ModifierType::MOD1_MASK)),
        on(App::properties),
    );
    menu.show_all();
    let mut a = app.borrow_mut();
    menu.set_attach_widget(Some(&a.tree));
    if let Some(old) = a.context_menu.replace(menu.clone()) {
        // SAFETY: the previous menu is closed and referenced nowhere else.
        unsafe { old.destroy() };
    }
    let tree = a.tree.clone();
    drop(a);
    match event {
        Some(event) => menu.popup_at_pointer(Some(event)),
        None => {
            let rows = tree.selection().selected_rows().0;
            let area = rows
                .first()
                .map(|path| tree.cell_area(Some(path), None::<&gtk::TreeViewColumn>));
            match (area, tree.bin_window()) {
                (Some(area), Some(bin)) => menu.popup_at_rect(
                    &bin,
                    &area,
                    gdk::Gravity::SouthWest,
                    gdk::Gravity::NorthWest,
                    None,
                ),
                _ => {
                    menu.popup_at_widget(&tree, gdk::Gravity::Center, gdk::Gravity::NorthWest, None)
                }
            }
        }
    }
}
fn mime_of(path: &std::path::Path) -> String {
    file_ops::guess_type(path).0.to_string()
}
impl App {
    fn publish(&mut self) {
        let mut state = self.window_state.lock().unwrap();
        state.toolkit = "gtk3".into();
        state.visible = self.window.is_visible();
        state.tray_available = self.tray_available;
        state.hotkey_status = self.hotkey_status.clone();
        state.query = self.entry.text().into();
        state.filter = self.filter.active_id().unwrap_or_default().into();
        state.matches = self.data.borrow().as_ref().map(|r| r.total).unwrap_or(0);
        state.settings_open = self.options.as_ref().is_some_and(|o| o.dialog.is_visible());
        state.status_text = self.status.text().into();
        state.selected_count = if self.tree.model().is_some() {
            self.tree.selection().selected_rows().0.len()
        } else {
            0
        };
    }
    fn focus_search(&mut self) {
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }
    fn show(&mut self) {
        let mapped = self.window.is_mapped();
        self.activate_on_map.set(!mapped);
        if !mapped {
            // A hidden window is placed by the window manager like a new one
            // (top left on Muffin) unless we ask for its last position.
            if self.memory.maximized {
                self.window.maximize();
            } else if let (Some(x), Some(y)) = (self.memory.x, self.memory.y)
                && on_screen(x, y, self.memory.width)
            {
                if self.memory.width > 0 && self.memory.height > 0 {
                    self.window.resize(self.memory.width, self.memory.height);
                }
                self.window.move_(x, y);
            }
        }
        self.window.show_all();
        if self
            .data
            .borrow()
            .as_ref()
            .is_none_or(|r| r.total <= 10_000)
        {
            self.pager.hide();
        }
        self.window.deiconify();
        self.window.present();
        if mapped {
            activate(&self.window);
        }
        if let Some(options) = &self.options {
            options.dialog.present();
        } else {
            self.focus_search();
        }
        self.publish();
    }
    /// Whether the window is in front of the user's other windows. See
    /// `desktop::x11_focus_is_ours` for why GTK's own flag is not enough on X11.
    fn foreground(&self) -> bool {
        let x11 = gdk::Display::default().is_some_and(|d| d.type_().name() == "GdkX11Display");
        if x11 && let Some(ours) = desktop::x11_focus_is_ours() {
            return ours;
        }
        self.window.is_active()
    }
    fn hide(&mut self) {
        if self.tray_available {
            if let Some(options) = &self.options {
                options.dialog.hide();
            }
            // Where the window is now is where it comes back (`show`).
            if self.window.is_visible() && !self.memory.maximized {
                let (x, y) = self.window.position();
                let (width, height) = self.window.size();
                self.memory.x = Some(x);
                self.memory.y = Some(y);
                self.memory.width = width;
                self.memory.height = height;
            }
            self.save_memory();
            self.window.hide();
        } else {
            self.status
                .set_text("No tray is available; the window stays open.");
        }
        self.publish();
    }
    fn show_sort(&self) {
        let title = match self.sort {
            Sort::Name => "Name",
            Sort::Path => "Path",
            Sort::Size => "Size",
            Sort::Modified => "Modified",
            Sort::Type => "Type",
        };
        for c in self.tree.columns() {
            c.set_sort_indicator(c.title().as_deref() == Some(title));
            c.set_sort_order(if self.descending {
                gtk::SortType::Descending
            } else {
                gtk::SortType::Ascending
            });
        }
    }
    /// Saves the window state one second after the last change.
    fn remember(&mut self) {
        if let Some(timer) = self.memory_timer.take() {
            timer.remove();
        }
        let messages = self.messages.clone();
        self.memory_timer = Some(glib::timeout_add_local_once(
            Duration::from_secs(1),
            move || {
                let _ = messages.try_send(Message::SaveWindow);
            },
        ));
    }
    fn save_memory(&mut self) {
        if let Some(timer) = self.memory_timer.take() {
            timer.remove();
        }
        self.memory.columns = self.tree.columns().iter().map(|c| c.width()).collect();
        self.memory.sort = Some(self.sort);
        self.memory.descending = self.descending;
        self.memory.filter = self.filter.active_id().unwrap_or_default().into();
        if WindowMemory::load(&self.memory_path) != self.memory
            && let Ok(json) = serde_json::to_vec_pretty(&self.memory)
        {
            let _ = config::atomic_write(&self.memory_path, &json);
        }
    }
    fn quit(&mut self) {
        self.save_memory();
        self.quitting = true;
        self.window.hide();
        gtk::main_quit();
    }
    fn busy(&mut self, text: &str) {
        self.status.set_text(text);
        self.spinner.start();
        self.spinner.show();
        self.publish();
    }
    fn rescan(&mut self) {
        self.busy("Indexing requested…");
        let service = self.backend.clone();
        let tx = self.messages.clone();
        std::thread::spawn(move || {
            if let Err(e) = service.rescan() {
                let _ = tx.send_blocking(Message::Error(e.to_string()));
            }
        });
    }
    fn pause(&mut self) {
        let paused = self.backend.status().paused;
        let service = self.backend.clone();
        let tx = self.messages.clone();
        std::thread::spawn(move || {
            if let Err(e) = service.pause(!paused) {
                let _ = tx.send_blocking(Message::Error(e.to_string()));
            }
        });
    }
    fn schedule(&mut self) {
        if let Some((source, fired)) = self.debounce.take()
            && !fired.get()
        {
            source.remove();
        }
        let id = self.query_version.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending = true;
        if !self.backend.status().scanning {
            self.status.set_text("Searching…");
        }
        // The type filter is one more term; OR binds tighter than the
        // implicit AND, so `a | b` plus `pic:` means (a or b) and a picture.
        let mut query = self.entry.text().to_string();
        if let Some(filter) = self.filter.active_id().filter(|f| !f.is_empty()) {
            if !query.trim().is_empty() {
                query.push(' ');
            }
            query.push_str(&filter);
        }
        self.requested_key = (query.clone(), self.sort, self.descending, self.offset);
        let request = SearchRequest {
            query,
            sort: self.sort,
            descending: self.descending,
            limit: 10_000,
            offset: self.offset,
        };
        let jobs = self.jobs.clone();
        let current = self.query_version.clone();
        // A one-shot debounce does not repaint or animate the window.
        let fired = Rc::new(Cell::new(false));
        let fired_callback = fired.clone();
        self.debounce = Some((
            glib::timeout_add_local_once(
                Duration::from_millis(self.backend.settings().ui.search_delay_ms),
                move || {
                    fired_callback.set(true);
                    if current.load(Ordering::Relaxed) == id {
                        let _ = jobs.send(Job { id, request });
                    }
                },
            ),
            fired,
        ));
        self.publish();
    }
    fn backend_changed(&mut self) {
        let status = self.backend.status();
        let settings = self.backend.settings();
        let mut desktop = settings.ui.clone();
        if self.shortcut_suspended {
            desktop.global_shortcut.clear();
        }
        self.integration.configure(&desktop);
        self.integration.set_paused(status.paused);
        if status.generation != self.last_generation {
            self.last_generation = status.generation;
            self.schedule();
        }
        self.update_status();
    }
    fn update_status(&mut self) {
        let s = self.backend.status();
        if self.applying {
            self.status
                .set_text("Saving options; indexing will start automatically…");
            self.spinner.start();
        } else if s.scanning {
            self.spinner.start();
            let text = match s.phase.as_str() {
                "watching" => "Preparing folder monitoring…".into(),
                "saving" => format!("Saving index… {} entries", count(s.visited)),
                _ if s.visited > 0 => format!("Indexing… {} entries scanned", count(s.visited)),
                _ => s.message.clone(),
            };
            self.status.set_text(&text);
        } else if s.phase == "error" {
            self.spinner.stop();
            self.status.set_text(&s.message);
        } else if s.paused {
            self.spinner.stop();
            self.status.set_text("Indexing paused");
        } else if self.pending {
            self.spinner.start();
            self.status.set_text("Searching…");
        } else {
            self.spinner.stop();
            self.status.set_text(if s.entries == 0 {
                "Use File → Add Folder to start indexing"
            } else {
                "Ready"
            });
        }
        if s.error_count > 0 || !s.watch_errors.is_empty() {
            self.status.set_tooltip_text(Some(&format!(
                "{} indexing errors\n{}",
                s.error_count,
                s.errors
                    .iter()
                    .chain(s.watch_errors.iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            )));
        } else {
            self.status.set_tooltip_text(None);
        }
        let counts = if let Some(r) = self.data.borrow().as_ref() {
            format!(
                "{} indexed  |  {} {}  |  {:.1} ms",
                count(s.entries),
                count(r.total),
                if r.total == 1 { "match" } else { "matches" },
                r.elapsed_ms
            )
        } else {
            format!("{} indexed", count(s.entries))
        };
        self.counts.set_text(&counts);
        self.publish();
    }
    fn replace_results(&mut self, response: SearchResponse) {
        let length = response.entries.len();
        let total = response.total;
        // Same search refreshed because files changed on disk: keep what the
        // user selected and where they scrolled, so live updates are calm.
        let refresh = self.shown_key.as_ref() == Some(&self.requested_key);
        let kept: std::collections::HashSet<Box<[u8]>> = if refresh {
            self.selected().into_iter().map(|e| e.path).collect()
        } else {
            Default::default()
        };
        let scroll = self.scroller.vadjustment().value();
        self.shown_key = Some(self.requested_key.clone());
        // The GTK model stores only row IDs. Text and icons are requested for
        // visible rows, avoiding thousands of duplicated path strings.
        let model = gtk::ListStore::new(&[u32::static_type()]);
        for i in 0..length {
            model.insert_with_values(None, &[(0, &(i as u32))]);
        }
        let reselect: Vec<i32> = if kept.is_empty() {
            Vec::new()
        } else {
            response
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| kept.contains(&e.path))
                .map(|(i, _)| i as i32)
                .collect()
        };
        *self.data.borrow_mut() = Some(response);
        self.tree.set_model(Some(&model));
        let selection = self.tree.selection();
        for row in reselect {
            selection.select_path(&gtk::TreePath::from_indicesv(&[row]));
        }
        let adjustment = self.scroller.vadjustment();
        if refresh {
            // The new model is measured on the next layout pass.
            glib::idle_add_local_once(move || {
                adjustment.set_value(scroll.min(adjustment.upper() - adjustment.page_size()));
            });
        } else {
            adjustment.set_value(0.);
        }
        self.pager.set_visible(total > 10_000);
        self.previous.set_sensitive(self.offset > 0);
        self.next.set_sensitive(self.offset + length < total);
        self.page_label.set_text(&format!(
            "{}–{} of {}",
            if length == 0 { 0 } else { self.offset + 1 },
            self.offset + length,
            count(total)
        ));
        self.pending = false;
        self.update_status();
    }
    fn apply_theme(&self, settings: &Settings) {
        if let Some(gtk) = gtk::Settings::default() {
            match settings.ui.theme.as_str() {
                "dark" => {
                    gtk.set_property("gtk-theme-name", "Adwaita");
                    gtk.set_property("gtk-application-prefer-dark-theme", true);
                }
                "light" => {
                    gtk.set_property("gtk-theme-name", "Adwaita");
                    gtk.set_property("gtk-application-prefer-dark-theme", false);
                }
                _ => {
                    gtk.set_property("gtk-theme-name", &self.system_theme);
                    gtk.set_property("gtk-application-prefer-dark-theme", self.system_dark);
                }
            }
            gtk.set_property("gtk-enable-animations", false);
        }
    }
    fn apply_settings(&mut self, settings: Settings, close: bool) {
        if self.applying {
            return;
        }
        self.applying = true;
        if let Some(options) = &self.options {
            options.sensitive(false);
        }
        self.busy("Saving options; preparing indexing…");
        let backend = self.backend.clone();
        let tx = self.messages.clone();
        std::thread::spawn(move || {
            let result = backend.apply_settings(settings).map_err(|e| e.to_string());
            let _ = tx.send_blocking(Message::Settings(Box::new(result), close));
        });
    }
    fn message(app: &AppRef, message: Message) {
        match message {
            Message::Desktop => {
                let events: Vec<_> = app.borrow().desktop_events.try_iter().collect();
                for event in events {
                    match event {
                        Event::Action(WindowAction::Settings) => {
                            app.borrow_mut().show();
                            Self::options(app);
                        }
                        Event::Action(action) => {
                            let mut a = app.borrow_mut();
                            match action {
                                WindowAction::Show { query } => {
                                    a.show();
                                    if let Some(query) = query {
                                        a.entry.set_text(&query);
                                        a.offset = 0;
                                        a.schedule();
                                        a.focus_search();
                                    }
                                }
                                WindowAction::Toggle => {
                                    if a.window.is_visible() && a.foreground() {
                                        a.hide()
                                    } else {
                                        a.show()
                                    }
                                }
                                WindowAction::Hide => a.hide(),
                                WindowAction::Rescan => a.rescan(),
                                WindowAction::Pause => a.pause(),
                                WindowAction::Quit => a.quit(),
                                _ => {}
                            }
                        }
                        Event::TrayAvailable(available) => {
                            let mut a = app.borrow_mut();
                            a.tray_available = available;
                            if available && a.hide_on_ready {
                                a.hide_on_ready = false;
                                a.hide();
                            } else if !available && !a.window.is_visible() {
                                a.show();
                            }
                            a.publish();
                        }
                        Event::IntegrationError(error) => {
                            app.borrow().status.set_tooltip_text(Some(&error));
                        }
                        Event::PortalActivation(token) => {
                            let mut a = app.borrow_mut();
                            if let Some(token) = token {
                                a.window.set_startup_id(&token);
                            }
                            if a.window.is_visible() && a.foreground() {
                                a.hide();
                            } else {
                                a.show();
                            }
                        }
                        Event::HotkeyStatus(status) => {
                            let mut a = app.borrow_mut();
                            a.hotkey_status = status;
                            a.publish();
                        }
                    }
                }
            }
            Message::Backend => app.borrow_mut().backend_changed(),
            Message::Search(id, result) => {
                let mut a = app.borrow_mut();
                if id != a.query_version.load(Ordering::Relaxed) {
                    return;
                }
                a.debounce = None;
                match result {
                    Ok(response) => a.replace_results(response),
                    Err(error) => {
                        a.pending = false;
                        a.spinner.stop();
                        a.status.set_text(&error);
                        a.publish();
                    }
                }
            }
            Message::Settings(result, close) => {
                let result = *result;
                let mut a = app.borrow_mut();
                a.applying = false;
                if let Some(options) = &a.options {
                    options.sensitive(true);
                }
                match result {
                    Ok(settings) => {
                        a.apply_theme(&settings);
                        if let Some(options) = &a.options {
                            options.saved(&settings);
                        }
                        if close && let Some(options) = a.options.take() {
                            options.dialog.close();
                        }
                        a.backend_changed();
                    }
                    Err(error) => {
                        if let Some(options) = &a.options {
                            options.error(&error);
                        }
                        a.status.set_text(&error);
                    }
                }
                a.publish();
            }
            Message::Error(error) => {
                let mut a = app.borrow_mut();
                a.spinner.stop();
                a.status.set_text(&error);
                a.publish();
            }
            Message::SaveWindow => {
                let mut a = app.borrow_mut();
                // The one-shot timer has fired and is gone; just forget it.
                a.memory_timer = None;
                a.save_memory();
            }
            Message::SuspendShortcut(value) => {
                let mut a = app.borrow_mut();
                a.shortcut_suspended = value;
                let mut config = a.backend.settings().ui;
                if value {
                    config.global_shortcut.clear();
                }
                a.integration.configure(&config);
            }
        }
    }
    fn options(app: &AppRef) {
        if let Some(options) = &app.borrow().options {
            options.dialog.present();
            return;
        }
        let (parent, settings, messages) = {
            let a = app.borrow();
            (a.window.clone(), a.backend.settings(), a.messages.clone())
        };
        let weak = Rc::downgrade(app);
        let capture = messages.clone();
        let capture_app = weak.clone();
        let options = OptionsWindow::new(&parent, settings, move |recording| {
            if let Some(app) = capture_app.upgrade()
                && let Ok(mut app) = app.try_borrow_mut()
            {
                app.shortcut_suspended = recording;
                let mut config = app.backend.settings().ui;
                if recording {
                    config.global_shortcut.clear();
                }
                app.integration.configure(&config);
            } else {
                let _ = capture.try_send(Message::SuspendShortcut(recording));
            }
        });
        let draft = options.draft.clone();
        let error = options.error_label.clone();
        let autostart = options.autostart.clone();
        options.dialog.connect_response(move |dialog, response| {
            if matches!(response, gtk::ResponseType::Ok | gtk::ResponseType::Apply) {
                if autostart.get() != file_ops::autostart_enabled() {
                    let mut data_dir = None;
                    with_app(&weak, |a| data_dir = Some(a.data_dir.clone()));
                    let custom = data_dir.filter(|dir| {
                        let default = crate::store::default_dir();
                        default.canonicalize().unwrap_or(default) != *dir
                    });
                    if let Err(e) = file_ops::set_autostart(autostart.get(), custom.as_deref()) {
                        error.set_text(&format!("Start at login: {e}"));
                        return;
                    }
                }
                let next = draft.borrow().clone();
                match next.validate() {
                    Ok(()) => with_app(&weak, |a| {
                        a.apply_settings(next, response == gtk::ResponseType::Ok)
                    }),
                    Err(e) => error.set_text(&e.to_string()),
                }
            } else {
                let _ = messages.try_send(Message::SuspendShortcut(false));
                dialog.hide();
                with_app(&weak, |a| {
                    a.options = None;
                    a.focus_search();
                    a.publish();
                });
                dialog.close();
            }
        });
        options.dialog.show_all();
        app.borrow_mut().options = Some(options);
        app.borrow_mut().publish();
    }
    fn choose_folder(app: &AppRef) {
        let parent = app.borrow().window.clone();
        let chooser = gtk::FileChooserDialog::with_buttons(
            Some("Add Folder to Index"),
            Some(&parent),
            gtk::FileChooserAction::SelectFolder,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Add Folder", gtk::ResponseType::Accept),
            ],
        );
        chooser.set_modal(true);
        chooser.set_select_multiple(true);
        let weak = Rc::downgrade(app);
        chooser.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let paths = dialog.filenames();
                with_app(&weak, |a| {
                    let mut settings = a.backend.settings();
                    for path in paths {
                        if !settings.index.roots.iter().any(|r| r.path.path() == path) {
                            settings.index.roots.push(config::RootConfig::new(&path));
                        }
                    }
                    match settings.validate() {
                        Ok(()) => a.apply_settings(settings, false),
                        Err(e) => a.info("Could not add folder", &e.to_string()),
                    }
                });
            }
            dialog.close();
        });
        chooser.show_all();
    }
    fn selected(&self) -> Vec<Entry> {
        if self.tree.model().is_none() {
            return Vec::new();
        }
        let (paths, model) = self.tree.selection().selected_rows();
        let data = self.data.borrow();
        let Some(data) = data.as_ref() else {
            return Vec::new();
        };
        paths
            .iter()
            .filter_map(|path| {
                model
                    .iter(path)
                    .and_then(|iter| model.value(&iter, 0).get::<u32>().ok())
                    .and_then(|i| data.entries.get(i as usize).cloned())
            })
            .collect()
    }
    fn open_selected(&mut self, parent: bool) {
        let paths = self.selected_paths();
        if paths.len() > 20 {
            self.info(
                "Too many files selected",
                "Select 20 or fewer files to open together.",
            );
            return;
        }
        open_paths(paths, parent, self.messages.clone());
    }
    fn copy_selected(&mut self, names: bool) {
        let text = self
            .selected()
            .iter()
            .map(|e| {
                if names {
                    e.display_name()
                } else {
                    e.display_path()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD).set_text(&text);
    }
    fn selected_items(&self) -> Vec<Item> {
        self.selected()
            .into_iter()
            .map(|e| {
                let path = e.path_buf();
                let folder = e.kind == Kind::Folder;
                let mime = if folder {
                    "inode/directory".to_owned()
                } else {
                    mime_of(&path)
                };
                Item { path, folder, mime }
            })
            .collect()
    }
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected().iter().map(Entry::path_buf).collect()
    }
    fn error(&self, text: &str) {
        self.status.set_text(text);
    }
    fn launch_context(&self) -> Option<gdk::AppLaunchContext> {
        let context = self.window.display().app_launch_context()?;
        context.set_timestamp(gtk::current_event_time());
        Some(context)
    }
    fn open_with(&mut self, info: &gio::AppInfo) {
        let paths = self.selected_paths();
        if let Err(e) = file_ops::launch(info, &paths, self.launch_context().as_ref()) {
            self.error(&format!("Cannot start {}: {e}", info.display_name()));
        }
    }
    fn choose_application(&mut self) {
        let Some(path) = self.selected_paths().into_iter().next() else {
            return;
        };
        let dialog = gtk::AppChooserDialog::new(
            Some(&self.window),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            &gio::File::for_path(&path),
        );
        let context = self.launch_context();
        let status = self.status.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Ok
                && let Some(info) = dialog.app_info()
                && let Err(e) =
                    file_ops::launch(&info, std::slice::from_ref(&path), context.as_ref())
            {
                status.set_text(&format!("Cannot start {}: {e}", info.display_name()));
            }
            dialog.close();
        });
        dialog.show_all();
    }
    /// Shows the selection in the file manager, selected in its folder;
    /// without a FileManager1 service, opens the containing folder.
    fn show_in_folder(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let tx = self.messages.clone();
        let fallback = paths.clone();
        file_ops::file_manager("ShowItems", &paths, move |answered| {
            if !answered {
                open_paths(fallback, true, tx);
            }
        });
    }
    fn terminal_here(&mut self) {
        let Some(item) = self.selected_items().into_iter().next() else {
            return;
        };
        let dir = if item.folder {
            item.path
        } else {
            item.path.parent().map(PathBuf::from).unwrap_or(item.path)
        };
        if let Err(e) = file_ops::open_terminal(&dir) {
            self.error(&e.to_string());
        }
    }
    fn clipboard(&mut self, cut: bool) {
        let paths = self.selected_paths();
        if !paths.is_empty() {
            file_ops::set_clipboard_files(&paths, cut);
        }
    }
    fn rename_selected(&mut self) {
        let paths = self.selected_paths();
        let [path] = paths.as_slice() else {
            return;
        };
        let path = path.clone();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dialog = gtk::Dialog::with_buttons(
            Some("Rename"),
            Some(&self.window),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Rename", gtk::ResponseType::Ok),
            ],
        );
        dialog.set_default_response(gtk::ResponseType::Ok);
        dialog.set_default_width(420);
        let entry = gtk::Entry::new();
        entry.set_text(&name);
        entry.set_activates_default(true);
        let error = gtk::Label::new(None);
        error.set_xalign(0.);
        error.set_line_wrap(true);
        let content = dialog.content_area();
        content.set_spacing(6);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_top(12);
        content.pack_start(&entry, false, false, 0);
        content.pack_start(&error, false, false, 0);
        dialog.show_all();
        // Select the name without its extension, as file managers do.
        let stem = match name.rfind('.') {
            Some(dot) if dot > 0 => name[..dot].chars().count() as i32,
            _ => -1,
        };
        entry.select_region(0, stem);
        dialog.connect_response(move |dialog, response| {
            if response != gtk::ResponseType::Ok {
                dialog.close();
                return;
            }
            match file_ops::rename(&path, &entry.text()) {
                Ok(_) => dialog.close(),
                Err(e) => error.set_text(&e.to_string()),
            }
        });
    }
    fn trash_selected(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        // One item goes straight to the Trash (it can be restored); a larger
        // selection is confirmed, since search results span many folders.
        if paths.len() > 1 {
            let dialog = gtk::MessageDialog::new(
                Some(&self.window),
                gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                gtk::MessageType::Question,
                gtk::ButtonsType::None,
                &format!("Move {} items to the Trash?", paths.len()),
            );
            dialog.add_buttons(&[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Move to Trash", gtk::ResponseType::Ok),
            ]);
            dialog.set_default_response(gtk::ResponseType::Ok);
            let tx = self.messages.clone();
            dialog.connect_response(move |dialog, response| {
                dialog.close();
                if response == gtk::ResponseType::Ok {
                    trash_paths(paths.clone(), tx.clone());
                }
            });
            dialog.show_all();
        } else {
            trash_paths(paths, self.messages.clone());
        }
    }
    fn run_action(&mut self, action: &Action, window: Option<u32>) {
        let items = self.selected_items();
        let result = action.commands(&items, window).and_then(|commands| {
            for argv in commands {
                let argv = if action.terminal {
                    file_ops::in_terminal(argv)?
                } else {
                    argv
                };
                file_ops::spawn(&argv, items[0].path.parent(), &[])?;
            }
            Ok(())
        });
        if let Err(e) = result {
            self.error(&e.to_string());
        }
    }
    fn run_script(&mut self, script: &Script) {
        let items = self.selected_items();
        let mut argv = vec![script.path.clone().into_os_string()];
        argv.extend(items.iter().map(|i| i.path.clone().into_os_string()));
        if let Err(e) = file_ops::spawn(&argv, items[0].path.parent(), &script.environment(&items))
        {
            self.error(&e.to_string());
        }
    }
    /// The file manager's properties window; File Minnow's summary when no
    /// file manager provides one.
    fn properties(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let summary = self.selected().first().map(|e| {
            format!(
                "{}\n\nLocation: {}\nType: {}\nSize: {}\nModified: {}",
                e.display_name(),
                e.display_path(),
                e.kind.as_str(),
                size(e.size),
                modified(e)
            )
        });
        let window = self.window.clone();
        file_ops::file_manager("ShowItemProperties", &paths, move |answered| {
            if !answered && let Some(summary) = summary {
                info(&window, "File Properties", &summary);
            }
        });
    }
    fn info(&self, title: &str, text: &str) {
        info(&self.window, title, text);
    }
}
/// Opens entries (or their folders) with the desktop's default applications.
fn open_paths(paths: Vec<PathBuf>, parent: bool, tx: async_channel::Sender<Message>) {
    std::thread::spawn(move || {
        for mut path in paths {
            if parent && let Some(p) = path.parent() {
                path = p.to_path_buf();
            }
            let result = std::fs::symlink_metadata(&path)
                .map_err(anyhow::Error::from)
                .and_then(|_| {
                    file_ops::spawn(
                        &["xdg-open".into(), path.clone().into_os_string()],
                        None,
                        &[],
                    )
                });
            if let Err(error) = result {
                let _ = tx.send_blocking(Message::Error(format!(
                    "Cannot open {}: {error}",
                    path.display()
                )));
            }
        }
    });
}
/// Moves entries to the Trash off the main thread; the index follows
/// through the folder watcher.
fn trash_paths(paths: Vec<PathBuf>, tx: async_channel::Sender<Message>) {
    std::thread::spawn(move || {
        let failures = file_ops::trash(&paths);
        let text = match failures.as_slice() {
            [] if paths.len() == 1 => "Moved 1 item to the Trash".to_owned(),
            [] => format!("Moved {} items to the Trash", paths.len()),
            [only] => format!("Cannot move to the Trash: {only}"),
            [first, rest @ ..] => format!(
                "Cannot move {} items to the Trash: {first} (and {} more)",
                failures.len(),
                rest.len()
            ),
        };
        let _ = tx.send_blocking(Message::Error(text));
    });
}
fn info(window: &gtk::Window, title: &str, text: &str) {
    {
        let dialog = gtk::MessageDialog::new(
            Some(window),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Info,
            gtk::ButtonsType::Close,
            text,
        );
        dialog.set_title(title);
        dialog.connect_response(|d, _| d.close());
        dialog.show_all();
    }
}
