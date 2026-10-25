//! File Minnow results in the desktop's own search: GNOME Shell's overview
//! (org.gnome.Shell.SearchProvider2) and KDE's KRunner (org.kde.krunner1).
//! D-Bus starts `file-minnow search-provider` when the desktop first asks;
//! it answers from the running File Minnow when there is one, otherwise from
//! the saved index, and exits after a minute without questions.
use crate::{
    index::{Entry, Kind, Snapshot},
    query::{SearchRequest, Sort},
    service::{self, Request},
    store,
};
use anyhow::Result;
use gtk::{gio, glib, prelude::*};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

pub const BUS_NAME: &str = "org.fileminnow.FileMinnow.SearchProvider";
pub const GNOME_PATH: &str = "/org/fileminnow/FileMinnow/SearchProvider";
pub const KRUNNER_PATH: &str = "/org/fileminnow/FileMinnow/KRunner";
const GNOME_XML: &str = r#"<node><interface name="org.gnome.Shell.SearchProvider2">
<method name="GetInitialResultSet"><arg type="as" name="terms" direction="in"/><arg type="as" name="results" direction="out"/></method>
<method name="GetSubsearchResultSet"><arg type="as" name="previous_results" direction="in"/><arg type="as" name="terms" direction="in"/><arg type="as" name="results" direction="out"/></method>
<method name="GetResultMetas"><arg type="as" name="identifiers" direction="in"/><arg type="aa{sv}" name="metas" direction="out"/></method>
<method name="ActivateResult"><arg type="s" name="identifier" direction="in"/><arg type="as" name="terms" direction="in"/><arg type="u" name="timestamp" direction="in"/></method>
<method name="LaunchSearch"><arg type="as" name="terms" direction="in"/><arg type="u" name="timestamp" direction="in"/></method>
</interface></node>"#;
const KRUNNER_XML: &str = r#"<node><interface name="org.kde.krunner1">
<method name="Teardown"/>
<method name="Actions"><arg name="matches" type="a(sss)" direction="out"/></method>
<method name="SetActivationToken"><arg name="token" type="s" direction="in"/></method>
<method name="Run"><arg name="matchId" type="s" direction="in"/><arg name="actionId" type="s" direction="in"/></method>
<method name="Match"><arg name="query" type="s" direction="in"/><arg name="matches" type="a(sssida{sv})" direction="out"/></method>
</interface></node>"#;
/// Results offered for one search: GNOME shows a few, KRunner more.
const RESULTS: usize = 20;
/// Matches considered for ranking (the most recently modified).
const CANDIDATES: usize = 300;
const IDLE_EXIT: Duration = Duration::from_secs(60);

/// Answers searches from the running File Minnow or the saved index.
pub struct Searcher {
    dir: PathBuf,
    /// The saved index while no File Minnow is running, with its file time.
    saved: Mutex<Option<(SystemTime, Arc<Snapshot>)>>,
}
impl Searcher {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            saved: Mutex::new(None),
        }
    }
    /// The best matches for what was typed, best first.
    pub fn search(&self, text: &str) -> Vec<Entry> {
        let text = text.trim();
        if text.chars().count() < 2 {
            return Vec::new();
        }
        let request = SearchRequest {
            query: text.to_owned(),
            sort: Sort::Modified,
            descending: true,
            limit: CANDIDATES,
            offset: 0,
        };
        let live = store::socket_path(&self.dir, "search.sock")
            .exists()
            .then(|| service::request(&self.dir, &Request::Search(request.clone())).ok())
            .flatten()
            .and_then(|r| r.search);
        let found = match live {
            Some(response) => response.entries,
            None => self
                .saved_index()
                .and_then(|s| crate::query::search(&s, &request, None, None).ok())
                .map(|r| r.entries)
                .unwrap_or_default(),
        };
        rank(found, text)
    }
    fn saved_index(&self) -> Option<Arc<Snapshot>> {
        let modified = std::fs::metadata(self.dir.join("index.bin"))
            .and_then(|m| m.modified())
            .ok()?;
        let mut saved = self.saved.lock().unwrap();
        if saved.as_ref().is_none_or(|(time, _)| *time != modified) {
            *saved = Some((modified, Arc::new(store::load(&self.dir).ok()?)));
        }
        saved.as_ref().map(|(_, s)| s.clone())
    }
}
/// Orders matches for a launcher: names that start with the first word
/// first, then visible files before hidden ones; otherwise the most
/// recently modified first (the order they arrive in).
fn rank(mut entries: Vec<Entry>, text: &str) -> Vec<Entry> {
    let first = crate::fold::fold(text.split_whitespace().next().unwrap_or("").as_bytes());
    entries.sort_by_key(|e| {
        let name = crate::fold::fold(e.name());
        let hidden = e
            .path
            .split(|b| *b == b'/')
            .any(|part| part.starts_with(b"."));
        (!name.starts_with(&first), hidden)
    });
    entries.truncate(RESULTS);
    entries
}
/// Result identifiers are file URIs: D-Bus strings must be text, paths
/// need not be.
fn uri(entry: &Entry) -> String {
    gio::File::for_path(entry.path_buf()).uri().to_string()
}
fn icon(entry: &Entry) -> gio::Icon {
    match entry.kind {
        Kind::Folder => gio::ThemedIcon::new("folder").upcast(),
        _ => gio::content_type_get_icon(&crate::file_ops::guess_type(&entry.path_buf()).0),
    }
}
fn icon_name(entry: &Entry) -> String {
    icon(entry)
        .downcast::<gio::ThemedIcon>()
        .ok()
        .and_then(|i| i.names().into_iter().next())
        .map_or_else(|| "text-x-generic".to_owned(), |n| n.to_string())
}
/// The folder an entry is in, with the home folder shortened to `~`.
fn location(entry: &Entry) -> String {
    let parent = entry
        .path_buf()
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && parent.starts_with(&home) => {
            format!("~{}", &parent[home.len()..])
        }
        _ => parent,
    }
}

struct Provider {
    searcher: Searcher,
    /// Entries of the latest results, by identifier, for metas and Run.
    shown: Mutex<Vec<(String, Entry)>>,
    /// XDG activation token from KRunner, for the next launch.
    token: Mutex<Option<String>>,
    /// Seconds since the epoch of the latest question, for exiting when idle.
    active: AtomicU64,
    data_dir: Option<PathBuf>,
}
impl Provider {
    fn touch(&self) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.active.store(now, Ordering::Relaxed);
    }
    fn search(&self, text: &str) -> Vec<(String, Entry)> {
        let found: Vec<(String, Entry)> = self
            .searcher
            .search(text)
            .into_iter()
            .map(|e| (uri(&e), e))
            .collect();
        let mut shown = self.shown.lock().unwrap();
        for (id, entry) in &found {
            if !shown.iter().any(|(s, _)| s == id) {
                shown.push((id.clone(), entry.clone()));
            }
        }
        let excess = shown.len().saturating_sub(500);
        shown.drain(..excess);
        found
    }
    fn entry(&self, id: &str) -> Option<Entry> {
        self.shown
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| s == id)
            .map(|(_, e)| e.clone())
    }
    fn context(&self) -> gio::AppLaunchContext {
        let context = gio::AppLaunchContext::new();
        if let Some(token) = self.token.lock().unwrap().take() {
            context.setenv("XDG_ACTIVATION_TOKEN", token);
        }
        context
    }
    /// Opens the result with its default application (a folder in the file manager).
    fn open(&self, id: &str) {
        let _ = gio::AppInfo::launch_default_for_uri(id, Some(&self.context()));
    }
    /// Shows the result selected in its folder.
    fn show_in_folder(&self, id: &str) {
        let shown = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
            .and_then(|bus| {
                bus.call_sync(
                    Some("org.freedesktop.FileManager1"),
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1",
                    "ShowItems",
                    Some(&(vec![id.to_owned()], String::new()).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    3000,
                    None::<&gio::Cancellable>,
                )
            })
            .is_ok();
        if !shown && let Some(parent) = gio::File::for_uri(id).parent() {
            self.open(&parent.uri());
        }
    }
    /// Opens File Minnow with the search.
    fn launch_search(&self, text: &str) {
        let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("file-minnow"));
        let mut command = std::process::Command::new(program);
        if let Some(dir) = &self.data_dir {
            command.arg("--data-dir").arg(dir);
        }
        let _ = command
            .arg("show")
            .arg(text)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    fn gnome(&self, method: &str, params: &glib::Variant) -> Option<glib::Variant> {
        let terms = |v: Option<glib::Variant>| -> String {
            v.and_then(|v| v.get::<Vec<String>>())
                .unwrap_or_default()
                .join(" ")
        };
        match method {
            "GetInitialResultSet" => {
                let ids: Vec<String> = self
                    .search(&terms(params.try_child_value(0)))
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                Some((ids,).to_variant())
            }
            "GetSubsearchResultSet" => {
                let ids: Vec<String> = self
                    .search(&terms(params.try_child_value(1)))
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                Some((ids,).to_variant())
            }
            "GetResultMetas" => {
                let ids = params
                    .try_child_value(0)
                    .and_then(|v| v.get::<Vec<String>>())
                    .unwrap_or_default();
                let metas = ids.iter().filter_map(|id| {
                    let entry = self.entry(id)?;
                    let meta = glib::VariantDict::new(None);
                    meta.insert("id", id);
                    meta.insert("name", entry.display_name());
                    meta.insert("description", location(&entry));
                    meta.insert_value("icon", &icon(&entry).serialize()?);
                    Some(meta.end())
                });
                let array =
                    glib::Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, metas);
                Some(glib::Variant::tuple_from_iter([array]))
            }
            "ActivateResult" => {
                if let Some(id) = params.try_child_value(0).and_then(|v| v.get::<String>()) {
                    self.open(&id);
                }
                Some(().to_variant())
            }
            "LaunchSearch" => {
                self.launch_search(&terms(params.try_child_value(0)));
                Some(().to_variant())
            }
            _ => None,
        }
    }
    fn krunner(&self, method: &str, params: &glib::Variant) -> Option<glib::Variant> {
        match method {
            "Match" => {
                let text = params
                    .try_child_value(0)
                    .and_then(|v| v.get::<String>())
                    .unwrap_or_default();
                let first =
                    crate::fold::fold(text.split_whitespace().next().unwrap_or("").as_bytes());
                let found = self.search(&text);
                let count = found.len().max(1) as f64;
                let matches = found.into_iter().enumerate().map(|(i, (id, entry))| {
                    let properties = glib::VariantDict::new(None);
                    properties.insert("subtext", location(&entry));
                    properties.insert("urls", vec![id.clone()]);
                    properties.insert("category", "Files");
                    // An exact name is a better match than a partial one.
                    let name = crate::fold::fold(entry.name());
                    let kind: i32 = if name == first { 100 } else { 30 };
                    let relevance = 0.9 - 0.4 * (i as f64 / count);
                    glib::Variant::tuple_from_iter([
                        id.to_variant(),
                        entry.display_name().to_variant(),
                        icon_name(&entry).to_variant(),
                        kind.to_variant(),
                        relevance.to_variant(),
                        properties.end(),
                    ])
                });
                let array = glib::Variant::array_from_iter_with_type(
                    glib::VariantTy::new("(sssida{sv})").unwrap(),
                    matches,
                );
                Some(glib::Variant::tuple_from_iter([array]))
            }
            "Actions" => {
                let actions = vec![(
                    "show-in-folder".to_owned(),
                    "Open Containing Folder".to_owned(),
                    "document-open-folder".to_owned(),
                )];
                Some((actions,).to_variant())
            }
            "Run" => {
                let (id, action) = params.get::<(String, String)>()?;
                if action == "show-in-folder" {
                    self.show_in_folder(&id);
                } else {
                    self.open(&id);
                }
                Some(().to_variant())
            }
            "SetActivationToken" => {
                *self.token.lock().unwrap() =
                    params.try_child_value(0).and_then(|v| v.get::<String>());
                Some(().to_variant())
            }
            "Teardown" => {
                self.shown.lock().unwrap().clear();
                Some(().to_variant())
            }
            _ => None,
        }
    }
}

fn register(connection: &gio::DBusConnection, provider: &Arc<Provider>) -> Result<()> {
    for (path, xml, gnome) in [
        (GNOME_PATH, GNOME_XML, true),
        (KRUNNER_PATH, KRUNNER_XML, false),
    ] {
        let info = gio::DBusNodeInfo::for_xml(xml)?;
        let interface = info.interfaces().first().expect("one interface");
        let provider = provider.clone();
        connection.register_object(
            path,
            interface,
            move |_, _, _, _, method, params, invocation| {
                provider.touch();
                let reply = if gnome {
                    provider.gnome(method, &params)
                } else {
                    provider.krunner(method, &params)
                };
                match reply {
                    Some(value) if value.type_() == glib::VariantTy::UNIT => {
                        invocation.return_value(None)
                    }
                    Some(value) => invocation.return_value(Some(&value)),
                    None => invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        &format!("Unknown method {method}"),
                    ),
                }
            },
            |_, _, _, _, _| glib::Variant::from(()),
            |_, _, _, _, _, _| false,
        )?;
    }
    Ok(())
}
/// Serves both interfaces until a minute passes without a question, or the
/// bus name is taken by another copy.
pub fn serve(dir: &Path, data_dir: Option<PathBuf>) -> Result<()> {
    let provider = Arc::new(Provider {
        searcher: Searcher::new(dir),
        shown: Mutex::new(Vec::new()),
        token: Mutex::new(None),
        active: AtomicU64::new(0),
        data_dir,
    });
    provider.touch();
    let main = glib::MainLoop::new(None, false);
    let registered = provider.clone();
    let failed = main.clone();
    let lost = main.clone();
    // The name is owned with the bus's own connection: when the session
    // ends, losing the name (with no connection left) ends the provider.
    let _owner = gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::DO_NOT_QUEUE,
        move |connection, _| {
            if let Err(e) = register(&connection, &registered) {
                eprintln!("Cannot offer search results: {e}");
                failed.quit();
            }
        },
        |_, _| {},
        move |_, _| lost.quit(),
    );
    let idle = main.clone();
    let watched = provider.clone();
    glib::timeout_add_seconds(15, move || {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now.saturating_sub(watched.active.load(Ordering::Relaxed)) >= IDLE_EXIT.as_secs() {
            idle.quit();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
    main.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranking_prefers_names_starting_with_the_word() {
        let entry = |path: &str| Entry::new(path.as_bytes().to_vec(), Kind::File, 1, 0);
        let ranked = rank(
            vec![
                entry("/h/old-report.txt"),
                entry("/h/.cache/report.tmp"),
                entry("/h/Report 2026.pdf"),
                entry("/h/x/reportage.md"),
            ],
            "report",
        );
        let names: Vec<String> = ranked.iter().map(Entry::display_name).collect();
        assert_eq!(
            names,
            [
                "Report 2026.pdf",
                "reportage.md",
                "report.tmp",
                "old-report.txt"
            ]
        );
    }
}
