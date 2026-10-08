//! Real modal GTK Options dialog. All edits are staged until Apply/OK.
use crate::config::{self, RawPath, RootConfig, Schedule, Settings};
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

pub struct OptionsWindow {
    pub dialog: gtk::Dialog,
    pub draft: Rc<RefCell<Settings>>,
    pub error_label: gtk::Label,
    body: gtk::Notebook,
}
impl OptionsWindow {
    pub fn new(parent: &gtk::Window, settings: Settings, record: impl Fn(bool) + 'static) -> Self {
        let dialog = gtk::Dialog::with_buttons(
            Some("File Minnow Options"),
            Some(parent),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Apply", gtk::ResponseType::Apply),
                ("OK", gtk::ResponseType::Ok),
            ],
        );
        dialog.set_default_size(680, 510);
        dialog.set_resizable(true);
        dialog.set_default_response(gtk::ResponseType::Ok);
        let draft = Rc::new(RefCell::new(settings));
        let body = gtk::Notebook::new();
        body.set_margin_start(8);
        body.set_margin_end(8);
        body.set_margin_top(8);
        dialog.content_area().pack_start(&body, true, true, 0);
        let error_label = gtk::Label::new(None);
        error_label.set_line_wrap(true);
        error_label.set_xalign(0.);
        error_label.set_margin_start(12);
        error_label.set_margin_end(12);
        error_label.set_margin_top(4);
        dialog
            .content_area()
            .pack_end(&error_label, false, false, 4);
        let record: Rc<dyn Fn(bool)> = Rc::new(record);
        // While a shortcut is being recorded, every key belongs to the recorder.
        let recording = Rc::new(Cell::new(false));
        body.append_page(
            &folders(&dialog, draft.clone(), error_label.clone()),
            Some(&gtk::Label::new(Some("Folders"))),
        );
        body.append_page(
            &exclusions(&dialog, draft.clone()),
            Some(&gtk::Label::new(Some("Exclusions"))),
        );
        body.append_page(
            &file_types(draft.clone(), error_label.clone()),
            Some(&gtk::Label::new(Some("File Types"))),
        );
        body.append_page(
            &search(draft.clone()),
            Some(&gtk::Label::new(Some("Search"))),
        );
        body.append_page(
            &keyboard(
                draft.clone(),
                record.clone(),
                recording.clone(),
                error_label.clone(),
            ),
            Some(&gtk::Label::new(Some("Keyboard"))),
        );
        body.append_page(
            &appearance(draft.clone()),
            Some(&gtk::Label::new(Some("Interface"))),
        );
        let key_dialog = dialog.clone();
        let capturing = recording.clone();
        dialog.connect_key_press_event(move |_, event| {
            if capturing.get() {
                return glib::Propagation::Proceed;
            }
            if event.keyval() == gdk::keys::constants::Escape {
                key_dialog.response(gtk::ResponseType::Cancel);
                return glib::Propagation::Stop;
            }
            if event.state().contains(gdk::ModifierType::CONTROL_MASK)
                && event.keyval() == gdk::keys::constants::Return
            {
                key_dialog.response(gtk::ResponseType::Ok);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        dialog.connect_destroy(move |_| record(false));
        Self {
            dialog,
            draft,
            error_label,
            body,
        }
    }
    pub fn sensitive(&self, value: bool) {
        self.body.set_sensitive(value);
        self.dialog
            .set_response_sensitive(gtk::ResponseType::Apply, value);
        self.dialog
            .set_response_sensitive(gtk::ResponseType::Ok, value);
    }
    pub fn saved(&self, settings: &Settings) {
        self.draft.borrow_mut().revision = settings.revision;
        self.error_label.set_text("");
    }
    pub fn error(&self, error: &str) {
        self.error_label.set_text(error);
    }
}
fn page() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    page.set_margin_start(12);
    page.set_margin_end(12);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page
}
fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.);
    label.set_line_wrap(true);
    label
}
fn list() -> (gtk::ScrolledWindow, gtk::TreeView, gtk::ListStore) {
    let store = gtk::ListStore::new(&[String::static_type()]);
    let tree = gtk::TreeView::with_model(&store);
    tree.set_headers_visible(false);
    let column = gtk::TreeViewColumn::new();
    let cell = gtk::CellRendererText::new();
    cell.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
    gtk::prelude::TreeViewColumnExt::pack_start(&column, &cell, true);
    gtk::prelude::TreeViewColumnExt::add_attribute(&column, &cell, "text", 0);
    tree.append_column(&column);
    let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroll.set_shadow_type(gtk::ShadowType::In);
    scroll.set_min_content_height(140);
    scroll.add(&tree);
    (scroll, tree, store)
}
fn chosen(tree: &gtk::TreeView) -> Option<usize> {
    let (model, iter) = tree.selection().selected()?;
    model.path(&iter)?.indices().first().map(|i| *i as usize)
}
fn fill_roots(store: &gtk::ListStore, draft: &Settings) {
    store.clear();
    for r in &draft.index.roots {
        store.insert_with_values(None, &[(0, &r.path.display())]);
    }
}
fn chooser(parent: &gtk::Dialog, title: &str, done: impl Fn(Vec<std::path::PathBuf>) + 'static) {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some(title),
        Some(parent),
        gtk::FileChooserAction::SelectFolder,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Select", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_modal(true);
    dialog.set_select_multiple(true);
    dialog.connect_response(move |d, r| {
        if r == gtk::ResponseType::Accept {
            done(d.filenames());
        }
        d.close();
    });
    dialog.show_all();
}
fn folders(dialog: &gtk::Dialog, draft: Rc<RefCell<Settings>>, error: gtk::Label) -> gtk::Box {
    let page = page();
    page.pack_start(&caption("Folders to index"), false, false, 0);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let (scroll, tree, store) = list();
    fill_roots(&store, &draft.borrow());
    row.pack_start(&scroll, true, true, 0);
    let buttons = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let add = gtk::Button::with_label("Add Folder…");
    let remove = gtk::Button::with_label("Remove");
    buttons.pack_start(&add, false, false, 0);
    buttons.pack_start(&remove, false, false, 0);
    row.pack_start(&buttons, false, false, 0);
    page.pack_start(&row, true, true, 0);
    let policy = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let enabled = gtk::CheckButton::with_label("Include this folder in the index");
    let recursive = gtk::CheckButton::with_label("Include subfolders");
    let monitor = gtk::CheckButton::with_label("Monitor changes");
    for button in [&enabled, &recursive, &monitor] {
        policy.pack_start(button, false, false, 0);
    }
    let schedule = gtk::ComboBoxText::new();
    for (id, label) in [
        ("interval", "Rescan at intervals"),
        ("daily", "Rescan daily"),
        ("never", "Rescan manually only"),
    ] {
        schedule.append(Some(id), label);
    }
    // Interval is edited in minutes; the stored schedule keeps seconds.
    let interval = gtk::SpinButton::with_range(1., 525_600., 5.);
    interval.set_numeric(true);
    let hour = gtk::SpinButton::with_range(0., 23., 1.);
    let minute = gtk::SpinButton::with_range(0., 59., 1.);
    let timing = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    timing.pack_start(&schedule, false, false, 0);
    // Only the controls for the chosen schedule are shown.
    let every = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    every.pack_start(&caption("every"), false, false, 0);
    every.pack_start(&interval, false, false, 0);
    every.pack_start(&caption("minutes"), false, false, 0);
    let daily = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    daily.pack_start(&caption("at"), false, false, 0);
    daily.pack_start(&hour, false, false, 0);
    daily.pack_start(&caption(":"), false, false, 0);
    daily.pack_start(&minute, false, false, 0);
    for group in [&every, &daily] {
        group.show_all();
        group.set_no_show_all(true);
        timing.pack_start(group, false, false, 0);
    }
    policy.pack_start(&timing, false, false, 0);
    page.pack_start(&policy, false, false, 0);
    page.pack_start(&caption("Folder changes take effect when you choose Apply or OK. Removing a folder never deletes its files."),false,false,0);
    let selected = Rc::new(Cell::new(None::<usize>));
    let loading = Rc::new(Cell::new(false));
    policy.set_sensitive(false);
    {
        let draft = draft.clone();
        let selected = selected.clone();
        let loading = loading.clone();
        let policy = policy.clone();
        let enabled = enabled.clone();
        let recursive = recursive.clone();
        let monitor = monitor.clone();
        let schedule = schedule.clone();
        let interval = interval.clone();
        let hour = hour.clone();
        let minute = minute.clone();
        tree.selection().connect_changed(move |s| {
            loading.set(true);
            let index = s
                .selected()
                .and_then(|(model, iter)| model.path(&iter))
                .and_then(|p| p.indices().first().copied())
                .map(|v| v as usize);
            selected.set(index);
            if let Some(r) = index.and_then(|i| draft.borrow().index.roots.get(i).cloned()) {
                policy.set_sensitive(true);
                enabled.set_active(r.enabled);
                recursive.set_active(r.recursive);
                monitor.set_active(r.monitor);
                match r.schedule {
                    Schedule::Interval { seconds } => {
                        schedule.set_active_id(Some("interval"));
                        interval.set_value((seconds / 60).max(1) as f64);
                    }
                    Schedule::Daily { hour: h, minute: m } => {
                        schedule.set_active_id(Some("daily"));
                        hour.set_value(h as f64);
                        minute.set_value(m as f64);
                    }
                    Schedule::Never => {
                        schedule.set_active_id(Some("never"));
                    }
                }
            } else {
                policy.set_sensitive(false);
            }
            loading.set(false);
        });
    }
    for (button, field) in [(enabled, 0), (recursive, 1), (monitor, 2)] {
        let draft = draft.clone();
        let selected = selected.clone();
        let loading = loading.clone();
        button.connect_toggled(move |b| {
            if !loading.get()
                && let Some(i) = selected.get()
                && let Some(root) = draft.borrow_mut().index.roots.get_mut(i)
            {
                match field {
                    0 => root.enabled = b.is_active(),
                    1 => root.recursive = b.is_active(),
                    _ => root.monitor = b.is_active(),
                }
            }
        });
    }
    let update: Rc<dyn Fn()> = Rc::new({
        let draft = draft.clone();
        let selected = selected.clone();
        let loading = loading.clone();
        let schedule = schedule.clone();
        let interval = interval.clone();
        let hour = hour.clone();
        let minute = minute.clone();
        let every = every.clone();
        let daily = daily.clone();
        move || {
            let mode = schedule.active_id().unwrap_or_else(|| "interval".into());
            every.set_visible(mode == "interval");
            daily.set_visible(mode == "daily");
            if !loading.get()
                && let Some(i) = selected.get()
                && let Some(root) = draft.borrow_mut().index.roots.get_mut(i)
            {
                root.schedule = match mode.as_str() {
                    "daily" => Schedule::Daily {
                        hour: hour.value_as_int() as u32,
                        minute: minute.value_as_int() as u32,
                    },
                    "never" => Schedule::Never,
                    _ => Schedule::Interval {
                        seconds: interval.value() as u64 * 60,
                    },
                };
            }
        }
    });
    {
        let update = update.clone();
        schedule.connect_changed(move |_| update());
    }
    for spin in [interval, hour, minute] {
        let update = update.clone();
        spin.connect_value_changed(move |_| update());
    }
    {
        let dialog = dialog.clone();
        let draft = draft.clone();
        let store = store.clone();
        let tree = tree.clone();
        add.connect_clicked(move |_| {
            let draft = draft.clone();
            let store = store.clone();
            let error = error.clone();
            let tree = tree.clone();
            chooser(&dialog, "Add Folder", move |paths| {
                let mut config = draft.borrow_mut();
                for path in paths {
                    if !config.index.roots.iter().any(|r| r.path.path() == path) {
                        config.index.roots.push(RootConfig::new(&path));
                    }
                }
                fill_roots(&store, &config);
                drop(config);
                error.set_text("");
                if let Some(iter) = store.iter_first() {
                    tree.selection().select_iter(&iter);
                }
            });
        });
    }
    {
        let draft = draft.clone();
        let store = store.clone();
        let tree = tree.clone();
        remove.connect_clicked(move |_| {
            if let Some(i) = chosen(&tree) {
                let mut config = draft.borrow_mut();
                config.index.roots.remove(i);
                fill_roots(&store, &config);
                drop(config);
                if let Some(iter) = store.iter_first() {
                    tree.selection().select_iter(&iter);
                }
            }
        });
    }
    // Select the first folder so its settings are visible straight away.
    if let Some(iter) = store.iter_first() {
        tree.selection().select_iter(&iter);
    }
    page
}
fn exclusions(dialog: &gtk::Dialog, draft: Rc<RefCell<Settings>>) -> gtk::Box {
    let page = page();
    let hidden = gtk::CheckButton::with_label("Exclude hidden files and folders");
    hidden.set_active(draft.borrow().index.exclude_hidden);
    page.pack_start(&hidden, false, false, 0);
    {
        let d = draft.clone();
        hidden.connect_toggled(move |b| d.borrow_mut().index.exclude_hidden = b.is_active());
    }
    page.pack_start(&caption("Excluded folders"), false, false, 0);
    let (scroll, tree, store) = list();
    for path in &draft.borrow().index.excluded_folders {
        store.insert_with_values(None, &[(0, &path.display())]);
    }
    page.pack_start(&scroll, true, true, 0);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add = gtk::Button::with_label("Add Folder…");
    let remove = gtk::Button::with_label("Remove");
    row.pack_start(&add, false, false, 0);
    row.pack_start(&remove, false, false, 0);
    page.pack_start(&row, false, false, 0);
    {
        let parent = dialog.clone();
        let d = draft.clone();
        let model = store.clone();
        add.connect_clicked(move |_| {
            let d = d.clone();
            let model = model.clone();
            chooser(&parent, "Exclude Folder", move |paths| {
                for path in paths {
                    let raw = RawPath::new(&path);
                    d.borrow_mut().index.excluded_folders.push(raw.clone());
                    model.insert_with_values(None, &[(0, &raw.display())]);
                }
            });
        });
    }
    {
        let d = draft.clone();
        remove.connect_clicked(move |_| {
            if let Some(i) = chosen(&tree) {
                d.borrow_mut().index.excluded_folders.remove(i);
                if let Some((_, iter)) = tree.selection().selected() {
                    store.remove(&iter);
                }
            }
        });
    }
    for (title, include) in [
        (
            "Include only files (one pattern per line; empty includes all)",
            true,
        ),
        ("Exclude files (one pattern per line)", false),
    ] {
        page.pack_start(&caption(title), false, false, 0);
        let view = gtk::TextView::new();
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        let buffer = view.buffer().unwrap();
        buffer.set_text(&if include {
            draft.borrow().index.included_files.join("\n")
        } else {
            draft.borrow().index.excluded_files.join("\n")
        });
        let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scroll.set_min_content_height(48);
        scroll.set_shadow_type(gtk::ShadowType::In);
        scroll.add(&view);
        page.pack_start(&scroll, false, false, 0);
        let d = draft.clone();
        buffer.connect_changed(move |b| {
            let text = b
                .text(&b.start_iter(), &b.end_iter(), false)
                .unwrap_or_default();
            let patterns = text
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if include {
                d.borrow_mut().index.included_files = patterns;
            } else {
                d.borrow_mut().index.excluded_files = patterns;
            }
        });
    }
    page
}
/// Descriptions for default types the desktop's MIME database does not know.
fn known_type(ext: &str) -> &'static str {
    match ext {
        "gch" | "pch" => "Precompiled header",
        "lo" => "Libtool object",
        "rlib" => "Rust library",
        "rmeta" => "Rust metadata",
        "swp" | "swo" => "Vim swap file",
        "tmp" | "temp" => "Temporary file",
        _ => "",
    }
}
/// Excluded file types: a sorted list with the desktop's description of each
/// type, an entry to add one, and a way back to the defaults.
fn file_types(draft: Rc<RefCell<Settings>>, error: gtk::Label) -> gtk::Box {
    let page = page();
    let enabled = gtk::CheckButton::with_label("Exclude these file types from the index");
    enabled.set_active(draft.borrow().index.exclude_file_types);
    page.pack_start(&enabled, false, false, 0);
    page.pack_start(
        &caption("Files most people never search for: temporary files, partial downloads, editor swap files and compiler output. Folders are never excluded by type."),
        false,
        false,
        0,
    );
    let store = gtk::ListStore::new(&[String::static_type(), String::static_type()]);
    let tree = gtk::TreeView::with_model(&store);
    tree.selection().set_mode(gtk::SelectionMode::Multiple);
    for (title, column) in [("Type", 0), ("Description", 1)] {
        let cell = gtk::CellRendererText::new();
        if column == 1 {
            cell.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
        }
        let view = gtk::TreeViewColumn::new();
        view.set_title(title);
        view.set_resizable(true);
        view.set_expand(column == 1);
        gtk::prelude::TreeViewColumnExt::pack_start(&view, &cell, true);
        gtk::prelude::TreeViewColumnExt::add_attribute(&view, &cell, "text", column);
        tree.append_column(&view);
    }
    let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_shadow_type(gtk::ShadowType::In);
    scroll.set_min_content_height(180);
    scroll.add(&tree);
    page.pack_start(&scroll, true, true, 0);
    let fill = {
        let store = store.clone();
        let draft = draft.clone();
        let tree = tree.clone();
        Rc::new(move |select: Option<&str>| {
            let mut types = draft.borrow().index.excluded_file_types.clone();
            types.sort();
            store.clear();
            for ext in &types {
                let (kind, uncertain) =
                    crate::file_ops::guess_type(std::path::Path::new(&format!("file.{ext}")));
                let description = if uncertain && !crate::file_ops::known_type(&kind) {
                    known_type(ext).to_owned()
                } else {
                    gio::content_type_get_description(&kind).to_string()
                };
                store.insert_with_values(None, &[(0, &format!(".{ext}")), (1, &description)]);
            }
            if let Some(select) = select
                && let Some(i) = types.iter().position(|t| t == select)
            {
                let path = gtk::TreePath::from_indicesv(&[i as i32]);
                tree.selection().unselect_all();
                tree.selection().select_path(&path);
                tree.scroll_to_cell(Some(&path), None::<&gtk::TreeViewColumn>, false, 0., 0.);
            }
        })
    };
    fill(None);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let input = gtk::Entry::new();
    input.set_placeholder_text(Some("Type, e.g. bak or tar.gz"));
    input.set_width_chars(18);
    let add = gtk::Button::with_label("Add");
    let remove = gtk::Button::with_label("Remove");
    let defaults = gtk::Button::with_label("Restore Defaults");
    row.pack_start(&input, false, false, 0);
    row.pack_start(&add, false, false, 0);
    row.pack_start(&remove, false, false, 0);
    row.pack_end(&defaults, false, false, 0);
    page.pack_start(&row, false, false, 0);
    let controls: Vec<gtk::Widget> = vec![scroll.clone().upcast(), row.clone().upcast()];
    let sensitive = move |on: bool| controls.iter().for_each(|w| w.set_sensitive(on));
    sensitive(enabled.is_active());
    {
        let d = draft.clone();
        enabled.connect_toggled(move |b| {
            d.borrow_mut().index.exclude_file_types = b.is_active();
            sensitive(b.is_active());
        });
    }
    let add_type = {
        let d = draft.clone();
        let fill = fill.clone();
        let input = input.clone();
        move || {
            let text = input.text();
            let Some(ext) = config::normalize_file_type(&text) else {
                error.set_text(
                    "Enter a file type such as bak or tar.gz (letters, digits and dots).",
                );
                return;
            };
            error.set_text("");
            {
                let types = &mut d.borrow_mut().index.excluded_file_types;
                if !types.contains(&ext) {
                    types.push(ext.clone());
                }
            }
            input.set_text("");
            fill(Some(&ext));
        }
    };
    let add_type = Rc::new(add_type);
    {
        let add_type = add_type.clone();
        add.connect_clicked(move |_| add_type());
    }
    input.connect_activate(move |_| add_type());
    {
        let d = draft.clone();
        let fill = fill.clone();
        remove.connect_clicked(move |_| {
            let (paths, _) = tree.selection().selected_rows();
            let doomed: Vec<String> = paths
                .iter()
                .filter_map(|path| store.iter(path))
                .filter_map(|iter| store.value(&iter, 0).get::<String>().ok())
                .map(|t| t.trim_start_matches('.').to_owned())
                .collect();
            d.borrow_mut()
                .index
                .excluded_file_types
                .retain(|t| !doomed.contains(t));
            fill(None);
        });
    }
    {
        let d = draft.clone();
        defaults.connect_clicked(move |_| {
            d.borrow_mut().index.excluded_file_types = config::DEFAULT_EXCLUDED_FILE_TYPES
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            fill(None);
        });
    }
    page
}
fn search(draft: Rc<RefCell<Settings>>) -> gtk::Box {
    let page = page();
    page.pack_start(&caption("Search as you type"), false, false, 0);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let delay = gtk::SpinButton::with_range(0., 1000., 5.);
    delay.set_value(draft.borrow().ui.search_delay_ms as f64);
    row.pack_start(&caption("Search delay:"), false, false, 0);
    row.pack_start(&delay, false, false, 0);
    row.pack_start(&caption("milliseconds"), false, false, 0);
    page.pack_start(&row, false, false, 0);
    {
        let d = draft.clone();
        delay.connect_value_changed(move |s| d.borrow_mut().ui.search_delay_ms = s.value() as u64);
    }
    page.pack_start(&caption("Search is focused automatically whenever the window opens. Results update without fades, smooth-scroll effects or other transitions."),false,false,6);
    page.pack_start(&caption("Default matching ignores ASCII letter case. Use case: for case-sensitive matching, path: for paths, and < > to group terms. OR is evaluated before AND."),false,false,6);
    page
}
fn keyboard(
    draft: Rc<RefCell<Settings>>,
    record: Rc<dyn Fn(bool)>,
    recording: Rc<Cell<bool>>,
    error: gtk::Label,
) -> gtk::Box {
    let page = page();
    page.pack_start(&caption("Global show / hide shortcut"), false, false, 0);
    let entry = gtk::Entry::new();
    entry.set_editable(false);
    entry.set_text(&draft.borrow().ui.global_shortcut);
    entry.set_placeholder_text(Some("Click here and press a key combination"));
    page.pack_start(&entry, false, false, 0);
    page.pack_start(&caption("Click the field, then press the shortcut on your keyboard. Escape cancels recording. Backspace clears the shortcut. Apply saves the change. If nothing happens, the desktop already uses that combination (Cinnamon: Keyboard → Shortcuts)."),false,false,0);
    // Recording starts on a click (or Enter/Space on the focused field), never
    // on mere focus, so tabbing through the window leaves Escape free to close it.
    let arm: Rc<dyn Fn(&gtk::Entry)> = {
        let recording = recording.clone();
        let record = record.clone();
        Rc::new(move |e: &gtk::Entry| {
            recording.set(true);
            record(true);
            e.set_text("Press a key combination… (Escape cancels)");
        })
    };
    {
        let arm = arm.clone();
        entry.connect_button_press_event(move |e, _| {
            arm(e);
            glib::Propagation::Proceed
        });
    }
    {
        let recording = recording.clone();
        let record = record.clone();
        let d = draft.clone();
        entry.connect_focus_out_event(move |e, _| {
            if recording.replace(false) {
                e.set_text(&d.borrow().ui.global_shortcut);
                record(false);
            }
            glib::Propagation::Proceed
        });
    }
    {
        let recording = recording.clone();
        let record = record.clone();
        let d = draft.clone();
        entry.connect_key_press_event(move |e, event| {
            if !recording.get() {
                use gdk::keys::constants as key;
                let keyval = event.keyval();
                if [key::Return, key::KP_Enter, key::space].contains(&keyval) {
                    arm(e);
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }
            if event.is_modifier() {
                return glib::Propagation::Stop;
            }
            if event.keyval() == gdk::keys::constants::Escape {
                e.set_text(&d.borrow().ui.global_shortcut);
                recording.set(false);
                record(false);
                return glib::Propagation::Stop;
            }
            let mods = event.state();
            if event.keyval() == gdk::keys::constants::BackSpace
                && !(mods.intersects(
                    gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::MOD1_MASK
                        | gdk::ModifierType::SUPER_MASK,
                ))
            {
                d.borrow_mut().ui.global_shortcut.clear();
                e.set_text("");
                recording.set(false);
                record(false);
                error.set_text("");
                return glib::Propagation::Stop;
            }
            let mut parts = Vec::new();
            if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                parts.push("Ctrl".to_owned());
            }
            if mods.contains(gdk::ModifierType::MOD1_MASK) {
                parts.push("Alt".to_owned());
            }
            if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                parts.push("Shift".to_owned());
            }
            if mods.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::MOD4_MASK) {
                parts.push("Super".to_owned());
            }
            let name = event
                .keyval()
                .name()
                .map(|n| n.to_string())
                .unwrap_or_default();
            parts.push(match name.as_str() {
                "space" => "Space".into(),
                "Return" => "Enter".into(),
                s if s.len() == 1 => s.to_ascii_uppercase(),
                s => s.to_owned(),
            });
            let binding = parts.join("+");
            match crate::shortcuts::Chord::parse(&binding) {
                Ok(chord)
                    if chord.ctrl
                        || chord.alt
                        || chord.super_key
                        || (chord.key.len() > 1 && chord.key.starts_with('F')) =>
                {
                    d.borrow_mut().ui.global_shortcut = binding.clone();
                    e.set_text(&binding);
                    recording.set(false);
                    record(false);
                    error.set_text("");
                }
                Ok(_) => error.set_text("Use Ctrl, Alt or Super with a key, or a function key."),
                Err(e) => error.set_text(&e.to_string()),
            }
            glib::Propagation::Stop
        });
    }
    page.pack_start(&caption("Window shortcuts"), false, false, 12);
    let grid = gtk::Grid::new();
    grid.set_column_spacing(24);
    grid.set_row_spacing(4);
    for (row, (keys, action)) in [
        ("Ctrl+L", "Focus search"),
        ("Ctrl+,", "Options"),
        ("Ctrl+Q", "Quit"),
        ("F5", "Rescan"),
        ("Enter", "Open selected files"),
        ("Ctrl+C", "Copy selected paths"),
        ("Ctrl+A", "Select all results"),
    ]
    .into_iter()
    .enumerate()
    {
        grid.attach(&caption(keys), 0, row as i32, 1, 1);
        grid.attach(&caption(action), 1, row as i32, 1, 1);
    }
    page.pack_start(&grid, false, false, 0);
    page.pack_start(
        &caption("Use the tray menu or your global shortcut to bring the window back."),
        false,
        false,
        12,
    );
    page
}
fn appearance(draft: Rc<RefCell<Settings>>) -> gtk::Box {
    let page = page();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let theme = gtk::ComboBoxText::new();
    for (name, label) in [
        ("system", "Follow desktop theme"),
        ("light", "Light"),
        ("dark", "Dark"),
    ] {
        theme.append(Some(name), label);
    }
    theme.set_active_id(Some(&draft.borrow().ui.theme));
    row.pack_start(&caption("Theme:"), false, false, 0);
    row.pack_start(&theme, false, false, 0);
    page.pack_start(&row, false, false, 0);
    {
        let d = draft.clone();
        theme.connect_changed(move |c| {
            if let Some(id) = c.active_id() {
                d.borrow_mut().ui.theme = id.to_string();
            }
        });
    }
    page.pack_start(&caption("Fonts, selection colors, buttons and dialogs use the native desktop toolkit. Animations and kinetic scrolling are disabled."),false,false,4);
    for (label, field, value) in [
        ("Show a tray icon", 0, draft.borrow().ui.tray_enabled),
        (
            "Keep running in the tray when the window is closed",
            1,
            draft.borrow().ui.close_to_tray,
        ),
        (
            "Start hidden when a tray is available",
            2,
            draft.borrow().ui.start_hidden,
        ),
    ] {
        let toggle = gtk::CheckButton::with_label(label);
        toggle.set_active(value);
        let d = draft.clone();
        toggle.connect_toggled(move |b| match field {
            0 => d.borrow_mut().ui.tray_enabled = b.is_active(),
            1 => d.borrow_mut().ui.close_to_tray = b.is_active(),
            _ => d.borrow_mut().ui.start_hidden = b.is_active(),
        });
        page.pack_start(&toggle, false, false, 0);
    }
    page.pack_start(
        &caption("Without a tray host, the app stays visible and closing the window quits it."),
        false,
        false,
        4,
    );
    page
}
