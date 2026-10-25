//! Compact GTK desktop frontend. GTK owns drawing, selection, focus and modality.
use crate::{
    actions::{self, Action, Item, Script},
    client::{Backend, Session, Subscription},
    config::{self, Settings},
    desktop::{self, Emitter, Event, Instance, Integration, WindowAction, WindowState},
    file_ops,
    history::History,
    index::{Entry, Kind},
    query::{SearchRequest, SearchResponse, Sort},
    rename::{self, Undo},
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Columns switched off in the View menu (Name cannot be hidden).
    #[serde(default = "default_hidden")]
    hidden_columns: Vec<String>,
    /// Preview pane shown, and its width.
    preview: bool,
    preview_width: i32,
    /// Saved after the Items column existed; older states get it hidden.
    #[serde(default)]
    knows_items: bool,
    /// Results shown as thumbnails (View > Thumbnails).
    #[serde(default)]
    thumbnails: bool,
}
fn default_hidden() -> Vec<String> {
    vec!["Created".into(), "Items".into(), "Run count".into()]
}
impl Default for WindowMemory {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            x: None,
            y: None,
            maximized: false,
            columns: Vec::new(),
            sort: None,
            descending: false,
            filter: String::new(),
            hidden_columns: default_hidden(),
            preview: false,
            preview_width: 320,
            knows_items: true,
            thumbnails: false,
        }
    }
}
impl WindowMemory {
    fn path(dir: &std::path::Path) -> PathBuf {
        config::ConfigStore::for_data_dir(dir)
            .path
            .with_file_name("window.json")
    }
    fn load(path: &std::path::Path) -> Self {
        let mut memory: Self = std::fs::read(path)
            .ok()
            .filter(|bytes| bytes.len() < 64 * 1024)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if !memory.knows_items {
            memory.hidden_columns.push("Items".into());
            memory.knows_items = true;
        }
        memory
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
    /// Run this search (a bookmark chosen from the menu).
    RunSearch(String),
    /// A preview loaded in the background, for request `id`.
    Preview(u64, PreviewContent),
    /// The selection settled: load the preview.
    LoadPreview,
    /// Renames finished (File Minnow's own, which Edit > Undo can revert).
    Renamed(rename::Outcome),
    /// These paths went to the Trash; the text summarizes the result.
    Trashed(Vec<PathBuf>, String),
    /// An undo finished: its summary or error, and the step to keep if it failed.
    Undone(Result<String, String>),
    /// A thumbnail for the grid: its generation, result index and pixels.
    Thumbnail(u64, u32, PreviewContent),
    /// Run counts were just recorded off-thread; refresh the visible results.
    RunsRecorded,
}
/// A thumbnail the grid asks for.
struct ThumbJob {
    generation: u64,
    index: u32,
    path: PathBuf,
    mime: String,
}
/// Requests for thumbnails, newest last; one background thread serves them.
type ThumbQueue = Arc<(Mutex<Vec<ThumbJob>>, std::sync::Condvar)>;
/// Pixel size of grid thumbnails.
const GRID_SIZE: i32 = 128;
pub(crate) enum PreviewContent {
    /// Decoded, already scaled pixels (RGB or RGBA, 8 bits per sample).
    Image {
        bytes: glib::Bytes,
        width: i32,
        height: i32,
        stride: i32,
        alpha: bool,
    },
    Text(String),
    Info(String),
}
/// The preview pane: a header, then an image, a text view or a summary.
pub(crate) struct PreviewPane {
    frame: gtk::Box,
    header: gtk::Label,
    image: gtk::Image,
    text: gtk::TextView,
    text_scroll: gtk::ScrolledWindow,
    info: gtk::Label,
}
impl PreviewPane {
    fn new() -> Self {
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 6);
        frame.set_margin_start(6);
        frame.set_margin_end(6);
        frame.set_margin_top(6);
        frame.set_margin_bottom(6);
        frame.set_no_show_all(true);
        let header = gtk::Label::new(None);
        header.set_xalign(0.);
        header.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        header.set_selectable(true);
        header.show();
        let image = gtk::Image::new();
        let text = gtk::TextView::new();
        text.set_editable(false);
        text.set_cursor_visible(false);
        text.set_monospace(true);
        text.set_wrap_mode(gtk::WrapMode::None);
        text.set_left_margin(4);
        text.show();
        let text_scroll =
            gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        text_scroll.set_shadow_type(gtk::ShadowType::In);
        text_scroll.add(&text);
        let info = gtk::Label::new(None);
        info.set_xalign(0.);
        info.set_yalign(0.);
        info.set_line_wrap(true);
        info.set_selectable(true);
        frame.pack_start(&header, false, false, 0);
        frame.pack_start(&image, true, true, 0);
        frame.pack_start(&text_scroll, true, true, 0);
        frame.pack_start(&info, true, true, 0);
        Self {
            frame,
            header,
            image,
            text,
            text_scroll,
            info,
        }
    }
    fn show_content(&self, content: PreviewContent) {
        self.image.hide();
        self.text_scroll.hide();
        self.info.hide();
        match content {
            PreviewContent::Image {
                bytes,
                width,
                height,
                stride,
                alpha,
            } => {
                let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
                    &bytes,
                    gtk::gdk_pixbuf::Colorspace::Rgb,
                    alpha,
                    8,
                    width,
                    height,
                    stride,
                );
                self.image.set_from_pixbuf(Some(&pixbuf));
                self.image.show();
            }
            PreviewContent::Text(text) => {
                if let Some(buffer) = self.text.buffer() {
                    buffer.set_text(&text);
                }
                self.text_scroll.show();
            }
            PreviewContent::Info(text) => {
                self.info.set_text(&text);
                self.info.show();
            }
        }
    }
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
    /// The list is empty because nothing is searched yet
    /// (`hide_results_until_search`).
    waiting_for_search: bool,
    /// The `hide_results_until_search` value the shown results follow.
    hide_until_search: bool,
    /// Folded search terms of the shown results, for highlighting.
    highlight: Rc<RefCell<Vec<Vec<u8>>>>,
    /// Roots whose drive or share is unavailable; their rows are greyed.
    offline: Rc<RefCell<Vec<Vec<u8>>>>,
    /// Result columns in order (Name, Path, Size, Modified, Type, Created, Items).
    columns: Vec<gtk::TreeViewColumn>,
    history: History,
    /// Model behind the search box's completion: the history, newest first.
    completion: gtk::ListStore,
    bookmarks_menu: gtk::Menu,
    /// The bookmarks and custom filters the menus/drop-down were built from.
    shown_bookmarks: Vec<config::Bookmark>,
    shown_filters: Vec<config::Filter>,
    /// Set while the filter drop-down is rebuilt, so its change signal is ignored.
    filter_rebuilding: Rc<Cell<bool>>,
    paned: gtk::Paned,
    preview: PreviewPane,
    /// Debounce for loading the preview after the selection changes.
    preview_timer: Option<glib::SourceId>,
    /// Number of the latest preview request; older results are dropped.
    preview_id: u64,
    /// The request whose results are displayed, and the one in flight; when
    /// they match, a refresh keeps the selection and scroll position.
    shown_key: Option<(String, Sort, bool, usize)>,
    requested_key: (String, Sort, bool, usize),
    /// Renames and moves to the Trash that Edit > Undo takes back, last last.
    undo: Vec<Undo>,
    undo_item: gtk::MenuItem,
    /// Searches before each folder browsed into, for Alt+Left.
    browse_back: Vec<String>,
    /// Tools > Recent Changes, while it is open.
    changes: Option<Rc<crate::changes_ui::ChangesWindow>>,
    /// The thumbnail grid (View > Thumbnails), its model (picture, name,
    /// result index) and the stack holding it and the list.
    grid: gtk::IconView,
    grid_scroller: gtk::ScrolledWindow,
    results: gtk::Stack,
    /// Set while the grid's selection is copied to the list.
    grid_syncing: Rc<Cell<bool>>,
    /// Increased each time the grid is filled; older thumbnails are dropped.
    grid_generation: Arc<AtomicU64>,
    grid_requested: Vec<bool>,
    thumb_queue: ThumbQueue,
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
fn created(e: &Entry) -> String {
    if e.created == crate::table::NO_DATE {
        return String::new();
    }
    chrono::DateTime::from_timestamp(e.created, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
/// Pango markup for `text` with every occurrence of the (folded) search
/// terms in bold. Positions are found in the folded text and mapped back.
fn highlight_markup(text: &str, terms: &[Vec<u8>]) -> String {
    let escaped = |s: &str| glib::markup_escape_text(s).to_string();
    if terms.is_empty() {
        return escaped(text);
    }
    let (folded, offsets) = crate::fold::fold_with_offsets(text);
    let mut marks = vec![false; text.len()];
    for term in terms {
        if term.is_empty() {
            continue;
        }
        for hit in memchr::memmem::find_iter(&folded, term) {
            let (Some(&start), Some(&last)) = (offsets.get(hit), offsets.get(hit + term.len() - 1))
            else {
                continue;
            };
            let end = text[last..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(i, _)| last + i);
            for m in &mut marks[start..end] {
                *m = true;
            }
        }
    }
    if !marks.iter().any(|m| *m) {
        return escaped(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut from = 0;
    let mut bold = false;
    for (i, _) in text.char_indices() {
        if marks[i] != bold {
            out.push_str(&escaped(&text[from..i]));
            out.push_str(if marks[i] { "<b>" } else { "</b>" });
            bold = marks[i];
            from = i;
        }
    }
    out.push_str(&escaped(&text[from..]));
    if bold {
        out.push_str("</b>");
    }
    out
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
    // Recent searches complete the search box (history, newest first).
    // Rows: text, description, and 0 for a recent search (completes the
    // whole box) or 1 for an operator, filter or bookmark (completes the
    // word being typed: after `:`, or on Ctrl+Space).
    let completion_store = gtk::ListStore::new(&[
        String::static_type(),
        String::static_type(),
        i32::static_type(),
    ]);
    let completion = gtk::EntryCompletion::new();
    completion.set_model(Some(&completion_store));
    completion.set_text_column(0);
    completion.set_inline_completion(false);
    completion.set_popup_completion(true);
    completion.set_minimum_key_length(1);
    let described = gtk::CellRendererText::new();
    described.set_property("foreground", "gray");
    completion.pack_start(&described, false);
    completion.add_attribute(&described, "text", 1);
    let asked = Rc::new(Cell::new(false));
    {
        let asked = asked.clone();
        completion.set_match_func(move |completion, key, iter| {
            let Some(model) = completion.model() else {
                return false;
            };
            let text = model
                .value(iter, 0)
                .get::<String>()
                .unwrap_or_default()
                .to_lowercase();
            // Ctrl+Space asks for operators, filters and bookmarks only.
            if model.value(iter, 2).get::<i32>().unwrap_or(0) == 0 {
                return !asked.get() && !key.is_empty() && text.starts_with(key) && text != key;
            }
            let word = key.rsplit(char::is_whitespace).next().unwrap_or("");
            if let Some(wanted) = word.strip_prefix(':') {
                let about = model
                    .value(iter, 1)
                    .get::<String>()
                    .unwrap_or_default()
                    .to_lowercase();
                return text.contains(wanted) || about.contains(wanted);
            }
            asked.get() && !word.is_empty() && text.starts_with(word) && text != word
        });
    }
    {
        let asked = asked.clone();
        let target = entry.clone();
        completion.connect_match_selected(move |_, model, iter| {
            asked.set(false);
            if model.value(iter, 2).get::<i32>().unwrap_or(0) == 0 {
                return glib::Propagation::Proceed;
            }
            // Replace only the word being typed.
            let insert = model.value(iter, 0).get::<String>().unwrap_or_default();
            let text = target.text().to_string();
            let start = text.rfind(char::is_whitespace).map_or(0, |i| i + 1);
            let joined = format!("{}{insert}", &text[..start]);
            target.set_text(&joined);
            target.set_position(-1);
            glib::Propagation::Stop
        });
    }
    {
        let asked = asked.clone();
        entry.connect_changed(move |e| {
            if e.text().ends_with(char::is_whitespace) {
                asked.set(false);
            }
        });
    }
    {
        let completion = completion.clone();
        entry.connect_key_press_event(move |_, event| {
            if event.keyval() == gdk::keys::constants::space
                && event.state().contains(gdk::ModifierType::CONTROL_MASK)
            {
                asked.set(true);
                completion.complete();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    entry.set_completion(Some(&completion));
    let filter = gtk::ComboBoxText::new();
    fill_filters(&filter, &[]);
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
    let grid = gtk::IconView::new();
    grid.set_selection_mode(gtk::SelectionMode::Multiple);
    grid.set_item_width(GRID_SIZE + 24);
    grid.set_row_spacing(4);
    grid.set_column_spacing(4);
    gtk::prelude::IconViewExt::set_margin(&grid, 6);
    let grid_scroller =
        gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    grid_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    grid_scroller.set_shadow_type(gtk::ShadowType::In);
    grid_scroller.set_kinetic_scrolling(false);
    grid_scroller.set_overlay_scrolling(false);
    grid_scroller.add(&grid);
    let results = gtk::Stack::new();
    results.add_named(&scroller, "list");
    results.add_named(&grid_scroller, "grid");
    // Results on the left, the optional preview pane on the right.
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.pack1(&results, true, false);
    let preview = PreviewPane::new();
    paned.pack2(&preview.frame, false, true);
    layout.pack_start(&paned, true, true, 0);
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
    // Thumbnails for the grid, most recently asked for first.
    let thumb_queue: ThumbQueue = Arc::default();
    let grid_generation = Arc::new(AtomicU64::new(0));
    {
        let (queue, generation, sender) = (
            thumb_queue.clone(),
            grid_generation.clone(),
            messages.clone(),
        );
        std::thread::spawn(move || {
            loop {
                let job = {
                    let (jobs, ready) = &*queue;
                    let mut jobs = jobs.lock().unwrap();
                    loop {
                        if let Some(job) = jobs.pop() {
                            break job;
                        }
                        jobs = ready.wait(jobs).unwrap();
                    }
                };
                if job.generation != generation.load(Ordering::Relaxed) {
                    continue;
                }
                let picture =
                    crate::thumbnails::thumbnail(&job.path, &job.mime, GRID_SIZE, thumbnailers())
                        .and_then(|p| image_content(&p));
                if let Some(content) = picture
                    && sender
                        .send_blocking(Message::Thumbnail(job.generation, job.index, content))
                        .is_err()
                {
                    break;
                }
            }
        });
    }
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
        waiting_for_search: false,
        hide_until_search: settings.ui.hide_results_until_search,
        highlight: Rc::default(),
        offline: Rc::default(),
        columns: Vec::new(),
        history: History::load(History::path_for(
            &config::ConfigStore::for_data_dir(&dir).path,
        )),
        completion: completion_store.clone(),
        bookmarks_menu: gtk::Menu::new(),
        shown_bookmarks: Vec::new(),
        shown_filters: Vec::new(),
        filter_rebuilding: Rc::new(Cell::new(false)),
        paned: paned.clone(),
        preview,
        preview_timer: None,
        preview_id: 0,
        shown_key: None,
        requested_key: (String::new(), Sort::Name, false, 0),
        undo: Vec::new(),
        undo_item: gtk::MenuItem::with_mnemonic("_Undo"),
        browse_back: Vec::new(),
        changes: None,
        grid,
        grid_scroller,
        results,
        grid_syncing: Rc::default(),
        grid_generation,
        grid_requested: Vec::new(),
        thumb_queue,
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
    let rebuilding = app.borrow().filter_rebuilding.clone();
    filter.connect_changed(move |_| {
        if rebuilding.get() {
            return;
        }
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
                if a.memory.thumbnails {
                    a.grid.grab_focus();
                    if a.grid.selected_items().is_empty() {
                        let first = gtk::TreePath::from_indicesv(&[0]);
                        a.grid.select_path(&first);
                        a.grid.set_cursor(&first, None::<&gtk::CellRenderer>, false);
                    }
                    return;
                }
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
        if a.memory.thumbnails {
            a.results.set_visible_child_name("grid");
        }
        a.entry.set_text(launch.query.as_deref().unwrap_or(""));
        a.refresh_completion();
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
    for column in &a.columns {
        let title = column.title().unwrap_or_default();
        column.set_visible(title == "Name" || !memory.hidden_columns.iter().any(|h| *h == title));
    }
    a.memory = memory;
    let preview = a.memory.preview;
    a.set_preview(preview);
    let weak = Rc::downgrade(app);
    a.paned.connect_position_notify(move |paned| {
        with_app(&weak, |a| {
            if a.memory.preview && paned.is_mapped() {
                let width = paned.allocated_width() - paned.position();
                if width > 80 {
                    a.memory.preview_width = width;
                    a.remember();
                }
            }
        });
    });
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
/// Whether `path` lies under one of the unavailable roots.
fn is_offline(offline: &[Vec<u8>], path: &[u8]) -> bool {
    offline.iter().any(|root| crate::table::within(path, root))
}
/// Fills the type filter drop-down: the built-in types, folders, then the
/// user's own filters (whose id is their search).
fn fill_filters(filter: &gtk::ComboBoxText, custom: &[config::Filter]) {
    filter.remove_all();
    filter.append(Some(""), "All");
    for (label, macro_, _) in crate::query::TYPE_FILTERS.iter().take(3) {
        filter.append(Some(macro_), label);
    }
    filter.append(Some("folder:"), "Folder");
    for (label, macro_, _) in crate::query::TYPE_FILTERS.iter().skip(3) {
        filter.append(Some(macro_), label);
    }
    for f in custom {
        filter.append(Some(&f.query), &f.name);
    }
}
/// Search operators offered while typing (after `:`, or on Ctrl+Space).
const OPERATORS: &[(&str, &str)] = &[
    ("ext:", "file extensions: ext:pdf;docx"),
    ("size:", "size: size:>10mb, size:1kb..2mb"),
    ("dm:", "date modified: dm:today, dm:lastweek, dm:2026"),
    ("dc:", "date created"),
    ("path:", "match the whole path"),
    ("parent:", "directly inside a folder"),
    ("child:", "folders that contain a name"),
    ("file:", "files only"),
    ("folder:", "folders only"),
    ("empty:", "empty folders"),
    ("dupe:", "duplicates: dupe:size, dupe:content"),
    ("content:", "text inside files (slow)"),
    ("case:", "match case"),
    ("ww:", "whole words"),
    ("regex:", "regular expression"),
];
/// Result columns: title, default width and sort key. Name is always shown.
const COLUMNS: [(&str, i32, Sort); 8] = [
    ("Name", 300, Sort::Name),
    ("Path", 400, Sort::Path),
    ("Size", 95, Sort::Size),
    ("Modified", 155, Sort::Modified),
    ("Type", 140, Sort::Type),
    ("Created", 155, Sort::Created),
    ("Items", 70, Sort::Items),
    ("Run count", 75, Sort::RunCount),
];
fn build_columns(app: &AppRef) {
    let mut a = app.borrow_mut();
    let types: Rc<RefCell<std::collections::HashMap<Vec<u8>, String>>> = Rc::default();
    let mut columns = Vec::new();
    for (number, (title, width, sort)) in COLUMNS.into_iter().enumerate() {
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
            let offline = a.offline.clone();
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
                    cell.set_property("sensitive", !is_offline(&offline.borrow(), &e.path));
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
        if number == 2 || number == 6 || number == 7 {
            text.set_property("xalign", 1_f32);
        }
        gtk::prelude::TreeViewColumnExt::pack_start(&column, &text, true);
        let rows = a.data.clone();
        let types = types.clone();
        let highlight = a.highlight.clone();
        let offline = a.offline.clone();
        gtk::prelude::TreeViewColumnExt::set_cell_data_func(
            &column,
            &text,
            Some(Box::new(move |_, cell, model, iter| {
                let Ok(i) = model.value(iter, 0).get::<u32>() else {
                    return;
                };
                let rows = rows.borrow();
                let Some(r) = rows.as_ref() else {
                    return;
                };
                let Some(e) = r.entries.get(i as usize) else {
                    return;
                };
                // Entries of an unplugged drive are shown greyed.
                cell.set_property("sensitive", !is_offline(&offline.borrow(), &e.path));
                let value = match number {
                    0 => {
                        // Matches in bold; the Name column uses markup.
                        let markup = highlight_markup(&e.display_name(), &highlight.borrow());
                        cell.set_property("markup", markup);
                        return;
                    }
                    1 => e
                        .path_buf()
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    // Folders show the total size of the files inside them.
                    2 => match e.kind {
                        Kind::File | Kind::Folder => size(e.size),
                        _ => String::new(),
                    },
                    3 => modified(e),
                    4 => type_name(&types, e),
                    5 => created(e),
                    // How many files and folders a folder holds, at any depth.
                    6 => match e.kind {
                        Kind::Folder => count(e.items as usize),
                        _ => String::new(),
                    },
                    // Times the file was opened or launched through the app.
                    // Blank when never used or when results carry no counters.
                    _ => match r.counts.get(i as usize) {
                        Some(c) if *c > 0 => count(*c as usize),
                        _ => String::new(),
                    },
                };
                cell.set_property("text", value);
            })),
        );
        a.tree.append_column(&column);
        columns.push(column.clone());
        let weak = Rc::downgrade(app);
        column.connect_clicked(move |_| {
            with_app(&weak, |a| {
                if a.sort == sort {
                    a.descending = !a.descending;
                } else {
                    a.sort = sort;
                    a.descending = matches!(
                        sort,
                        Sort::Size | Sort::Modified | Sort::Items | Sort::RunCount
                    );
                }
                a.offset = 0;
                a.show_sort();
                a.remember();
                a.schedule();
            })
        });
    }
    a.columns = columns;
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
/// Selects every result. The selection handler updates the app, so the app
/// must not be borrowed while the selection changes.
fn select_all(w: &AppWeak) {
    let widgets = w.upgrade().map(|a| {
        let a = a.borrow();
        (a.tree.clone(), a.grid.clone(), a.memory.thumbnails)
    });
    if let Some((tree, grid, thumbnails)) = widgets {
        if thumbnails {
            grid.select_all();
        } else {
            tree.selection().select_all();
        }
    }
}
fn item(menu: &gtk::Menu, label: &str, action: impl Fn() + 'static) {
    let item = gtk::MenuItem::with_mnemonic(label);
    item.connect_activate(move |_| action());
    menu.append(&item);
}
fn build_menus(app: &AppRef, bar: &gtk::MenuBar) {
    for name in [
        "_File",
        "_Edit",
        "_View",
        "_Search",
        "_Bookmarks",
        "_Tools",
        "_Help",
    ] {
        let root = gtk::MenuItem::with_mnemonic(name);
        let menu = gtk::Menu::new();
        root.set_submenu(Some(&menu));
        bar.append(&root);
        let w = Rc::downgrade(app);
        match name {
            "_View" => {
                // Built before the saved window state is applied: read it directly.
                let (columns, hidden) = {
                    let a = app.borrow();
                    (
                        a.columns.clone(),
                        WindowMemory::load(&a.memory_path).hidden_columns,
                    )
                };
                for column in columns.into_iter().skip(1) {
                    let title = column.title().unwrap_or_default().to_string();
                    let check = gtk::CheckMenuItem::with_label(&title);
                    check.set_active(!hidden.contains(&title));
                    let t = w.clone();
                    check.connect_toggled(move |c| {
                        column.set_visible(c.is_active());
                        with_app(&t, |a| {
                            a.memory.hidden_columns.retain(|h| *h != title);
                            if !c.is_active() {
                                a.memory.hidden_columns.push(title.clone());
                            }
                            a.remember();
                        });
                    });
                    menu.append(&check);
                }
                menu.append(&gtk::SeparatorMenuItem::new());
                let preview = gtk::CheckMenuItem::with_mnemonic("_Preview Pane");
                preview.set_active(WindowMemory::load(&app.borrow().memory_path).preview);
                let t = w.clone();
                preview.connect_toggled(move |c| {
                    with_app(&t, |a| a.set_preview(c.is_active()));
                });
                menu.append(&preview);
                let thumbnails = gtk::CheckMenuItem::with_mnemonic("_Thumbnails");
                thumbnails.set_active(WindowMemory::load(&app.borrow().memory_path).thumbnails);
                let t = w.clone();
                thumbnails.connect_toggled(move |c| {
                    with_app(&t, |a| a.set_thumbnails(c.is_active()));
                });
                menu.append(&thumbnails);
            }
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
                menu.append(&gtk::SeparatorMenuItem::new());
                let t = w.clone();
                item(&menu, "_Export Results…", move || {
                    if let Some(a) = t.upgrade() {
                        App::export(&a);
                    }
                });
                menu.append(&gtk::SeparatorMenuItem::new());
                item(&menu, "_Quit", move || with_app(&w, App::quit));
            }
            "_Bookmarks" => {
                let t = w.clone();
                item(&menu, "_Add Bookmark…", move || {
                    if let Some(a) = t.upgrade() {
                        App::add_bookmark(&a);
                    }
                });
                let t = w.clone();
                item(&menu, "_Organize Bookmarks…", move || {
                    if let Some(a) = t.upgrade() {
                        App::options(&a);
                        if let Some(options) = &a.borrow().options {
                            options.select_tab("Bookmarks");
                        }
                    }
                });
                menu.append(&gtk::SeparatorMenuItem::new());
                app.borrow_mut().bookmarks_menu = menu.clone();
            }
            "_Edit" => {
                let undo = app.borrow().undo_item.clone();
                if let Some(label) = undo
                    .child()
                    .and_then(|c| c.downcast::<gtk::AccelLabel>().ok())
                {
                    use glib::translate::IntoGlib;
                    label.set_accel(
                        gdk::keys::constants::z.into_glib(),
                        gdk::ModifierType::CONTROL_MASK,
                    );
                }
                undo.set_sensitive(false);
                let t = w.clone();
                undo.connect_activate(move |_| with_app(&t, App::undo));
                menu.append(&undo);
                menu.append(&gtk::SeparatorMenuItem::new());
                let t = w.clone();
                item(&menu, "Copy Full _Path", move || {
                    with_app(&t, |a| a.copy_selected(false))
                });
                let t = w.clone();
                item(&menu, "Copy _Name", move || {
                    with_app(&t, |a| a.copy_selected(true))
                });
                item(&menu, "Select _All", move || select_all(&w));
            }
            "_Search" => {
                let t = w.clone();
                item(&menu, "_Focus Search", move || {
                    with_app(&t, App::focus_search)
                });
                let t = w.clone();
                item(&menu, "_Clear Search", move || {
                    with_app(&t, |a| {
                        a.entry.set_text("");
                        a.offset = 0;
                        a.schedule();
                        a.focus_search();
                    })
                });
                menu.append(&gtk::SeparatorMenuItem::new());
                // Recent searches are listed when the menu opens.
                let recent = gtk::MenuItem::with_mnemonic("_Recent Searches");
                let submenu = gtk::Menu::new();
                recent.set_submenu(Some(&submenu));
                menu.append(&recent);
                let t = w.clone();
                menu.connect_show(move |_| {
                    for child in submenu.children() {
                        submenu.remove(&child);
                    }
                    let Some(app) = t.upgrade() else {
                        return;
                    };
                    let items: Vec<String> = app
                        .borrow()
                        .history
                        .items()
                        .iter()
                        .take(25)
                        .map(|i| i.query.clone())
                        .collect();
                    if items.is_empty() {
                        let none = gtk::MenuItem::with_label("No recent searches");
                        none.set_sensitive(false);
                        submenu.append(&none);
                    }
                    for query in items {
                        let label = gtk::MenuItem::with_label(&query);
                        let t = t.clone();
                        label.connect_activate(move |_| {
                            with_app(&t, |a| a.set_search(&query));
                        });
                        submenu.append(&label);
                    }
                    submenu.show_all();
                });
                item(&menu, "Clear Search _History", move || {
                    with_app(&w, |a| {
                        a.history.clear();
                        a.refresh_completion();
                    })
                });
            }
            "_Tools" => {
                let t = w.clone();
                item(&menu, "Recent _Changes…", move || {
                    if let Some(a) = t.upgrade() {
                        App::recent_changes(&a);
                    }
                });
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
                        a.info("Search syntax","invoice    Filename contains invoice\next:rs;toml    File extensions\nfile: / folder:    Entry type\npath:/Projects    Search full paths\ncase:README    Case-sensitive\nsize:>10mb    Size filter\ndm:today, dm:lastweek, dm:2026, dm:last7days    Date modified\ndupe:, dupe:size, dupe:content    Duplicates among the results\nparent:/home/alex/Documents    Directly inside a folder\nregex:\"^report.*pdf$\"    Regular expression\n!draft    Exclude a term\n<apple banana> | orange    Grouping\n\nOR has precedence over AND. Type : in the search box (or press Ctrl+Space) for the list of operators, filters and bookmarks.")
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
/// Results drag out as files: to a file manager (copy or move), the
/// desktop, a browser's upload field or a mail. Set before the press
/// handlers, which keep a multiple selection while it is dragged; a drag
/// that starts clears `pending` (the narrowing on release).
fn drag_out<T: Clone + 'static>(
    widget: &gtk::Widget,
    app: &AppRef,
    pending: Rc<RefCell<Option<T>>>,
) {
    widget.drag_source_set(
        gdk::ModifierType::BUTTON1_MASK,
        &[
            gtk::TargetEntry::new("text/uri-list", gtk::TargetFlags::OTHER_APP, 0),
            gtk::TargetEntry::new("text/plain;charset=utf-8", gtk::TargetFlags::OTHER_APP, 1),
        ],
        gdk::DragAction::COPY | gdk::DragAction::MOVE | gdk::DragAction::LINK,
    );
    let w = Rc::downgrade(app);
    widget.connect_drag_begin(move |_, context| {
        pending.replace(None);
        with_app(&w, |a| {
            let icon: gio::Icon = match a.selected_items().first() {
                Some(item) if item.folder => gio::ThemedIcon::new("folder").upcast(),
                Some(item) => gio::content_type_get_icon(&item.mime),
                None => return,
            };
            context.drag_set_icon_gicon(&icon, 0, 0);
        });
    });
    let w = Rc::downgrade(app);
    widget.connect_drag_data_get(move |_, _, data, info, _| {
        with_app(&w, |a| {
            let paths = a.selected_paths();
            if info == 0 {
                let uris: Vec<String> = paths
                    .iter()
                    .map(|p| gio::File::for_path(p).uri().to_string())
                    .collect();
                data.set_uris(&uris.iter().map(String::as_str).collect::<Vec<_>>());
            } else {
                let text: Vec<String> = paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                data.set_text(&text.join("\n"));
            }
        })
    });
}
/// Keys of the result list and the thumbnail grid.
fn result_key(w: &AppWeak, e: &gdk::EventKey) -> glib::Propagation {
    use gdk::keys::constants as key;
    let mods = e.state()
        & (gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::MOD1_MASK
            | gdk::ModifierType::SHIFT_MASK);
    let ctrl = mods == gdk::ModifierType::CONTROL_MASK;
    match e.keyval() {
        key::c | key::C if ctrl => with_app(w, |a| a.clipboard(false)),
        key::x | key::X if ctrl => with_app(w, |a| a.clipboard(true)),
        key::a | key::A if ctrl => select_all(w),
        key::z | key::Z if ctrl => with_app(w, App::undo),
        key::BackSpace if mods.is_empty() => with_app(w, App::browse_up),
        key::Up if mods == gdk::ModifierType::MOD1_MASK => with_app(w, App::browse_up),
        key::Left if mods == gdk::ModifierType::MOD1_MASK => with_app(w, App::browse_previous),
        key::Delete | key::KP_Delete if mods.is_empty() => with_app(w, App::trash_selected),
        key::F2 if mods.is_empty() => with_app(w, App::rename_selected),
        key::Return | key::KP_Enter if mods == gdk::ModifierType::MOD1_MASK => {
            with_app(w, App::properties)
        }
        key::Menu => popup(w, None),
        key::F10 if mods == gdk::ModifierType::SHIFT_MASK => popup(w, None),
        _ => return glib::Propagation::Proceed,
    }
    glib::Propagation::Stop
}
fn connect_results(app: &AppRef) {
    let tree = app.borrow().tree.clone();
    // A press on an already selected row of a multiple selection: the
    // selection is kept for a drag, and narrowed to that row on release.
    let pending: Rc<RefCell<Option<gtk::TreePath>>> = Rc::default();
    drag_out(tree.upcast_ref(), app, pending.clone());
    let narrowing = pending.clone();
    let release_selection = tree.selection();
    tree.connect_button_release_event(move |tree, e| {
        if e.button() == 1
            && let Some(path) = narrowing.take()
        {
            release_selection.unselect_all();
            tree.set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
        }
        glib::Propagation::Proceed
    });
    let w = Rc::downgrade(app);
    tree.connect_row_activated(move |_, _, _| with_app(&w, |a| a.open_selected(false)));
    let w = Rc::downgrade(app);
    tree.selection().connect_changed(move |_| {
        with_app(&w, |a| {
            a.update_counts();
            a.publish();
            a.schedule_preview();
        })
    });
    let w = Rc::downgrade(app);
    tree.connect_key_press_event(move |_, e| result_key(&w, e));
    let selection = tree.selection();
    let w = Rc::downgrade(app);
    tree.connect_button_press_event(move |tree, e| {
        let modified = e
            .state()
            .intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
        if e.button() == 1 && e.event_type() == gdk::EventType::ButtonPress && !modified {
            let (x, y) = e.position();
            if let Some((Some(path), _, _, _)) = tree.path_at_pos(x as i32, y as i32)
                && selection.path_is_selected(&path)
                && selection.count_selected_rows() > 1
            {
                tree.grab_focus();
                pending.replace(Some(path));
                return glib::Propagation::Stop;
            }
        }
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
    connect_grid(app);
}
/// The thumbnail grid shows the same results; its selection is copied to
/// the list's, which every action reads.
fn connect_grid(app: &AppRef) {
    let (grid, tree, syncing) = {
        let a = app.borrow();
        (a.grid.clone(), a.tree.clone(), a.grid_syncing.clone())
    };
    let pending: Rc<RefCell<Option<gtk::TreePath>>> = Rc::default();
    drag_out(grid.upcast_ref(), app, pending.clone());
    {
        let (tree, syncing) = (tree.clone(), syncing.clone());
        grid.connect_selection_changed(move |grid| {
            if syncing.replace(true) {
                return;
            }
            let selection = tree.selection();
            selection.unselect_all();
            for path in grid.selected_items() {
                selection.select_path(&path);
            }
            syncing.set(false);
        });
    }
    let w = Rc::downgrade(app);
    grid.connect_item_activated(move |_, _| with_app(&w, |a| a.open_selected(false)));
    let w = Rc::downgrade(app);
    grid.connect_key_press_event(move |_, e| result_key(&w, e));
    let narrowing = pending.clone();
    grid.connect_button_release_event(move |grid, e| {
        if e.button() == 1
            && let Some(path) = narrowing.take()
        {
            grid.unselect_all();
            grid.select_path(&path);
            grid.set_cursor(&path, None::<&gtk::CellRenderer>, false);
        }
        glib::Propagation::Proceed
    });
    let w = Rc::downgrade(app);
    grid.connect_button_press_event(move |grid, e| {
        let (x, y) = e.position();
        let path = grid.path_at_pos(x as i32, y as i32);
        let modified = e
            .state()
            .intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
        if e.button() == 1 && e.event_type() == gdk::EventType::ButtonPress && !modified {
            if let Some(path) = path
                && grid.path_is_selected(&path)
                && grid.selected_items().len() > 1
            {
                grid.grab_focus();
                pending.replace(Some(path));
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        if e.button() == 3 {
            if let Some(path) = path
                && !grid.path_is_selected(&path)
            {
                grid.unselect_all();
                grid.select_path(&path);
            }
            popup(&w, Some(e));
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    let w = Rc::downgrade(app);
    let scroller = app.borrow().grid_scroller.clone();
    scroller
        .vadjustment()
        .connect_value_changed(move |_| with_app(&w, App::request_thumbnails));
    let w = Rc::downgrade(app);
    grid.connect_size_allocate(move |_, _| {
        let w = w.clone();
        glib::idle_add_local_once(move || with_app(&w, App::request_thumbnails));
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
    // A snap sees only its own applications; the portal asks instead.
    let kind = items[0].mime.clone();
    if file_ops::confined() {
        if items.len() == 1 {
            menu_item(
                &menu,
                "Open _With…",
                None,
                on(|a| a.portal_selected(file_ops::Portal::OpenWith)),
            );
        }
    } else if items.iter().all(|i| i.mime == kind) {
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
    if let [item] = items.as_slice()
        && item.folder
    {
        let folder = item.path.clone();
        let w = w.clone();
        menu_item(&menu, "_Browse in File Minnow", None, move || {
            with_app(&w, |a| {
                a.browse(&folder);
            })
        });
    }
    menu_item(
        &menu,
        "Open Containing _Folder",
        None,
        on(|a| a.show_in_folder()),
    );
    if !file_ops::confined() {
        menu_item(&menu, "Open in Ter_minal", None, on(App::terminal_here));
    }
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
    menu_item(
        &menu,
        "_Rename…",
        Some((key::F2, none)),
        on(App::rename_selected),
    );
    menu_item(
        &menu,
        "Mo_ve to Trash",
        Some((key::Delete, none)),
        on(App::trash_selected),
    );
    separator(&menu);
    let exclude = menu_item(
        &menu,
        "E_xclude from Index",
        None,
        on(App::exclude_selected),
    );
    exclude.set_tooltip_text(Some(
        "Leave these out of the index (files stay where they are). Undo in Options → Exclusions.",
    ));
    separator(&menu);
    let window = app.borrow().window.clone();
    // The default file manager decides whose actions and scripts appear.
    let manager = actions::Manager::from_desktop_id(
        gio::AppInfo::default_for_type("inode/directory", false)
            .and_then(|a| a.id())
            .as_deref(),
    );
    let mut submenus: Vec<(String, gtk::Menu)> = Vec::new();
    // A snap cannot run the file manager's actions and scripts.
    let confined = file_ops::confined();
    let loaded = if confined {
        Vec::new()
    } else {
        actions::load(manager)
    };
    for action in loaded {
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
    let scripts = if confined {
        Vec::new()
    } else {
        actions::scripts(manager)
    };
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
        state.query_selected = self.entry.selection_bounds().is_some();
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
    /// Focuses the search box with the cursor after the text, so typing
    /// adds to it (a folder scope from a file manager's "Search Here").
    fn focus_search_end(&mut self) {
        self.entry.grab_focus();
        self.entry.set_position(-1);
    }
    fn set_preview(&mut self, on: bool) {
        self.memory.preview = on;
        if on {
            self.preview.frame.show();
            let total = self.paned.allocated_width();
            if total > 0 {
                self.paned
                    .set_position((total - self.memory.preview_width.max(160)).max(200));
            }
            self.schedule_preview();
        } else {
            self.preview.frame.hide();
        }
        self.remember();
    }
    fn set_thumbnails(&mut self, on: bool) {
        self.memory.thumbnails = on;
        self.results
            .set_visible_child_name(if on { "grid" } else { "list" });
        if on {
            self.fill_grid();
        } else {
            self.grid.set_model(None::<&gtk::TreeModel>);
            self.grid_generation.fetch_add(1, Ordering::Relaxed);
        }
        self.remember();
    }
    /// Shows the current results in the grid: file type icons at once,
    /// thumbnails as they are made for the items on screen.
    fn fill_grid(&mut self) {
        self.grid_generation.fetch_add(1, Ordering::Relaxed);
        self.thumb_queue.0.lock().unwrap().clear();
        let store = gtk::ListStore::new(&[
            gtk::gdk_pixbuf::Pixbuf::static_type(),
            String::static_type(),
            u32::static_type(),
        ]);
        let theme = gtk::IconTheme::default();
        let mut icons: std::collections::HashMap<String, Option<gtk::gdk_pixbuf::Pixbuf>> =
            std::collections::HashMap::new();
        let data = self.data.borrow();
        let entries = data.as_ref().map(|r| r.entries.as_slice()).unwrap_or(&[]);
        for (i, e) in entries.iter().enumerate() {
            let key = match e.kind {
                Kind::Folder => "inode/directory".to_owned(),
                _ => file_ops::guess_type(&e.path_buf()).0.to_string(),
            };
            let icon = icons
                .entry(key.clone())
                .or_insert_with(|| {
                    let gicon: gio::Icon = if key == "inode/directory" {
                        gio::ThemedIcon::new("folder").upcast()
                    } else {
                        gio::content_type_get_icon(&key)
                    };
                    theme.as_ref().and_then(|t| {
                        t.lookup_by_gicon(&gicon, GRID_SIZE / 2, gtk::IconLookupFlags::FORCE_SIZE)
                            .and_then(|info| info.load_icon().ok())
                    })
                })
                .clone();
            store.insert_with_values(
                None,
                &[(0, &icon), (1, &e.display_name()), (2, &(i as u32))],
            );
        }
        drop(data);
        self.grid_requested = vec![false; store.iter_n_children(None) as usize];
        self.grid.set_model(Some(&store));
        self.grid.set_pixbuf_column(0);
        self.grid.set_text_column(1);
        // The list's selection (kept across live updates) shows in the grid.
        self.grid_syncing.set(true);
        for path in self.tree.selection().selected_rows().0 {
            self.grid.select_path(&path);
        }
        self.grid_syncing.set(false);
        self.request_thumbnails();
    }
    /// Asks for thumbnails of the items on screen and a little beyond.
    fn request_thumbnails(&mut self) {
        if !self.memory.thumbnails {
            return;
        }
        let Some((first, last)) = self.grid.visible_range() else {
            return;
        };
        let index = |p: &gtk::TreePath| p.indices().first().copied().unwrap_or(0).max(0) as usize;
        let (first, last) = (index(&first).saturating_sub(24), index(&last) + 48);
        let generation = self.grid_generation.load(Ordering::Relaxed);
        let data = self.data.borrow();
        let entries = data.as_ref().map(|r| r.entries.as_slice()).unwrap_or(&[]);
        let mut jobs = Vec::new();
        for i in (first..=last.min(entries.len().saturating_sub(1))).rev() {
            if self.grid_requested.get(i) != Some(&false) || entries[i].kind != Kind::File {
                continue;
            }
            self.grid_requested[i] = true;
            let path = entries[i].path_buf();
            let mime = file_ops::guess_type(&path).0.to_string();
            jobs.push(ThumbJob {
                generation,
                index: i as u32,
                path,
                mime,
            });
        }
        if !jobs.is_empty() {
            let (queue, ready) = &*self.thumb_queue;
            queue.lock().unwrap().extend(jobs);
            ready.notify_one();
        }
    }
    /// Loads the preview shortly after the selection settles.
    fn schedule_preview(&mut self) {
        if let Some(timer) = self.preview_timer.take() {
            timer.remove();
        }
        if !self.memory.preview {
            return;
        }
        let messages = self.messages.clone();
        self.preview_timer = Some(glib::timeout_add_local_once(
            Duration::from_millis(120),
            move || {
                let _ = messages.try_send(Message::LoadPreview);
            },
        ));
    }
    fn load_preview(&mut self) {
        self.preview_timer = None;
        if !self.memory.preview {
            return;
        }
        self.preview_id += 1;
        let id = self.preview_id;
        let selected = self.selected();
        let Some(entry) = selected.first().cloned() else {
            self.preview.header.set_text("");
            self.preview
                .show_content(PreviewContent::Info(String::new()));
            return;
        };
        self.preview.header.set_markup(&format!(
            "<b>{}</b>",
            glib::markup_escape_text(&entry.display_name())
        ));
        if selected.len() > 1 {
            self.preview.show_content(PreviewContent::Info(format!(
                "{} items selected",
                count(selected.len())
            )));
            return;
        }
        let max_width = (self.preview.frame.allocated_width() - 12).max(160);
        let max_height = (self.preview.frame.allocated_height() - 60).max(160);
        let summary = format!(
            "Location: {}\nType: {}\nSize: {}\nModified: {}\nCreated: {}",
            entry.display_path(),
            entry.kind.as_str(),
            size(entry.size),
            modified(&entry),
            created(&entry)
        );
        let messages = self.messages.clone();
        std::thread::spawn(move || {
            let content = preview_content(&entry, max_width, max_height)
                .unwrap_or(PreviewContent::Info(summary));
            let _ = messages.send_blocking(Message::Preview(id, content));
        });
    }
    /// Runs `query` as if typed.
    fn set_search(&mut self, query: &str) {
        self.entry.set_text(query);
        self.offset = 0;
        self.schedule();
        self.focus_search();
        self.entry.set_position(-1);
    }
    /// Remembers the current search (when the user acts on it: opens a
    /// result, hides the window...).
    fn record_history(&mut self) {
        let ui = self.backend.settings().ui;
        let text = self.entry.text();
        if ui.history_enabled
            && self
                .history
                .record(&text, ui.history_days, ui.history_limit)
        {
            self.refresh_completion();
        }
    }
    fn refresh_completion(&mut self) {
        self.completion.clear();
        for item in self.history.items() {
            self.completion
                .insert_with_values(None, &[(0, &item.query), (1, &""), (2, &0)]);
        }
        let settings = self.backend.settings();
        let operators = OPERATORS
            .iter()
            .map(|(text, about)| ((*text).to_owned(), (*about).to_owned()));
        let types = crate::query::TYPE_FILTERS
            .iter()
            .map(|(label, text, _)| ((*text).to_owned(), format!("{label} files")));
        let filters = settings
            .ui
            .filters
            .iter()
            .map(|f| (f.query.clone(), format!("Filter: {}", f.name)));
        let bookmarks = settings
            .bookmarks
            .iter()
            .map(|b| (b.query.clone(), format!("Bookmark: {}", b.name)));
        for (text, about) in operators.chain(types).chain(filters).chain(bookmarks) {
            self.completion
                .insert_with_values(None, &[(0, &text), (1, &about), (2, &1)]);
        }
    }
    /// Rebuilds the Bookmarks menu and the filter drop-down when their
    /// settings changed (Options, CLI or another instance).
    fn sync_saved_searches(&mut self, settings: &Settings) {
        if settings.bookmarks != self.shown_bookmarks {
            let children = self.bookmarks_menu.children();
            // Keep Add, Organize and the separator; replace the rest.
            for child in children.iter().skip(3) {
                self.bookmarks_menu.remove(child);
            }
            if settings.bookmarks.is_empty() {
                let none = gtk::MenuItem::with_label("No bookmarks yet");
                none.set_sensitive(false);
                self.bookmarks_menu.append(&none);
            }
            for bookmark in &settings.bookmarks {
                let item = gtk::MenuItem::with_label(&bookmark.name);
                item.set_tooltip_text(Some(&bookmark.query));
                let query = bookmark.query.clone();
                let messages = self.messages.clone();
                item.connect_activate(move |_| {
                    let _ = messages.try_send(Message::RunSearch(query.clone()));
                });
                self.bookmarks_menu.append(&item);
            }
            self.bookmarks_menu.show_all();
            self.shown_bookmarks = settings.bookmarks.clone();
            self.refresh_completion();
        }
        if settings.ui.filters != self.shown_filters {
            let active = self.filter.active_id().map(|id| id.to_string());
            self.filter_rebuilding.set(true);
            fill_filters(&self.filter, &settings.ui.filters);
            if !self.filter.set_active_id(active.as_deref()) {
                self.filter.set_active_id(Some(""));
            }
            self.filter_rebuilding.set(false);
            self.shown_filters = settings.ui.filters.clone();
            self.refresh_completion();
        }
    }
    /// Saves the current search as a bookmark.
    fn add_bookmark(app: &AppRef) {
        let (window, query) = {
            let a = app.borrow();
            (a.window.clone(), a.entry.text().to_string())
        };
        let dialog = gtk::Dialog::with_buttons(
            Some("Add Bookmark"),
            Some(&window),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Add", gtk::ResponseType::Ok),
            ],
        );
        dialog.set_default_response(gtk::ResponseType::Ok);
        dialog.set_default_width(440);
        let grid = gtk::Grid::new();
        grid.set_column_spacing(8);
        grid.set_row_spacing(6);
        grid.set_margin_start(12);
        grid.set_margin_end(12);
        grid.set_margin_top(12);
        let name = gtk::Entry::new();
        name.set_text(&query);
        name.set_activates_default(true);
        let search = gtk::Entry::new();
        search.set_text(&query);
        search.set_activates_default(true);
        for (row, label, entry) in [(0, "Name:", &name), (1, "Search:", &search)] {
            let caption = gtk::Label::new(Some(label));
            caption.set_xalign(0.);
            grid.attach(&caption, 0, row, 1, 1);
            entry.set_hexpand(true);
            grid.attach(entry, 1, row, 1, 1);
        }
        let error = gtk::Label::new(None);
        error.set_xalign(0.);
        error.set_line_wrap(true);
        grid.attach(&error, 0, 2, 2, 1);
        dialog.content_area().pack_start(&grid, false, false, 0);
        dialog.show_all();
        name.grab_focus();
        name.select_region(0, -1);
        let weak = Rc::downgrade(app);
        dialog.connect_response(move |dialog, response| {
            if response != gtk::ResponseType::Ok {
                dialog.close();
                return;
            }
            let bookmark = config::Bookmark {
                name: name.text().trim().to_owned(),
                query: search.text().trim().to_owned(),
            };
            let mut outcome = Ok(());
            with_app(&weak, |a| {
                let mut settings = a.backend.settings();
                settings.bookmarks.push(bookmark.clone());
                outcome = settings.validate();
                if outcome.is_ok() {
                    a.apply_settings(settings, false);
                }
            });
            match outcome {
                Ok(()) => dialog.close(),
                Err(e) => error.set_text(&e.to_string()),
            }
        });
    }
    /// Writes every result of the current search to a CSV or text file.
    fn export(app: &AppRef) {
        let (window, query, sort, descending, backend, messages) = {
            let a = app.borrow();
            (
                a.window.clone(),
                a.requested_key.0.clone(),
                a.sort,
                a.descending,
                a.backend.clone(),
                a.messages.clone(),
            )
        };
        let dialog = gtk::FileChooserDialog::with_buttons(
            Some("Export Results"),
            Some(&window),
            gtk::FileChooserAction::Save,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Export", gtk::ResponseType::Accept),
            ],
        );
        dialog.set_modal(true);
        dialog.set_do_overwrite_confirmation(true);
        dialog.set_current_name("file-minnow-results.csv");
        for (name, pattern) in [
            ("CSV (name, path, size, dates, type)", "*.csv"),
            ("Text, one full path per line", "*.txt"),
            ("Everything file list (EFU)", "*.efu"),
        ] {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some(name));
            filter.add_pattern(pattern);
            dialog.add_filter(filter);
        }
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept
                && let Some(path) = dialog.filename()
            {
                let extension = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase());
                let format = match extension.as_deref() {
                    Some("txt") => Export::Text,
                    Some("efu") => Export::Efu,
                    _ => Export::Csv,
                };
                let query = query.clone();
                let backend = backend.clone();
                let messages = messages.clone();
                std::thread::spawn(move || {
                    let report =
                        match export_results(&backend, &query, sort, descending, &path, format) {
                            Ok(n) => format!("Exported {} results to {}", count(n), path.display()),
                            Err(e) => format!("Export failed: {e}"),
                        };
                    let _ = messages.send_blocking(Message::Error(report));
                });
            }
            dialog.close();
        });
        dialog.show_all();
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
            self.record_history();
            // The next opening starts a new search; the old one is in the
            // history. Cleared now so it never flashes when shown.
            if self.backend.settings().ui.clear_search_on_open && !self.entry.text().is_empty() {
                self.entry.set_text("");
                self.offset = 0;
                self.schedule();
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
            Sort::Created => "Created",
            Sort::Items => "Items",
            Sort::RunCount => "Run count",
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
        self.record_history();
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
        // Nothing searched yet: show an empty list instead of everything,
        // without running a search (the version bump above drops any
        // search still in flight).
        self.hide_until_search = self.backend.settings().ui.hide_results_until_search;
        self.waiting_for_search = self.hide_until_search && query.trim().is_empty();
        if self.waiting_for_search {
            self.offset = 0;
            self.replace_results(SearchResponse {
                entries: Vec::new(),
                total: 0,
                generation: self.last_generation,
                elapsed_ms: 0.,
                counts: Vec::new(),
            });
            return;
        }
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
        self.sync_saved_searches(&settings);
        let offline: Vec<Vec<u8>> = status
            .offline
            .iter()
            .map(|r| r.as_bytes().to_vec())
            .collect();
        if *self.offline.borrow() != offline {
            *self.offline.borrow_mut() = offline;
            self.tree.queue_draw();
        }
        if status.generation != self.last_generation
            || settings.ui.hide_results_until_search != self.hide_until_search
        {
            if status.generation != self.last_generation
                && let Some(changes) = &self.changes
            {
                changes.reload();
            }
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
        } else if self.waiting_for_search && s.entries > 0 {
            self.spinner.stop();
            self.status.set_text("Type to search");
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
        self.update_counts();
        self.publish();
    }
    /// The counts at the right of the status bar: what is selected, then
    /// the index and search totals.
    fn update_counts(&mut self) {
        let entries = self.backend.status().entries;
        let mut counts = if let Some(r) = self.data.borrow().as_ref()
            && !self.waiting_for_search
        {
            format!(
                "{} indexed  |  {} {}  |  {:.1} ms",
                count(entries),
                count(r.total),
                if r.total == 1 { "match" } else { "matches" },
                r.elapsed_ms
            )
        } else {
            format!("{} indexed", count(entries))
        };
        let selected = self.selected();
        if !selected.is_empty() {
            let bytes: u64 = selected
                .iter()
                .filter(|e| matches!(e.kind, Kind::File | Kind::Folder))
                .map(|e| e.size)
                .sum();
            counts = format!(
                "{} selected ({})  |  {counts}",
                count(selected.len()),
                size(bytes)
            );
        }
        self.counts.set_text(&counts);
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
        *self.highlight.borrow_mut() = crate::query::Query::parse(&self.requested_key.0)
            .map(|q| q.highlight_terms())
            .unwrap_or_default();
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
        if self.memory.thumbnails {
            self.fill_grid();
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
        // The selection changed with the model; the app was busy, so the
        // selection handler could not refresh the preview.
        self.schedule_preview();
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
                                        a.focus_search_end();
                                        a.publish();
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
            Message::RunsRecorded => app.borrow_mut().schedule(),
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
            Message::RunSearch(query) => {
                app.borrow_mut().set_search(&query);
            }
            Message::LoadPreview => app.borrow_mut().load_preview(),
            Message::Thumbnail(generation, index, content) => {
                let a = app.borrow();
                if generation == a.grid_generation.load(Ordering::Relaxed)
                    && let PreviewContent::Image {
                        bytes,
                        width,
                        height,
                        stride,
                        alpha,
                    } = content
                    && let Some(store) = a
                        .grid
                        .model()
                        .and_then(|m| m.downcast::<gtk::ListStore>().ok())
                    && let Some(iter) = store.iter_nth_child(None, index as i32)
                {
                    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
                        &bytes,
                        gtk::gdk_pixbuf::Colorspace::Rgb,
                        alpha,
                        8,
                        width,
                        height,
                        stride,
                    );
                    store.set_value(&iter, 0, &pixbuf.to_value());
                }
            }
            Message::Renamed(outcome) => {
                let mut a = app.borrow_mut();
                let count = outcome.done.len();
                let text = match &outcome.error {
                    None if count == 1 => "Renamed 1 item (Ctrl+Z undoes it)".to_owned(),
                    None => format!("Renamed {count} items (Ctrl+Z undoes it)"),
                    Some(e) => format!("Renamed {count} items, then stopped: {e}"),
                };
                if count > 0 {
                    a.push_undo(Undo::Renamed(outcome.done));
                }
                if outcome.error.is_some() {
                    a.info("Rename stopped", &text);
                }
                a.status.set_text(&text);
                a.publish();
            }
            Message::Trashed(moved, text) => {
                let mut a = app.borrow_mut();
                // Items trashed through the portal (a snap) cannot be put back.
                if !moved.is_empty() && !file_ops::confined() {
                    a.push_undo(Undo::Trashed(moved));
                }
                a.spinner.stop();
                a.status.set_text(&text);
                a.publish();
            }
            Message::Undone(result) => {
                let mut a = app.borrow_mut();
                a.spinner.stop();
                match result {
                    Ok(text) => a.status.set_text(&text),
                    Err(e) => {
                        a.status.set_text(&e);
                        a.info("Cannot undo", &e);
                    }
                }
                a.publish();
            }
            Message::Preview(id, content) => {
                let a = app.borrow();
                if id == a.preview_id && a.memory.preview {
                    a.preview.show_content(content);
                }
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
        if file_ops::confined()
            && let Some(home) = file_ops::real_home()
        {
            chooser.set_current_folder(home);
        }
        let weak = Rc::downgrade(app);
        chooser.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let paths = dialog.filenames();
                with_app(&weak, |a| {
                    let mut settings = a.backend.settings();
                    for path in paths {
                        if !settings.index.roots.iter().any(|r| r.path.path() == path) {
                            settings
                                .index
                                .roots
                                .push(config::RootConfig::for_folder(&path));
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
        self.record_history();
        if !parent && self.backend.settings().ui.open_folders_inside {
            let selected = self.selected();
            if let [only] = selected.as_slice()
                && only.kind == Kind::Folder
                && self.browse(&only.path_buf())
            {
                return;
            }
        }
        let paths = self.selected_paths();
        if paths.len() > 20 {
            self.info(
                "Too many files selected",
                "Select 20 or fewer files to open together.",
            );
            return;
        }
        open_paths(paths, parent, self.messages.clone(), self.backend.clone());
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
        match file_ops::launch(info, &paths, self.launch_context().as_ref()) {
            Ok(()) => {
                self.backend.record_runs(&paths);
                self.schedule();
            }
            Err(e) => self.error(&format!("Cannot start {}: {e}", info.display_name())),
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
        let backend = self.backend.clone();
        let messages = self.messages.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Ok
                && let Some(info) = dialog.app_info()
            {
                match file_ops::launch(&info, std::slice::from_ref(&path), context.as_ref()) {
                    Ok(()) => {
                        backend.record_runs(std::slice::from_ref(&path));
                        let _ = messages.try_send(Message::RunsRecorded);
                    }
                    Err(e) => {
                        status.set_text(&format!("Cannot start {}: {e}", info.display_name()));
                    }
                }
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
        if file_ops::confined() {
            self.portal_selected(file_ops::Portal::ShowInFolder);
            return;
        }
        let tx = self.messages.clone();
        let backend = self.backend.clone();
        let fallback = paths.clone();
        file_ops::file_manager("ShowItems", &paths, move |answered| {
            if !answered {
                open_paths(fallback, true, tx, backend);
            }
        });
    }
    /// Asks the desktop portal to act on the selection, off the main thread.
    fn portal_selected(&mut self, action: file_ops::Portal) {
        let paths = self.selected_paths();
        let tx = self.messages.clone();
        let backend = self.backend.clone();
        std::thread::spawn(move || {
            let mut recorded = false;
            for path in paths.iter().take(20) {
                if let Err(e) = file_ops::portal(action, path) {
                    let _ = tx.send_blocking(Message::Error(format!(
                        "Cannot open {}: {e}",
                        path.display()
                    )));
                    break;
                }
                if matches!(action, file_ops::Portal::Open | file_ops::Portal::OpenWith) {
                    backend.record_runs(std::slice::from_ref(path));
                    recorded = true;
                }
            }
            if recorded {
                let _ = tx.send_blocking(Message::RunsRecorded);
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
        let selected = self.selected();
        if selected.len() > 1 {
            let items = selected
                .iter()
                .map(|e| rename::Item {
                    path: e.path_buf(),
                    folder: e.kind == Kind::Folder,
                    modified: e.modified,
                })
                .collect();
            let tx = self.messages.clone();
            crate::rename_ui::show(&self.window, items, move |changes| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send_blocking(Message::Renamed(rename::apply(&changes)));
                });
            });
            return;
        }
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
        let tx = self.messages.clone();
        dialog.connect_response(move |dialog, response| {
            if response != gtk::ResponseType::Ok {
                dialog.close();
                return;
            }
            match file_ops::rename(&path, &entry.text()) {
                Ok(target) => {
                    if target != path {
                        let _ = tx.try_send(Message::Renamed(rename::Outcome {
                            done: vec![(path.clone(), target)],
                            error: None,
                        }));
                    }
                    dialog.close()
                }
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
    /// Shows Tools > Recent Changes (or brings it forward).
    fn recent_changes(app: &AppRef) {
        let mut a = app.borrow_mut();
        if let Some(window) = &a.changes {
            window.window.present();
            return;
        }
        let tx = a.messages.clone();
        let backend = a.backend.clone();
        let open: crate::changes_ui::Open = Rc::new(move |path, folder| {
            open_paths(vec![path], folder, tx.clone(), backend.clone());
        });
        let window = crate::changes_ui::ChangesWindow::new(&a.window, &a.data_dir, open);
        let weak = Rc::downgrade(app);
        window
            .window
            .connect_destroy(move |_| with_app(&weak, |a| a.changes = None));
        a.changes = Some(window);
    }
    /// Takes back the last rename or move to the Trash.
    fn undo(&mut self) {
        let Some(step) = self.undo.pop() else {
            return;
        };
        self.refresh_undo();
        self.busy("Undoing…");
        let tx = self.messages.clone();
        std::thread::spawn(move || {
            let result = step
                .run(&rename::TrashDirs::from_env())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send_blocking(Message::Undone(result));
        });
    }
    fn push_undo(&mut self, step: Undo) {
        self.undo.push(step);
        if self.undo.len() > 50 {
            self.undo.remove(0);
        }
        self.refresh_undo();
    }
    fn refresh_undo(&self) {
        let label = self
            .undo
            .last()
            .map_or_else(|| "Undo".to_owned(), Undo::label);
        self.undo_item
            .set_label(&label.replacen("Undo", "_Undo", 1));
        self.undo_item.set_use_underline(true);
        self.undo_item.set_sensitive(!self.undo.is_empty());
    }
    /// The folder being browsed: the search is exactly `parent:"FOLDER"`.
    fn browsed(&self) -> Option<PathBuf> {
        let text = self.entry.text();
        let folder = text.strip_prefix("parent:\"")?.strip_suffix('"')?;
        (!folder.is_empty() && !folder.contains('"')).then(|| PathBuf::from(folder))
    }
    /// Lists what is in `folder` (a `parent:` search) and keeps the
    /// results focused, like a file manager. False when the path cannot be
    /// written as a search.
    fn browse(&mut self, folder: &std::path::Path) -> bool {
        let Some(text) = folder.to_str().filter(|t| !t.contains('"')) else {
            return false;
        };
        self.browse_back.push(self.entry.text().into());
        if self.browse_back.len() > 100 {
            self.browse_back.remove(0);
        }
        self.set_search(&format!("parent:\"{text}\""));
        self.tree.grab_focus();
        true
    }
    fn browse_up(&mut self) {
        if let Some(parent) = self.browsed().as_deref().and_then(std::path::Path::parent) {
            let parent = parent.to_path_buf();
            self.browse(&parent);
        }
    }
    fn browse_previous(&mut self) {
        if let Some(previous) = self.browse_back.pop() {
            self.set_search(&previous);
            if self.browsed().is_some() {
                self.tree.grab_focus();
            }
        }
    }
    /// Adds the selected files and folders to the excluded paths; the
    /// indexer then drops just those entries.
    fn exclude_selected(&mut self) {
        let mut settings = self.backend.settings();
        let before = settings.index.excluded_folders.len();
        for path in self.selected_paths() {
            let raw = config::RawPath::new(&path);
            if !settings.index.excluded_folders.contains(&raw) {
                settings.index.excluded_folders.push(raw);
            }
        }
        if settings.index.excluded_folders.len() == before {
            return;
        }
        self.busy("Excluding from the index…");
        let backend = self.backend.clone();
        let tx = self.messages.clone();
        std::thread::spawn(move || {
            let result = backend.apply_settings(settings).map_err(|e| e.to_string());
            let _ = tx.send_blocking(Message::Settings(Box::new(result), false));
        });
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
/// Decodes an image (scaled to fit) or reads the start of a text file, off
/// the main thread. None means "show the summary instead".
fn preview_content(entry: &Entry, max_width: i32, max_height: i32) -> Option<PreviewContent> {
    const TEXT_BYTES: usize = 64 * 1024;
    const IMAGE_BYTES: u64 = 80 << 20;
    if entry.kind != Kind::File {
        return None;
    }
    let path = entry.path_buf();
    let (kind, _) = file_ops::guess_type(&path);
    if kind.starts_with("image/")
        && entry.size <= IMAGE_BYTES
        && let Some(pixbuf) =
            gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&path, max_width, max_height, true)
                .ok()
                .and_then(|p| p.apply_embedded_orientation())
    {
        return image_content(&pixbuf);
    }
    if gio::content_type_is_a(&kind, "text/plain") || kind == "application/json" {
        use std::io::Read;
        let mut file = std::fs::File::open(&path).ok()?;
        let mut bytes = Vec::with_capacity(TEXT_BYTES.min(entry.size as usize + 1));
        file.by_ref()
            .take(TEXT_BYTES as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.iter().take(4096).any(|b| *b == 0) {
            return None;
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if entry.size as usize > TEXT_BYTES {
            text.push_str("\n…");
        }
        return Some(PreviewContent::Text(text));
    }
    // Videos, documents, fonts and pictures GTK cannot decode: the
    // desktop's thumbnail.
    let size = max_width.min(max_height).clamp(128, 256);
    let thumbnail = crate::thumbnails::thumbnail(&path, &kind, size, thumbnailers())?;
    image_content(&thumbnail)
}
/// The desktop's thumbnailers, read once.
fn thumbnailers() -> &'static crate::thumbnails::Thumbnailers {
    static THUMBNAILERS: std::sync::OnceLock<crate::thumbnails::Thumbnailers> =
        std::sync::OnceLock::new();
    THUMBNAILERS.get_or_init(crate::thumbnails::Thumbnailers::load)
}
/// Pixels of an 8-bit RGB(A) picture, to send to the main thread.
fn image_content(pixbuf: &gtk::gdk_pixbuf::Pixbuf) -> Option<PreviewContent> {
    if pixbuf.bits_per_sample() != 8 || pixbuf.colorspace() != gtk::gdk_pixbuf::Colorspace::Rgb {
        return None;
    }
    Some(PreviewContent::Image {
        bytes: pixbuf.read_pixel_bytes(),
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride(),
        alpha: pixbuf.has_alpha(),
    })
}
/// Fetches all results page by page and writes them; returns the count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Export {
    Csv,
    /// One full path per line.
    Text,
    /// An Everything file list.
    Efu,
}
fn export_results(
    backend: &Backend,
    query: &str,
    sort: Sort,
    descending: bool,
    path: &std::path::Path,
    format: Export,
) -> Result<usize> {
    use std::io::Write;
    let file = std::fs::File::create(path)?;
    let mut out = std::io::BufWriter::new(file);
    match format {
        Export::Csv => writeln!(out, "Name,Path,Size,Modified,Created,Type")?,
        Export::Efu => crate::efu::write_header(&mut out)?,
        Export::Text => {}
    }
    let csv = |field: &str| -> String {
        if field.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", field.replace('"', "\"\""))
        } else {
            field.to_owned()
        }
    };
    let mut written = 0;
    let mut offset = 0;
    loop {
        let page = backend.search(
            &SearchRequest {
                query: query.into(),
                sort,
                descending,
                limit: 10_000,
                offset,
            },
            None,
        )?;
        if page.entries.is_empty() {
            break;
        }
        for e in &page.entries {
            if format == Export::Efu {
                writeln!(out, "{}", crate::efu::line(e))?;
            } else if format == Export::Text {
                out.write_all(&e.path)?;
                out.write_all(b"\n")?;
            } else {
                let parent = e
                    .path_buf()
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let size = match e.kind {
                    Kind::File | Kind::Folder => e.size.to_string(),
                    _ => String::new(),
                };
                writeln!(
                    out,
                    "{},{},{},{},{},{}",
                    csv(&e.display_name()),
                    csv(&parent),
                    size,
                    modified(e),
                    created(e),
                    e.kind.as_str()
                )?;
            }
            written += 1;
        }
        offset += page.entries.len();
        if offset >= page.total {
            break;
        }
    }
    out.flush()?;
    Ok(written)
}
/// Opens entries (or their folders) with the desktop's default applications.
/// Opens of the files themselves (not their folders) are counted.
fn open_paths(
    paths: Vec<PathBuf>,
    parent: bool,
    tx: async_channel::Sender<Message>,
    backend: Backend,
) {
    std::thread::spawn(move || {
        let mut recorded = false;
        for mut path in paths {
            if parent && let Some(p) = path.parent() {
                path = p.to_path_buf();
            }
            let result = std::fs::symlink_metadata(&path)
                .map_err(anyhow::Error::from)
                .and_then(|_| {
                    if file_ops::confined() {
                        file_ops::portal(file_ops::Portal::Open, &path)
                    } else {
                        file_ops::spawn(
                            &["xdg-open".into(), path.clone().into_os_string()],
                            None,
                            &[],
                        )
                    }
                });
            match result {
                Ok(()) if !parent => {
                    backend.record_runs(std::slice::from_ref(&path));
                    recorded = true;
                }
                Ok(()) => {}
                Err(error) => {
                    let _ = tx.send_blocking(Message::Error(format!(
                        "Cannot open {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        if recorded {
            let _ = tx.send_blocking(Message::RunsRecorded);
        }
    });
}
/// Moves entries to the Trash off the main thread; the index follows
/// through the folder watcher.
fn trash_paths(paths: Vec<PathBuf>, tx: async_channel::Sender<Message>) {
    std::thread::spawn(move || {
        let failures = file_ops::trash(&paths);
        let text = match failures.as_slice() {
            [] if paths.len() == 1 => "Moved 1 item to the Trash (Ctrl+Z restores it)".to_owned(),
            [] => format!(
                "Moved {} items to the Trash (Ctrl+Z restores them)",
                paths.len()
            ),
            [(_, only)] => format!("Cannot move to the Trash: {only}"),
            [(_, first), rest @ ..] => format!(
                "Cannot move {} items to the Trash: {first} (and {} more)",
                failures.len(),
                rest.len()
            ),
        };
        let moved = paths
            .into_iter()
            .filter(|p| !failures.iter().any(|(f, _)| f == p))
            .collect();
        let _ = tx.send_blocking(Message::Trashed(moved, text));
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
