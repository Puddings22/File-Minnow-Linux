//! Tools > Recent Changes: what was created, deleted, changed, moved or
//! renamed in the indexed folders (see `journal`), newest first.
use crate::journal::{Action, Change, Journal};
use gtk::{glib, prelude::*};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

pub struct ChangesWindow {
    pub window: gtk::Window,
    journal: Journal,
    store: gtk::ListStore,
    filter: gtk::SearchEntry,
    period: gtk::ComboBoxText,
    shown: Rc<RefCell<Vec<Change>>>,
    status: gtk::Label,
}
/// Opens a path (`false`), or shows it in its folder (`true`).
pub type Open = Rc<dyn Fn(PathBuf, bool)>;

fn when(time: i64) -> String {
    use chrono::TimeZone;
    let Some(local) = chrono::Local.timestamp_opt(time, 0).earliest() else {
        return String::new();
    };
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("Today %H:%M").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M").to_string()
    }
}
fn split(path: &crate::config::RawPath) -> (String, String) {
    let path = path.path();
    (
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path.parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}
impl ChangesWindow {
    pub fn new(parent: &gtk::Window, dir: &std::path::Path, open: Open) -> Rc<Self> {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Recent Changes");
        window.set_transient_for(Some(parent));
        window.set_default_size(820, 520);
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 6);
        layout.set_margin_start(8);
        layout.set_margin_end(8);
        layout.set_margin_top(8);
        layout.set_margin_bottom(8);
        window.add(&layout);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let filter = gtk::SearchEntry::new();
        filter.set_placeholder_text(Some("Filter by name or folder"));
        filter.set_hexpand(true);
        let period = gtk::ComboBoxText::new();
        for (id, label) in [
            ("3600", "Last hour"),
            ("today", "Today"),
            ("604800", "Last 7 days"),
            ("2592000", "Last 30 days"),
        ] {
            period.append(Some(id), label);
        }
        period.set_active_id(Some("604800"));
        top.pack_start(&filter, true, true, 0);
        top.pack_start(&period, false, false, 0);
        layout.pack_start(&top, false, false, 0);
        // Time, change, name, folder, index into `shown`.
        let store = gtk::ListStore::new(&[
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            u32::static_type(),
        ]);
        let tree = gtk::TreeView::with_model(&store);
        for (title, column, width) in [
            ("Time", 0, 130),
            ("Change", 1, 80),
            ("Name", 2, 260),
            ("Folder", 3, 300),
        ] {
            let cell = gtk::CellRendererText::new();
            cell.set_property("ellipsize", gtk::pango::EllipsizeMode::Middle);
            let view = gtk::TreeViewColumn::new();
            view.set_title(title);
            view.set_resizable(true);
            view.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            view.set_fixed_width(width);
            gtk::prelude::TreeViewColumnExt::pack_start(&view, &cell, true);
            gtk::prelude::TreeViewColumnExt::add_attribute(&view, &cell, "text", column);
            tree.append_column(&view);
        }
        tree.set_fixed_height_mode(true);
        let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scroll.set_shadow_type(gtk::ShadowType::In);
        scroll.add(&tree);
        layout.pack_start(&scroll, true, true, 0);
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let status = gtk::Label::new(None);
        status.set_xalign(0.);
        let open_button = gtk::Button::with_mnemonic("_Open");
        let folder_button = gtk::Button::with_mnemonic("Open Containing _Folder");
        bottom.pack_start(&status, true, true, 0);
        bottom.pack_end(&folder_button, false, false, 0);
        bottom.pack_end(&open_button, false, false, 0);
        layout.pack_start(&bottom, false, false, 0);
        let this = Rc::new(Self {
            window: window.clone(),
            journal: Journal::new(dir),
            store,
            filter: filter.clone(),
            period: period.clone(),
            shown: Rc::default(),
            status,
        });
        let chosen = {
            let (tree, shown) = (tree.clone(), this.shown.clone());
            move || -> Option<Change> {
                let (model, iter) = tree.selection().selected()?;
                let index = model.value(&iter, 4).get::<u32>().ok()?;
                shown.borrow().get(index as usize).cloned()
            }
        };
        let act = {
            let (open, chosen) = (open.clone(), chosen.clone());
            move |folder: bool| {
                if let Some(change) = chosen() {
                    let path = change.current().path();
                    if folder || path.exists() {
                        open(path, folder);
                    }
                }
            }
        };
        {
            let act = act.clone();
            open_button.connect_clicked(move |_| act(false));
        }
        {
            let act = act.clone();
            folder_button.connect_clicked(move |_| act(true));
        }
        {
            let act = act.clone();
            tree.connect_row_activated(move |_, _, _| act(false));
        }
        {
            let (chosen, open_button) = (chosen.clone(), open_button.clone());
            tree.selection().connect_changed(move |_| {
                let deleted = chosen().is_none_or(|c| c.action == Action::Deleted);
                open_button.set_sensitive(!deleted);
            });
        }
        {
            let weak = Rc::downgrade(&this);
            filter.connect_search_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.reload();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            period.connect_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.reload();
                }
            });
        }
        window.connect_key_press_event(|window, event| {
            if event.keyval() == gtk::gdk::keys::constants::Escape {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        this.reload();
        window.show_all();
        filter.grab_focus();
        this
    }
    /// Reads the history again (after an index update, or a new filter).
    pub fn reload(&self) {
        let now = chrono::Local::now();
        let since = match self.period.active_id().as_deref() {
            Some("today") => now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|t| t.and_local_timezone(chrono::Local).earliest())
                .map_or(0, |t| t.timestamp()),
            Some(seconds) => now.timestamp() - seconds.parse::<i64>().unwrap_or(604_800),
            None => 0,
        };
        let wanted = self.filter.text().to_lowercase();
        let changes: Vec<Change> = self
            .journal
            .read(since)
            .into_iter()
            .filter(|c| {
                wanted.is_empty()
                    || c.path.display().to_lowercase().contains(&wanted)
                    || c.to
                        .as_ref()
                        .is_some_and(|t| t.display().to_lowercase().contains(&wanted))
            })
            .take(20_000)
            .collect();
        self.store.clear();
        for (index, change) in changes.iter().enumerate() {
            let (name, folder) = split(&change.path);
            let (name, folder) = match &change.to {
                Some(to) => {
                    let (new_name, new_folder) = split(to);
                    if new_folder == folder {
                        (format!("{name} → {new_name}"), folder)
                    } else {
                        (new_name, format!("{folder} → {new_folder}"))
                    }
                }
                None => (name, folder),
            };
            self.store.insert_with_values(
                None,
                &[
                    (0, &when(change.time)),
                    (1, &change.label()),
                    (2, &name),
                    (3, &folder),
                    (4, &(index as u32)),
                ],
            );
        }
        self.status.set_text(&match changes.len() {
            0 => "No changes in this period".to_owned(),
            1 => "1 change".to_owned(),
            n => format!("{n} changes"),
        });
        *self.shown.borrow_mut() = changes;
    }
}
