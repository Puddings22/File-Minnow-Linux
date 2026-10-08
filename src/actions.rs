//! The file manager's own right-click items: custom actions and scripts.
//!
//! Linux has no shared context-menu API like the Windows shell; each file
//! manager reads its own plain files. File Minnow reads the formats of the
//! desktop's default file manager (all of them when it is not recognised):
//!
//! - Nemo (Cinnamon): `*.nemo_action` in `nemo/actions` under the XDG data
//!   directories, and scripts in `nemo/scripts`.
//! - Thunar (Xfce): custom actions in `Thunar/uca.xml` (XDG config).
//! - Dolphin (KDE): service menus in `kio/servicemenus` and the older
//!   `kservices5/ServiceMenus` (XDG data).
//! - Nautilus (GNOME) and Caja (MATE): scripts in `nautilus/scripts` and
//!   `caja/scripts`. Their actions are compiled extensions, which only those
//!   file managers can load.
//!
//! Nemo action keys: Active, Name (localized, with %N/%f), Comment, Exec
//! (%U %F %P %p %f %N %e %X %%, and `<program ...>` relative to the action
//! file), Selection, Extensions, Mimetypes, Separator, Quote, Dependencies,
//! Terminal, and the `dbus <name>` condition. Actions with other conditions
//! (`desktop`, `removable`, `exec`, `gsettings`) or the %D device token are
//! left out: they depend on Nemo's own state.
use anyhow::{Context, Result, bail};
use regex::Regex;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::LazyLock,
};

/// One selected entry as an action sees it.
pub struct Item {
    pub path: PathBuf,
    pub folder: bool,
    /// MIME type (`inode/directory` for folders).
    pub mime: String,
}

/// The desktop's file manager, which decides the action formats read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Nemo,
    Nautilus,
    Caja,
    Thunar,
    Dolphin,
    Other,
}
impl Manager {
    /// From the desktop file id of the default folder handler
    /// (`nemo.desktop`, `org.gnome.Nautilus.desktop`, `thunar.desktop`...).
    pub fn from_desktop_id(id: Option<&str>) -> Self {
        let id = id.unwrap_or_default().to_ascii_lowercase();
        [
            ("nemo", Self::Nemo),
            ("nautilus", Self::Nautilus),
            ("caja", Self::Caja),
            ("thunar", Self::Thunar),
            ("dolphin", Self::Dolphin),
        ]
        .into_iter()
        .find(|(name, _)| id.contains(name))
        .map_or(Self::Other, |(_, manager)| manager)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Syntax {
    Nemo,
    Thunar,
    Kde,
}

#[derive(Clone, Debug)]
enum Filter {
    /// Extensions (with any/dir/nodirs/none) or MIME types.
    Nemo {
        extensions: Vec<String>,
        mimetypes: Vec<String>,
    },
    /// Name patterns, plus which kinds of entries qualify.
    Thunar {
        patterns: Vec<String>,
        directories: bool,
        audio: bool,
        image: bool,
        text: bool,
        video: bool,
        other: bool,
    },
    /// MIME types, with all/all (everything) and all/allfiles (files).
    Kde { mimetypes: Vec<String> },
}

#[derive(Clone, Debug)]
pub struct Action {
    pub name: String,
    pub comment: String,
    /// Submenu label (Thunar `submenu`, KDE `X-KDE-Submenu`).
    pub submenu: Option<String>,
    exec: String,
    dir: PathBuf,
    syntax: Syntax,
    /// Allowed number of selected entries.
    min: usize,
    max: usize,
    filter: Filter,
    separator: String,
    quote: Option<char>,
    dependencies: Vec<String>,
    dbus: Vec<String>,
    pub terminal: bool,
}

fn home_dir(variable: &str, fallback: &str) -> Option<PathBuf> {
    match std::env::var_os(variable).filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)),
    }
}
fn system_dirs(variable: &str, fallback: &str) -> Vec<PathBuf> {
    std::env::var(variable)
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| fallback.into())
        .split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect()
}
/// Data directories in XDG order: the user's first.
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = home_dir("XDG_DATA_HOME", ".local/share")
        .into_iter()
        .collect();
    dirs.extend(system_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
    dirs
}
/// Configuration directories in XDG order: the user's first.
fn config_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = home_dir("XDG_CONFIG_HOME", ".config").into_iter().collect();
    dirs.extend(system_dirs("XDG_CONFIG_DIRS", "/etc/xdg"));
    dirs
}

/// Locale names to try for `Key[locale]`, most specific first.
fn locales() -> Vec<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    let base = raw.split(['.', '@']).next().unwrap_or("").to_owned();
    let mut out = Vec::new();
    if !base.is_empty() && base != "C" && base != "POSIX" {
        out.push(base.clone());
        if let Some((lang, _)) = base.split_once('_') {
            out.push(lang.to_owned());
        }
    }
    out
}

type Group = BTreeMap<String, String>;

/// Reads the groups of a desktop-entry style key file.
fn key_groups(text: &str) -> BTreeMap<String, Group> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].to_owned();
            groups.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        let (Some(group), Some((key, value))) = (&current, line.split_once('=')) else {
            continue;
        };
        let mut unescaped = String::with_capacity(value.len());
        let mut chars = value.trim().chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                unescaped.push(c);
                continue;
            }
            match chars.next() {
                Some('s') => unescaped.push(' '),
                Some('n') => unescaped.push('\n'),
                Some('t') => unescaped.push('\t'),
                Some('r') => unescaped.push('\r'),
                Some(other) => {
                    // `\;` stays escaped for list splitting.
                    if other == ';' {
                        unescaped.push('\\');
                    }
                    unescaped.push(other)
                }
                None => unescaped.push('\\'),
            }
        }
        groups
            .entry(group.clone())
            .or_default()
            .insert(key.trim().to_owned(), unescaped);
    }
    groups
}

fn localized(keys: &Group, key: &str) -> Option<String> {
    locales()
        .iter()
        .find_map(|l| keys.get(&format!("{key}[{l}]")))
        .or_else(|| keys.get(key))
        .cloned()
}

fn list(value: Option<&String>) -> Vec<String> {
    value
        .map(|v| {
            v.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn truthy(value: Option<&String>) -> bool {
    value.is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Case-insensitive `*` / `?` wildcard match, as Thunar uses for patterns.
fn wildcard(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&name[n])) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|b| *b == b'*')
}

fn mime_matches(pattern: &str, mime: &str) -> bool {
    pattern == "*"
        || pattern == "*/*"
        || pattern.eq_ignore_ascii_case(mime)
        || pattern.strip_suffix("/*").is_some_and(|major| {
            mime.split_once('/')
                .is_some_and(|(m, _)| m.eq_ignore_ascii_case(major))
        })
}

impl Action {
    fn base(syntax: Syntax, name: String, exec: String, dir: &Path, filter: Filter) -> Self {
        Self {
            name,
            comment: String::new(),
            submenu: None,
            exec,
            dir: dir.to_path_buf(),
            syntax,
            min: 1,
            max: usize::MAX,
            filter,
            separator: " ".into(),
            quote: None,
            dependencies: Vec::new(),
            dbus: Vec::new(),
            terminal: false,
        }
    }

    /// Parses a `.nemo_action` file. None for inactive, malformed or
    /// unsupported actions.
    pub fn parse_nemo(text: &str, dir: &Path) -> Option<Self> {
        let keys = key_groups(text).remove("Nemo Action")?;
        if keys
            .get("Active")
            .is_some_and(|v| v.eq_ignore_ascii_case("false"))
        {
            return None;
        }
        let exec = keys.get("Exec")?.clone();
        if exec.contains("%D") {
            return None;
        }
        let mut dbus = Vec::new();
        for condition in list(keys.get("Conditions")) {
            match condition.split_once(' ') {
                Some(("dbus", name)) => dbus.push(name.trim().to_owned()),
                _ => return None,
            }
        }
        let (min, max) = match keys
            .get("Selection")
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("s") | None => (1, 1),
            Some("m") => (2, usize::MAX),
            Some("any") | Some("notnone") => (1, usize::MAX),
            Some("none") => return None,
            Some(n) => {
                let n = n.parse().ok().filter(|n| *n > 0)?;
                (n, n)
            }
        };
        let extensions = list(keys.get("Extensions"));
        let mimetypes = list(keys.get("Mimetypes"));
        if extensions.is_empty() && mimetypes.is_empty() {
            return None;
        }
        let quote = match keys.get("Quote").map(|q| q.to_ascii_lowercase()).as_deref() {
            Some("double") => Some('"'),
            Some("single") => Some('\''),
            Some("backtick") => Some('`'),
            _ => None,
        };
        let mut action = Self::base(
            Syntax::Nemo,
            localized(&keys, "Name")?,
            exec,
            dir,
            Filter::Nemo {
                extensions,
                mimetypes,
            },
        );
        action.comment = localized(&keys, "Comment").unwrap_or_default();
        action.min = min;
        action.max = max;
        action.separator = keys.get("Separator").cloned().unwrap_or_else(|| " ".into());
        action.quote = quote;
        action.dependencies = list(keys.get("Dependencies"));
        action.dbus = dbus;
        action.terminal = truthy(keys.get("Terminal"));
        Some(action)
    }

    /// Parses Thunar's `uca.xml` (a flat list of `<action>` elements).
    pub fn parse_thunar(xml: &str) -> Vec<Self> {
        static ACTION: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?s)<action\b[^>]*>(.*?)</action>").unwrap());
        static ELEMENT: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?s)<([A-Za-z-]+)([^>]*?)(?:/>|>([^<]*)</[A-Za-z-]+>)"#).unwrap()
        });
        static LANG: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"xml:lang\s*=\s*["']([^"']*)["']"#).unwrap());
        let locales = locales();
        let mut actions = Vec::new();
        for block in ACTION.captures_iter(xml) {
            // element -> (language or "", text)
            let mut values: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
            for element in ELEMENT.captures_iter(&block[1]) {
                let lang = LANG
                    .captures(&element[2])
                    .map(|c| c[1].to_owned())
                    .unwrap_or_default();
                let text = element.get(3).map_or("", |m| m.as_str());
                values
                    .entry(element[1].to_owned())
                    .or_default()
                    .push((lang, xml_text(text.trim())));
            }
            let text = |key: &str| -> Option<String> {
                let all = values.get(key)?;
                locales
                    .iter()
                    .find_map(|l| all.iter().find(|(lang, _)| lang == l))
                    .or_else(|| all.iter().find(|(lang, _)| lang.is_empty()))
                    .map(|(_, value)| value.clone())
            };
            let flag = |key: &str| values.contains_key(key);
            let (Some(name), Some(command)) = (text("name"), text("command")) else {
                continue;
            };
            if name.is_empty() || command.is_empty() {
                continue;
            }
            let patterns = text("patterns").unwrap_or_else(|| "*".into());
            let mut action = Self::base(
                Syntax::Thunar,
                name.replace('_', "__"),
                command,
                Path::new("/"),
                Filter::Thunar {
                    patterns: list(Some(&patterns)),
                    directories: flag("directories"),
                    audio: flag("audio-files"),
                    image: flag("image-files"),
                    text: flag("text-files"),
                    video: flag("video-files"),
                    other: flag("other-files"),
                },
            );
            action.comment = text("description").unwrap_or_default();
            action.submenu = text("submenu").filter(|s| !s.is_empty());
            if let Some(range) = text("range").filter(|r| !r.is_empty() && r != "*") {
                let (low, high) = range.split_once('-').unwrap_or((&range, &range));
                action.min = low.trim().parse().unwrap_or(1).max(1);
                action.max = high
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .map_or(usize::MAX, |n| n as usize);
            }
            actions.push(action);
        }
        actions
    }

    /// Parses a KDE service menu: one action per `[Desktop Action ...]`.
    pub fn parse_kde(text: &str, dir: &Path) -> Vec<Self> {
        let mut groups = key_groups(text);
        let Some(entry) = groups.remove("Desktop Entry") else {
            return Vec::new();
        };
        let mimetypes = list(entry.get("MimeType"));
        let protocols = list(entry.get("X-KDE-Protocols"));
        if mimetypes.is_empty()
            || truthy(entry.get("Hidden"))
            || entry.contains_key("X-KDE-ShowIfRunning")
            || entry.contains_key("X-KDE-ShowIfDBusCall")
            || (!protocols.is_empty() && !protocols.iter().any(|p| p == "file"))
        {
            return Vec::new();
        }
        let count = |key: &str| entry.get(key).and_then(|v| v.trim().parse::<usize>().ok());
        let submenu = localized(&entry, "X-KDE-Submenu").filter(|s| !s.is_empty());
        let mut actions = Vec::new();
        for id in list(entry.get("Actions")) {
            let Some(keys) = groups.get(&format!("Desktop Action {id}")) else {
                continue;
            };
            let (Some(name), Some(exec)) = (localized(keys, "Name"), keys.get("Exec")) else {
                continue;
            };
            if id == "_SEPARATOR_" || truthy(keys.get("NoDisplay")) {
                continue;
            }
            let mut action = Self::base(
                Syntax::Kde,
                name,
                exec.clone(),
                dir,
                Filter::Kde {
                    mimetypes: mimetypes.clone(),
                },
            );
            action.submenu = submenu.clone();
            action.min = count("X-KDE-MinNumberOfUrls").unwrap_or(1).max(1);
            action.max = count("X-KDE-MaxNumberOfUrls").unwrap_or(usize::MAX);
            if let Some(n) = count("X-KDE-RequiredNumberOfUrls") {
                (action.min, action.max) = (n, n);
            }
            actions.push(action);
        }
        actions
    }

    /// Whether the action is offered for this selection. `bus_has` tells
    /// whether a D-Bus name currently has an owner.
    pub fn applies(&self, items: &[Item], bus_has: &dyn Fn(&str) -> bool) -> bool {
        !items.is_empty()
            && (self.min..=self.max).contains(&items.len())
            && items.iter().all(|item| self.matches(item))
            && self.dependencies.iter().all(|d| self.available(d))
            && self.dbus.iter().all(|name| bus_has(name))
    }

    fn matches(&self, item: &Item) -> bool {
        let name = item
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        match &self.filter {
            Filter::Nemo {
                extensions,
                mimetypes,
            } => {
                extensions
                    .iter()
                    .any(|ext| match ext.to_ascii_lowercase().as_str() {
                        "any" => true,
                        "dir" => item.folder,
                        "nodirs" => !item.folder,
                        "none" => !item.folder && !name.trim_start_matches('.').contains('.'),
                        ext => !item.folder && name.ends_with(&format!(".{ext}")),
                    })
                    || mimetypes.iter().any(|m| mime_matches(m, &item.mime))
            }
            Filter::Thunar {
                patterns,
                directories,
                audio,
                image,
                text,
                video,
                other,
            } => {
                let kind = if item.folder {
                    *directories
                } else {
                    match item.mime.split_once('/').map(|(major, _)| major) {
                        Some("audio") => *audio,
                        Some("image") => *image,
                        Some("text") => *text,
                        Some("video") => *video,
                        _ => *other,
                    }
                };
                kind && patterns
                    .iter()
                    .any(|p| wildcard(p.as_bytes(), name.as_bytes()))
            }
            Filter::Kde { mimetypes } => mimetypes.iter().any(|m| match m.as_str() {
                "all/all" => true,
                "all/allfiles" => !item.folder,
                m => mime_matches(m, &item.mime),
            }),
        }
    }

    fn available(&self, program: &str) -> bool {
        let program = program.trim_start_matches('<').trim_end_matches('>');
        if program.contains('/') {
            let path = Path::new(program);
            return if path.is_absolute() {
                path.exists()
            } else {
                self.dir.join(path).exists()
            };
        }
        self.dir.join(program).exists() || find_program(program).is_some()
    }

    /// The label, with %N/%f replaced by the first item's name (Nemo).
    /// Underscores are mnemonics.
    pub fn label(&self, items: &[Item]) -> String {
        if self.syntax != Syntax::Nemo {
            return self.name.clone();
        }
        let first = items
            .first()
            .and_then(|i| i.path.file_name())
            .map(|n| n.to_string_lossy().replace('_', "__"))
            .unwrap_or_default();
        self.name.replace("%N", &first).replace("%f", &first)
    }

    /// The command lines to run, split into arguments. KDE service menus
    /// whose Exec takes one file (%f, %u) run once per selected entry.
    pub fn commands(&self, items: &[Item], window: Option<u32>) -> Result<Vec<Vec<OsString>>> {
        let commands = match self.syntax {
            Syntax::Nemo | Syntax::Thunar => vec![self.substituted(items, window)?],
            Syntax::Kde => {
                let single = !self.exec.contains("%F") && !self.exec.contains("%U");
                if single && items.len() > 1 {
                    items
                        .iter()
                        .map(|item| self.desktop_entry(std::slice::from_ref(item)))
                        .collect::<Result<_>>()?
                } else {
                    vec![self.desktop_entry(items)?]
                }
            }
        };
        if commands.iter().any(Vec::is_empty) {
            bail!("{}: empty command", self.name);
        }
        Ok(commands)
    }

    /// Nemo and Thunar: tokens are replaced in the command text, which is
    /// then split like a shell would. Values are escaped for where the token
    /// stands: inside quotes that the command itself opened, or bare (then
    /// Nemo's Quote= applies). Nemo always adds Quote= quotes, which breaks
    /// `"%F"` for names with spaces.
    fn substituted(&self, items: &[Item], window: Option<u32>) -> Result<Vec<OsString>> {
        let context = std::cell::Cell::new(None::<u8>);
        let quote = |path: &[u8]| -> Vec<u8> {
            match (context.get(), self.quote) {
                (Some(b'\''), _) => path
                    .split(|b| *b == b'\'')
                    .collect::<Vec<_>>()
                    .join(&b"'\\''"[..]),
                (Some(_), _) => {
                    let mut out = Vec::with_capacity(path.len());
                    for &b in path {
                        if b"\"\\$`".contains(&b) {
                            out.push(b'\\');
                        }
                        out.push(b);
                    }
                    out
                }
                (None, Some(q)) if !path.contains(&(q as u8)) => {
                    let mut out = vec![q as u8];
                    out.extend_from_slice(path);
                    out.push(q as u8);
                    out
                }
                (None, _) => escape(path),
            }
        };
        let joined = |parts: Vec<Vec<u8>>| -> Vec<u8> {
            parts
                .iter()
                .map(|p| quote(p))
                .collect::<Vec<_>>()
                .join(self.separator.as_bytes())
        };
        let first = items.first().context("Nothing selected")?;
        let parent = |p: &Path| p.parent().unwrap_or(Path::new("/")).to_path_buf();
        let bytes = |p: &Path| p.as_os_str().as_bytes().to_vec();
        let name_of = |p: &Path| {
            p.file_name()
                .map(|n| n.as_bytes().to_vec())
                .unwrap_or_default()
        };
        let paths = || items.iter().map(|i| bytes(&i.path)).collect::<Vec<_>>();
        let uris = || {
            items
                .iter()
                .map(|i| file_uri(&i.path).into_bytes())
                .collect::<Vec<_>>()
        };
        let mut exec = self.exec.trim().to_owned();
        let mut line = Vec::new();
        if self.syntax == Syntax::Nemo && exec.starts_with('<') && exec.ends_with('>') {
            exec = exec[1..exec.len() - 1].to_owned();
            let (program, rest) = exec.split_once(' ').unwrap_or((exec.as_str(), ""));
            line.extend(escape(self.dir.join(program).as_os_str().as_bytes()));
            exec = format!(" {rest}");
        }
        let mut chars = exec.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                match (c, context.get()) {
                    ('"', None) => context.set(Some(b'"')),
                    ('"', Some(b'"')) => context.set(None),
                    ('\'', None) => context.set(Some(b'\'')),
                    ('\'', Some(b'\'')) => context.set(None),
                    ('\\', quoted) if quoted != Some(b'\'') => {
                        line.push(b'\\');
                        if let Some(next) = chars.next() {
                            let mut buffer = [0; 4];
                            line.extend_from_slice(next.encode_utf8(&mut buffer).as_bytes());
                        }
                        continue;
                    }
                    _ => {}
                }
                let mut buffer = [0; 4];
                line.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                continue;
            }
            let token = chars.next();
            let value = match (self.syntax, token) {
                (_, Some('%')) => Some(b"%".to_vec()),
                (_, Some('F')) => Some(joined(paths())),
                (_, Some('U')) => Some(joined(uris())),
                (Syntax::Nemo, Some('P')) => Some(quote(&bytes(&parent(&first.path)))),
                (Syntax::Nemo, Some('p')) => Some(quote(&name_of(&parent(&first.path)))),
                (Syntax::Nemo, Some('f') | Some('N')) => Some(quote(&name_of(&first.path))),
                (Syntax::Nemo, Some('e')) => {
                    let name = name_of(&first.path);
                    let stem = match name.iter().rposition(|b| *b == b'.') {
                        Some(i) if i > 0 => name[..i].to_vec(),
                        _ => name,
                    };
                    Some(quote(&stem))
                }
                (Syntax::Nemo, Some('X')) => Some(window.unwrap_or(0).to_string().into_bytes()),
                (Syntax::Thunar, Some('f')) => Some(quote(&bytes(&first.path))),
                (Syntax::Thunar, Some('u')) => Some(quote(file_uri(&first.path).as_bytes())),
                (Syntax::Thunar, Some('d')) => Some(quote(&bytes(&parent(&first.path)))),
                (Syntax::Thunar, Some('D')) => Some(joined(
                    items.iter().map(|i| bytes(&parent(&i.path))).collect(),
                )),
                (Syntax::Thunar, Some('n')) => Some(quote(&name_of(&first.path))),
                (Syntax::Thunar, Some('N')) => {
                    Some(joined(items.iter().map(|i| name_of(&i.path)).collect()))
                }
                _ => None,
            };
            match (value, token) {
                (Some(value), _) => line.extend(value),
                (None, Some(other)) => {
                    line.push(b'%');
                    let mut buffer = [0; 4];
                    line.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
                }
                (None, None) => line.push(b'%'),
            }
        }
        gtk::glib::shell_parse_argv(OsString::from_vec(line))
            .map_err(|e| anyhow::anyhow!("{}: {e}", self.name))
    }

    /// KDE (Desktop Entry Specification): the command is split first, then
    /// field codes are expanded; %F and %U become one argument per entry.
    fn desktop_entry(&self, items: &[Item]) -> Result<Vec<OsString>> {
        let first = items.first().context("Nothing selected")?;
        let args = gtk::glib::shell_parse_argv(&self.exec)
            .map_err(|e| anyhow::anyhow!("{}: {e}", self.name))?;
        let mut argv = Vec::new();
        for arg in args {
            let arg = arg.as_bytes();
            match arg {
                b"%F" => argv.extend(items.iter().map(|i| i.path.clone().into_os_string())),
                b"%U" => argv.extend(items.iter().map(|i| OsString::from(file_uri(&i.path)))),
                // Icon, desktop-file and deprecated codes expand to nothing.
                b"%i" | b"%k" | b"%d" | b"%D" | b"%n" | b"%N" | b"%v" | b"%m" => {}
                _ => {
                    let mut out = Vec::with_capacity(arg.len());
                    let mut bytes = arg.iter();
                    while let Some(&b) = bytes.next() {
                        if b != b'%' {
                            out.push(b);
                            continue;
                        }
                        match bytes.next() {
                            Some(b'f') => out.extend_from_slice(first.path.as_os_str().as_bytes()),
                            Some(b'u') => out.extend_from_slice(file_uri(&first.path).as_bytes()),
                            Some(b'c') => out.extend_from_slice(self.name.as_bytes()),
                            Some(b'%') => out.push(b'%'),
                            Some(b'F') => out.extend(
                                items
                                    .iter()
                                    .map(|i| i.path.as_os_str().as_bytes().to_vec())
                                    .collect::<Vec<_>>()
                                    .join(&b' '),
                            ),
                            Some(b'U') => out.extend(
                                items
                                    .iter()
                                    .map(|i| file_uri(&i.path))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                                    .into_bytes(),
                            ),
                            // Icon, desktop-file and deprecated codes expand to nothing;
                            // anything else (a stray %) is kept as written.
                            Some(b'i' | b'k' | b'd' | b'D' | b'n' | b'N' | b'v' | b'm') => {}
                            Some(&other) => out.extend_from_slice(&[b'%', other]),
                            None => out.push(b'%'),
                        }
                    }
                    argv.push(OsString::from_vec(out));
                }
            }
        }
        Ok(argv)
    }
}

/// Text content of an XML element: entities decoded.
fn xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else {
            break;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Backslash-escapes characters the shell-style parser would interpret.
fn escape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    for &b in bytes {
        if b" \t\n'\"\\`$&|;<>()*?[]#~!{}".contains(&b) {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// `file://` URI with the path's raw bytes percent-encoded.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

pub fn find_program(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}

/// Reads `*.<extension>` files from `folder` under each directory. A file
/// in an earlier directory hides one of the same name in a later one, even
/// when it yields no action (that is how a user disables a system action).
fn layered(
    dirs: &[PathBuf],
    folder: &str,
    extension: &str,
    parse: impl Fn(&Path, &str, bool) -> Vec<Action>,
) -> Vec<Action> {
    let mut found: BTreeMap<OsString, Vec<Action>> = BTreeMap::new();
    for (i, dir) in dirs.iter().enumerate() {
        let dir = dir.join(folder);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != extension)
                || found.contains_key(&entry.file_name())
            {
                continue;
            }
            let actions = std::fs::read_to_string(&path)
                .map(|text| parse(&path, &text, i == 0))
                .unwrap_or_default();
            found.insert(entry.file_name(), actions);
        }
    }
    found.into_values().flatten().collect()
}

/// The custom actions for this file manager (every format when unknown),
/// ordered by file name within each format.
pub fn load(manager: Manager) -> Vec<Action> {
    let all = manager == Manager::Other;
    let mut actions = Vec::new();
    if all || manager == Manager::Nemo {
        actions.extend(layered(
            &data_dirs(),
            "nemo/actions",
            "nemo_action",
            |path, text, _| {
                Action::parse_nemo(text, path.parent().unwrap_or(Path::new("/")))
                    .into_iter()
                    .collect()
            },
        ));
    }
    if all || manager == Manager::Thunar {
        // Thunar reads the user's file, or the system default when absent.
        if let Some(text) = config_dirs()
            .iter()
            .find_map(|d| std::fs::read_to_string(d.join("Thunar/uca.xml")).ok())
        {
            actions.extend(Action::parse_thunar(&text));
        }
    }
    if all || manager == Manager::Dolphin {
        let dirs = data_dirs();
        for folder in ["kio/servicemenus", "kservices5/ServiceMenus"] {
            actions.extend(layered(&dirs, folder, "desktop", |path, text, user| {
                // Like KDE, a user's service menu runs only when it is executable.
                use std::os::unix::fs::PermissionsExt;
                let executable = path
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
                if user && folder == "kio/servicemenus" && !executable {
                    return Vec::new();
                }
                Action::parse_kde(text, path.parent().unwrap_or(Path::new("/")))
            }));
        }
    }
    actions
}

/// A file-manager script: run with the selected paths as arguments and the
/// file manager's environment variables.
pub struct Script {
    pub name: String,
    pub path: PathBuf,
    /// `NEMO`, `NAUTILUS` or `CAJA`, for the `*_SCRIPT_*` variables.
    pub manager: &'static str,
}

pub fn scripts(manager: Manager) -> Vec<Script> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let data = home_dir("XDG_DATA_HOME", ".local/share");
    let config = home_dir("XDG_CONFIG_HOME", ".config");
    let all = manager == Manager::Other;
    for (dir, folder, prefix, owner) in [
        (&data, "nemo/scripts", "NEMO", Manager::Nemo),
        (&data, "nautilus/scripts", "NAUTILUS", Manager::Nautilus),
        (&config, "caja/scripts", "CAJA", Manager::Caja),
    ] {
        if !(all || manager == owner) {
            continue;
        }
        let Some(Ok(entries)) = dir.as_ref().map(|d| std::fs::read_dir(d.join(folder))) else {
            continue;
        };
        let mut found: Vec<Script> = entries
            .flatten()
            .filter(|e| {
                e.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .map(|e| Script {
                name: e.file_name().to_string_lossy().replace('_', "__"),
                path: e.path(),
                manager: prefix,
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(found);
    }
    out
}

impl Script {
    pub fn environment(&self, items: &[Item]) -> Vec<(String, OsString)> {
        let mut paths = Vec::new();
        for item in items {
            paths.extend_from_slice(item.path.as_os_str().as_bytes());
            paths.push(b'\n');
        }
        let uris: String = items.iter().map(|i| file_uri(&i.path) + "\n").collect();
        let current = items
            .first()
            .and_then(|i| i.path.parent())
            .map(file_uri)
            .unwrap_or_default();
        let prefix = self.manager;
        vec![
            (
                format!("{prefix}_SCRIPT_SELECTED_FILE_PATHS"),
                OsString::from_vec(paths),
            ),
            (format!("{prefix}_SCRIPT_SELECTED_URIS"), uris.into()),
            (format!("{prefix}_SCRIPT_CURRENT_URI"), current.into()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, folder: bool, mime: &str) -> Item {
        Item {
            path: PathBuf::from(path),
            folder,
            mime: mime.into(),
        }
    }
    fn one(commands: Vec<Vec<OsString>>) -> Vec<OsString> {
        assert_eq!(commands.len(), 1, "{commands:?}");
        commands.into_iter().next().unwrap()
    }

    #[test]
    fn parses_and_matches_like_nemo() {
        let dir = Path::new("/usr/share/nemo/actions");
        let iso = Action::parse_nemo(
            "[Nemo Action]\nActive=true\nName=Make bootable USB stick\nName[xx]=Other\nExec=true -i %F\nSelection=S\nExtensions=iso;img;\n",
            dir,
        )
        .unwrap();
        let has = |_: &str| true;
        assert!(iso.applies(&[item("/d/a.ISO", false, "application/x-cd-image")], &has));
        assert!(!iso.applies(&[item("/d/a.txt", false, "text/plain")], &has));
        assert!(!iso.applies(
            &[item("/d/a.iso", false, ""), item("/d/b.iso", false, "")],
            &has
        ));
        let images = Action::parse_nemo(
            "[Nemo Action]\nName=Set as Wallpaper...\nExec=gsettings set key \"%U\"\nSelection=s\nMimetypes=image/*;\nConditions=dbus org.Cinnamon;\n",
            dir,
        )
        .unwrap();
        let photo = [item("/d/My photo.jpg", false, "image/jpeg")];
        assert!(images.applies(&photo, &|name| name == "org.Cinnamon"));
        assert!(!images.applies(&photo, &|_| false));
        assert_eq!(
            one(images.commands(&photo, None).unwrap()),
            ["gsettings", "set", "key", "file:///d/My%20photo.jpg"]
        );
        // Unsupported conditions and tokens, inactive and selection-less actions.
        for skipped in [
            "[Nemo Action]\nName=A\nExec=a\nExtensions=any;\nConditions=desktop;\n",
            "[Nemo Action]\nName=A\nExec=a %D\nExtensions=any;\n",
            "[Nemo Action]\nName=A\nExec=a\nExtensions=any;\nConditions=exec <check>;\n",
            "[Nemo Action]\nActive=false\nName=A\nExec=a\nExtensions=any;\n",
            "[Nemo Action]\nName=A\nExec=a\nSelection=None\nExtensions=any;\n",
            "[Nemo Action]\nName=A\nExec=a\n",
        ] {
            assert!(Action::parse_nemo(skipped, dir).is_none(), "{skipped}");
        }
    }

    #[test]
    fn builds_commands_with_quoting_and_tokens() {
        let dir = Path::new("/opt/actions");
        let items = [
            item("/data/a b.zip", false, ""),
            item("/data/it's.zip", false, ""),
        ];
        let plain = Action::parse_nemo(
            "[Nemo Action]\nName=X\nExec=<run.sh -v %F %P %e %X %%>\nSelection=m\nExtensions=zip;\n",
            dir,
        )
        .unwrap();
        assert_eq!(
            one(plain.commands(&items, Some(42)).unwrap()),
            [
                "/opt/actions/run.sh",
                "-v",
                "/data/a b.zip",
                "/data/it's.zip",
                "/data",
                "a b",
                "42",
                "%"
            ]
        );
        let quoted = Action::parse_nemo(
            "[Nemo Action]\nName=Extract %N\nExec=/x \"%F\"\nQuote=double\nExtensions=zip;\n",
            dir,
        )
        .unwrap();
        assert_eq!(quoted.label(&items[..1]), "Extract a b.zip");
        // Nemo's double quoting inside a quoted token joins the pieces.
        assert_eq!(
            one(quoted.commands(&items[..1], None).unwrap()),
            ["/x", "/data/a b.zip"]
        );
        let single = Action::parse_nemo(
            "[Nemo Action]\nName=X\nExec=sh -c 'echo %f' x\nQuote=double\nExtensions=zip;\n",
            dir,
        )
        .unwrap();
        assert_eq!(
            one(single.commands(&items[1..], None).unwrap()),
            ["sh", "-c", "echo it's.zip", "x"]
        );
        let bare = Action::parse_nemo(
            "[Nemo Action]\nName=X\nExec=/x %f\nQuote=single\nExtensions=zip;\n",
            dir,
        )
        .unwrap();
        assert_eq!(
            one(bare.commands(&items[1..], None).unwrap()),
            ["/x", "it's.zip"]
        );
    }

    #[test]
    fn reads_thunar_custom_actions() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<actions>
<action>
	<icon>utilities-terminal</icon>
	<name>Open Terminal Here</name>
	<name xml:lang="xx">Translated</name>
	<submenu></submenu>
	<unique-id>1</unique-id>
	<command>exo-open --working-directory %f --launch TerminalEmulator</command>
	<description>Example for a custom action</description>
	<range></range>
	<patterns>*</patterns>
	<startup-notify/>
	<directories/>
</action>
<action>
	<name>Compress &amp; send</name>
	<command>tar czf &quot;%n.tgz&quot; %N</command>
	<submenu>Archive</submenu>
	<patterns>*.txt;*.MD</patterns>
	<range>1-2</range>
	<text-files/>
</action>
</actions>"#;
        let actions = Action::parse_thunar(xml);
        assert_eq!(actions.len(), 2);
        let (terminal, compress) = (&actions[0], &actions[1]);
        assert_eq!(terminal.name, "Open Terminal Here");
        let has = |_: &str| true;
        assert!(terminal.applies(&[item("/data/src", true, "inode/directory")], &has));
        assert!(!terminal.applies(&[item("/data/a.txt", false, "text/plain")], &has));
        assert_eq!(
            one(terminal
                .commands(&[item("/my dir", true, "")], None)
                .unwrap()),
            [
                "exo-open",
                "--working-directory",
                "/my dir",
                "--launch",
                "TerminalEmulator"
            ]
        );
        assert_eq!(compress.name, "Compress & send");
        assert_eq!(compress.submenu.as_deref(), Some("Archive"));
        let notes = [
            item("/d/a b.txt", false, "text/plain"),
            item("/d/README.md", false, "text/markdown"),
        ];
        assert!(compress.applies(&notes, &has));
        assert!(!compress.applies(&[item("/d/a.png", false, "image/png")], &has));
        let three = [
            item("/d/1.txt", false, "text/plain"),
            item("/d/2.txt", false, "text/plain"),
            item("/d/3.txt", false, "text/plain"),
        ];
        assert!(!compress.applies(&three, &has));
        assert_eq!(
            one(compress.commands(&notes, None).unwrap()),
            ["tar", "czf", "a b.txt.tgz", "a b.txt", "README.md"]
        );
        assert!(wildcard(b"*.t?t", b"x.TXT") && !wildcard(b"*.txt", b"x.txt.gz"));
    }

    #[test]
    fn reads_kde_service_menus() {
        let text = "[Desktop Entry]\nType=Service\nMimeType=image/*;\nActions=small;big;_SEPARATOR_;\nX-KDE-Submenu=Resize\nX-KDE-MaxNumberOfUrls=5\n\n\
[Desktop Action small]\nName=Small copy\nExec=convert %f -resize 50% \"%f.small.png\"\n\n\
[Desktop Action big]\nName=Big copies\nExec=resize-all --icon %i %F\n";
        let actions = Action::parse_kde(text, Path::new("/usr/share/kio/servicemenus"));
        assert_eq!(
            actions.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["Small copy", "Big copies"]
        );
        assert_eq!(actions[0].submenu.as_deref(), Some("Resize"));
        let photos = [
            item("/p/a b.png", false, "image/png"),
            item("/p/c.jpg", false, "image/jpeg"),
        ];
        let has = |_: &str| true;
        assert!(actions[0].applies(&photos, &has));
        assert!(!actions[0].applies(&[item("/p/x.txt", false, "text/plain")], &has));
        // %f runs once per entry; %F passes all entries at once.
        let small = actions[0].commands(&photos, None).unwrap();
        assert_eq!(
            small,
            [
                vec![
                    "convert",
                    "/p/a b.png",
                    "-resize",
                    "50%",
                    "/p/a b.png.small.png"
                ],
                vec![
                    "convert",
                    "/p/c.jpg",
                    "-resize",
                    "50%",
                    "/p/c.jpg.small.png"
                ],
            ]
        );
        assert_eq!(
            one(actions[1].commands(&photos, None).unwrap()),
            ["resize-all", "--icon", "/p/a b.png", "/p/c.jpg"]
        );
        let folders = Action::parse_kde(
            "[Desktop Entry]\nMimeType=all/allfiles;\nActions=a;\n[Desktop Action a]\nName=A\nExec=a %U\n",
            Path::new("/"),
        );
        assert!(!folders[0].applies(&[item("/d", true, "inode/directory")], &has));
        assert!(folders[0].applies(&[item("/d/f", false, "application/x-zerosize")], &has));
        // Conditions File Minnow cannot evaluate hide the menu.
        assert!(
            Action::parse_kde(
                "[Desktop Entry]\nMimeType=all/all;\nX-KDE-ShowIfRunning=org.kde.konsole\nActions=a;\n[Desktop Action a]\nName=A\nExec=a\n",
                Path::new("/"),
            )
            .is_empty()
        );
    }

    #[test]
    fn detects_file_managers() {
        for (id, manager) in [
            (Some("nemo.desktop"), Manager::Nemo),
            (Some("org.gnome.Nautilus.desktop"), Manager::Nautilus),
            (Some("caja-folder-handler.desktop"), Manager::Caja),
            (Some("thunar.desktop"), Manager::Thunar),
            (Some("org.kde.dolphin.desktop"), Manager::Dolphin),
            (Some("pcmanfm.desktop"), Manager::Other),
            (None, Manager::Other),
        ] {
            assert_eq!(Manager::from_desktop_id(id), manager, "{id:?}");
        }
    }

    /// The only test that changes XDG variables, so loaders run in one place.
    #[test]
    fn reads_user_and_system_files_per_file_manager() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let system = temp.path().join("system");
        let config = temp.path().join("config");
        for (root, name, body) in [
            (&system, "a.nemo_action", "Name=System A"),
            (&user, "a.nemo_action", "Name=User A"),
            (&system, "b.nemo_action", "Name=System B"),
            (&user, "c.nemo_action", "Active=false\nName=C"),
            (&system, "c.nemo_action", "Name=System C"),
        ] {
            let dir = root.join("nemo/actions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(name),
                format!("[Nemo Action]\n{body}\nExec=true\nExtensions=any;\n"),
            )
            .unwrap();
        }
        let menu = |name: &str| {
            format!(
                "[Desktop Entry]\nMimeType=all/all;\nActions=a;\n[Desktop Action a]\nName={name}\nExec=true %F\n"
            )
        };
        for (root, folder, file, name, mode) in [
            (
                &system,
                "kio/servicemenus",
                "k.desktop",
                "KDE system",
                0o644,
            ),
            (&user, "kio/servicemenus", "u.desktop", "KDE user", 0o755),
            (
                &user,
                "kio/servicemenus",
                "n.desktop",
                "KDE not executable",
                0o644,
            ),
            (
                &system,
                "kservices5/ServiceMenus",
                "old.desktop",
                "KDE 5",
                0o644,
            ),
        ] {
            let dir = root.join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), menu(name)).unwrap();
            std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        std::fs::create_dir_all(config.join("Thunar")).unwrap();
        std::fs::write(
            config.join("Thunar/uca.xml"),
            "<actions><action><name>Thunar one</name><command>true %F</command><patterns>*</patterns><other-files/></action></actions>",
        )
        .unwrap();
        for (folder, root) in [("nemo/scripts", &user), ("caja/scripts", &config)] {
            let dir = root.join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Resize_it"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(
                dir.join("Resize_it"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::fs::write(dir.join("notes.txt"), "").unwrap();
        }
        // SAFETY: this is the only test that reads or writes these variables.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &user);
            std::env::set_var("XDG_DATA_DIRS", &system);
            std::env::set_var("XDG_CONFIG_HOME", &config);
            std::env::set_var("XDG_CONFIG_DIRS", temp.path().join("none"));
        }
        let names =
            |manager| -> Vec<String> { load(manager).into_iter().map(|a| a.name).collect() };
        assert_eq!(names(Manager::Nemo), ["User A", "System B"]);
        assert_eq!(names(Manager::Thunar), ["Thunar one"]);
        assert_eq!(names(Manager::Dolphin), ["KDE system", "KDE user", "KDE 5"]);
        assert!(names(Manager::Nautilus).is_empty());
        assert_eq!(names(Manager::Other).len(), 6);
        let scripts_of =
            |manager| -> Vec<&str> { scripts(manager).into_iter().map(|s| s.manager).collect() };
        assert_eq!(scripts_of(Manager::Nemo), ["NEMO"]);
        assert_eq!(scripts_of(Manager::Caja), ["CAJA"]);
        assert!(scripts_of(Manager::Thunar).is_empty());
        assert_eq!(scripts_of(Manager::Other), ["NEMO", "CAJA"]);
        let script = scripts(Manager::Nemo).pop().unwrap();
        assert_eq!(script.name, "Resize__it");
        let env = script.environment(&[item("/d/x y", false, "")]);
        assert_eq!(env[0].1, "/d/x y\n");
        assert_eq!(env[1].1, "file:///d/x%20y\n");
        assert_eq!(env[2].1, "file:///d");
    }
}
