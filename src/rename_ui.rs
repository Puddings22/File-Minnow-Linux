//! The window for renaming several results at once: a rule, a preview of
//! every new name, then Rename. File Minnow applies the renames and keeps
//! them for Edit > Undo.
use crate::rename::{self, Case, Item, Options, Planned, Rule};
use gtk::prelude::*;
use std::{path::PathBuf, rc::Rc};

/// Shows the window; `rename` receives the confirmed `(old, new)` paths.
pub fn show(
    parent: &gtk::Window,
    items: Vec<Item>,
    rename: impl Fn(Vec<(PathBuf, PathBuf)>) + 'static,
) {
    let dialog = gtk::Dialog::with_buttons(
        Some(&format!("Rename {} Items", items.len())),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Rename", gtk::ResponseType::Ok),
        ],
    );
    dialog.set_default_size(680, 520);
    dialog.set_default_response(gtk::ResponseType::Ok);
    let content = dialog.content_area();
    content.set_spacing(8);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    let label = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.);
        label
    };
    let mode = gtk::ComboBoxText::new();
    mode.append(Some("replace"), "Find and replace");
    mode.append(Some("pattern"), "New name from a pattern");
    mode.append(Some("case"), "Change upper and lower case");
    mode.set_active_id(Some("replace"));
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.pack_start(&label("Rename by:"), false, false, 0);
    top.pack_start(&mode, false, false, 0);
    content.pack_start(&top, false, false, 0);

    let grid = |rows: &[(&str, &gtk::Widget)]| {
        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(8);
        for (row, (text, widget)) in rows.iter().enumerate() {
            grid.attach(&label(text), 0, row as i32, 1, 1);
            widget.set_hexpand(true);
            grid.attach(*widget, 1, row as i32, 1, 1);
        }
        grid
    };
    let find = gtk::Entry::new();
    let with = gtk::Entry::new();
    let regex = gtk::CheckButton::with_label("Regular expression ($1 inserts the first group)");
    let case_sensitive = gtk::CheckButton::with_label("Match case");
    let replace_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    replace_page.pack_start(
        &grid(&[
            ("Find:", find.upcast_ref()),
            ("Replace with:", with.upcast_ref()),
        ]),
        false,
        false,
        0,
    );
    replace_page.pack_start(&regex, false, false, 0);
    replace_page.pack_start(&case_sensitive, false, false, 0);

    let template = gtk::Entry::new();
    template.set_text("[N] [C]");
    let start = gtk::SpinButton::with_range(0., 1e9, 1.);
    start.set_value(1.);
    let step = gtk::SpinButton::with_range(1., 1000., 1.);
    let digits = gtk::SpinButton::with_range(0., 10., 1.);
    digits.set_value(2.);
    let counter = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    counter.pack_start(&label("Counter from"), false, false, 0);
    counter.pack_start(&start, false, false, 0);
    counter.pack_start(&label("by"), false, false, 0);
    counter.pack_start(&step, false, false, 0);
    counter.pack_start(&label("with at least"), false, false, 0);
    counter.pack_start(&digits, false, false, 0);
    counter.pack_start(&label("digits"), false, false, 0);
    let pattern_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    pattern_page.pack_start(
        &grid(&[("New name:", template.upcast_ref())]),
        false,
        false,
        0,
    );
    let help = label(
        "[N] old name   [C] counter   [P] folder name   [D] date modified   [E] extension   [F] full old name",
    );
    help.set_line_wrap(true);
    pattern_page.pack_start(&help, false, false, 0);
    pattern_page.pack_start(&counter, false, false, 0);

    let case = gtk::ComboBoxText::new();
    for (id, text) in [
        ("lower", "lower case"),
        ("upper", "UPPER CASE"),
        ("title", "Title Case"),
        ("sentence", "Sentence case"),
    ] {
        case.append(Some(id), text);
    }
    case.set_active_id(Some("title"));
    let case_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    case_page.pack_start(&grid(&[("Change to:", case.upcast_ref())]), false, false, 0);

    let stack = gtk::Stack::new();
    stack.add_named(&replace_page, "replace");
    stack.add_named(&pattern_page, "pattern");
    stack.add_named(&case_page, "case");
    stack.set_vhomogeneous(false);
    content.pack_start(&stack, false, false, 0);
    let extension = gtk::CheckButton::with_label("Change the extension too");
    content.pack_start(&extension, false, false, 0);

    // Preview: old name, new name, problem (tooltip), whether it changes.
    let store = gtk::ListStore::new(&[
        String::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
    ]);
    let tree = gtk::TreeView::with_model(&store);
    tree.set_tooltip_column(2);
    for (title, column) in [("Name", 0), ("New name", 1)] {
        let cell = gtk::CellRendererText::new();
        cell.set_property("ellipsize", gtk::pango::EllipsizeMode::Middle);
        let view = gtk::TreeViewColumn::new();
        view.set_title(title);
        view.set_expand(true);
        view.set_resizable(true);
        gtk::prelude::TreeViewColumnExt::pack_start(&view, &cell, true);
        gtk::prelude::TreeViewColumnExt::add_attribute(&view, &cell, "text", column);
        if column == 1 {
            gtk::prelude::TreeViewColumnExt::add_attribute(&view, &cell, "foreground", 3);
        }
        tree.append_column(&view);
    }
    let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroll.set_shadow_type(gtk::ShadowType::In);
    scroll.add(&tree);
    content.pack_start(&scroll, true, true, 0);
    let status = label("");
    status.set_line_wrap(true);
    content.pack_start(&status, false, false, 0);

    let options = {
        let (mode, find, with, regex, case_sensitive) = (
            mode.clone(),
            find.clone(),
            with.clone(),
            regex.clone(),
            case_sensitive.clone(),
        );
        let (template, start, step, digits, case, extension) = (
            template.clone(),
            start.clone(),
            step.clone(),
            digits.clone(),
            case.clone(),
            extension.clone(),
        );
        move || Options {
            rule: match mode.active_id().as_deref() {
                Some("pattern") => Rule::Pattern {
                    template: template.text().into(),
                    start: start.value() as u64,
                    step: step.value() as u64,
                    digits: digits.value() as usize,
                },
                Some("case") => Rule::Case(match case.active_id().as_deref() {
                    Some("lower") => Case::Lower,
                    Some("upper") => Case::Upper,
                    Some("sentence") => Case::Sentence,
                    _ => Case::Title,
                }),
                _ => Rule::Replace {
                    find: find.text().into(),
                    with: with.text().into(),
                    regex: regex.is_active(),
                    case: case_sensitive.is_active(),
                },
            },
            extension: extension.is_active(),
        }
    };
    let items = Rc::new(items);
    let planned: Rc<std::cell::RefCell<Vec<Planned>>> = Rc::default();
    let refresh: Rc<dyn Fn()> = {
        let (items, planned, store, status, dialog) = (
            items.clone(),
            planned.clone(),
            store.clone(),
            status.clone(),
            dialog.clone(),
        );
        Rc::new(move || {
            store.clear();
            let result = rename::plan(&items, &options());
            let plan = match result {
                Ok(plan) => plan,
                Err(e) => {
                    status.set_text(&e.to_string());
                    dialog.set_response_sensitive(gtk::ResponseType::Ok, false);
                    planned.borrow_mut().clear();
                    return;
                }
            };
            let name = |p: &PathBuf| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            for p in &plan {
                let colour = if p.problem.is_some() && p.from != p.to {
                    "red"
                } else {
                    ""
                };
                store.insert_with_values(
                    None,
                    &[
                        (0, &name(&p.from)),
                        (1, &name(&p.to)),
                        (2, &p.problem.clone().unwrap_or_default()),
                        (3, &colour),
                    ],
                );
            }
            let changing = plan.iter().filter(|p| p.changes()).count();
            let blocking: Vec<&Planned> = plan
                .iter()
                .filter(|p| p.problem.is_some() && p.from != p.to)
                .collect();
            status.set_text(&match blocking.as_slice() {
                [] => format!("{changing} of {} names change.", plan.len()),
                [first, ..] => format!(
                    "{} cannot be renamed: {} ({}).",
                    blocking.len(),
                    first.problem.as_deref().unwrap_or_default(),
                    name(&first.from)
                ),
            });
            dialog
                .set_response_sensitive(gtk::ResponseType::Ok, changing > 0 && blocking.is_empty());
            *planned.borrow_mut() = plan;
        })
    };
    for entry in [&find, &with, &template] {
        let refresh = refresh.clone();
        entry.connect_changed(move |_| refresh());
    }
    for check in [&regex, &case_sensitive, &extension] {
        let refresh = refresh.clone();
        check.connect_toggled(move |_| refresh());
    }
    for spin in [&start, &step, &digits] {
        let refresh = refresh.clone();
        spin.connect_value_changed(move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        case.connect_changed(move |_| refresh());
    }
    {
        let (refresh, stack, find, template) = (
            refresh.clone(),
            stack.clone(),
            find.clone(),
            template.clone(),
        );
        mode.connect_changed(move |m| {
            let page = m.active_id().unwrap_or_else(|| "replace".into());
            stack.set_visible_child_name(&page);
            match page.as_str() {
                "replace" => find.grab_focus(),
                "pattern" => template.grab_focus(),
                _ => {}
            }
            refresh();
        });
    }
    find.set_activates_default(true);
    with.set_activates_default(true);
    template.set_activates_default(true);
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Ok {
            let changes: Vec<(PathBuf, PathBuf)> = planned
                .borrow()
                .iter()
                .filter(|p| p.changes())
                .map(|p| (p.from.clone(), p.to.clone()))
                .collect();
            if changes.is_empty() {
                return;
            }
            rename(changes);
        }
        dialog.close();
    });
    dialog.show_all();
    refresh();
    find.grab_focus();
}
