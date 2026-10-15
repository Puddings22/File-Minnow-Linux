//! Renaming many files at once from one rule, with a preview of every new
//! name, and undoing File Minnow's own renames and moves to the Trash.
use anyhow::{Context, Result, bail};
use regex::{Regex, RegexBuilder};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    Lower,
    Upper,
    /// Every word starts with a capital letter.
    Title,
    /// Only the first letter is a capital.
    Sentence,
}
#[derive(Clone, Debug)]
pub enum Rule {
    /// Replace every occurrence of `find` (text, or a regular expression
    /// whose groups `with` can use as `$1`).
    Replace {
        find: String,
        with: String,
        regex: bool,
        case: bool,
    },
    /// A new name from a template: `[N]` the old name, `[E]` its extension,
    /// `[F]` the full old name, `[C]` a counter, `[P]` the parent folder's
    /// name, `[D]` the modified date.
    Pattern {
        template: String,
        start: u64,
        step: u64,
        digits: usize,
    },
    Case(Case),
}
#[derive(Clone, Debug)]
pub struct Options {
    pub rule: Rule,
    /// The rule applies to the whole name. Otherwise the extension (after
    /// the last dot, files only) is kept as it is.
    pub extension: bool,
}
#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub folder: bool,
    /// Modification time (seconds), for `[D]`.
    pub modified: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Why this item cannot be renamed as planned.
    pub problem: Option<String>,
}
impl Planned {
    pub fn changes(&self) -> bool {
        self.problem.is_none() && self.from != self.to
    }
}

/// Splits a name into the part the rule changes and the extension it keeps
/// (with its dot). Folders and names like `.bashrc` have no extension.
fn split(name: &str, folder: bool, extension: bool) -> (&str, &str) {
    if extension || folder {
        return (name, "");
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}
fn change_case(text: &str, case: Case) -> String {
    match case {
        Case::Lower => text.to_lowercase(),
        Case::Upper => text.to_uppercase(),
        Case::Title | Case::Sentence => {
            let mut out = String::with_capacity(text.len());
            let mut start = true;
            for c in text.chars() {
                if start && c.is_alphanumeric() {
                    out.extend(c.to_uppercase());
                    start = false;
                } else {
                    out.extend(c.to_lowercase());
                    if case == Case::Title && !c.is_alphanumeric() && c != '\'' {
                        start = true;
                    }
                }
            }
            out
        }
    }
}
fn local_date(seconds: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(seconds, 0)
        .earliest()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}
/// Why `name` is not usable as a file name, if it is not.
pub fn invalid_name(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        Some("The new name is empty")
    } else if name.contains(['/', '\0']) {
        Some("A name cannot contain “/”")
    } else if name.len() > 255 {
        Some("The new name is too long")
    } else {
        None
    }
}

enum Compiled {
    Replace {
        regex: Regex,
        with: String,
        expand: bool,
    },
    Pattern {
        template: String,
        start: u64,
        step: u64,
        digits: usize,
    },
    Case(Case),
}
fn compile(rule: &Rule) -> Result<Compiled> {
    Ok(match rule {
        Rule::Replace {
            find,
            with,
            regex,
            case,
        } => {
            let pattern = if *regex {
                find.clone()
            } else {
                regex::escape(find)
            };
            let compiled = RegexBuilder::new(&pattern)
                .case_insensitive(!case)
                .size_limit(1 << 20)
                .build()
                .context("Invalid regular expression")?;
            Compiled::Replace {
                regex: compiled,
                with: with.clone(),
                expand: *regex,
            }
        }
        Rule::Pattern {
            template,
            start,
            step,
            digits,
        } => Compiled::Pattern {
            template: template.clone(),
            start: *start,
            step: *step,
            digits: (*digits).min(20),
        },
        Rule::Case(case) => Compiled::Case(*case),
    })
}

/// The new name of every item, in order (the counter follows it), with
/// the problems that would stop a rename: names that are invalid, already
/// taken in the folder, or given to two items.
pub fn plan(items: &[Item], options: &Options) -> Result<Vec<Planned>> {
    let rule = compile(&options.rule)?;
    let mut planned = Vec::with_capacity(items.len());
    for (number, item) in items.iter().enumerate() {
        let Some(name) = item.path.file_name() else {
            planned.push(Planned {
                from: item.path.clone(),
                to: item.path.clone(),
                problem: Some("This item cannot be renamed".into()),
            });
            continue;
        };
        let Some(name) = name.to_str() else {
            planned.push(Planned {
                from: item.path.clone(),
                to: item.path.clone(),
                problem: Some("The name is not valid text; it is left as it is".into()),
            });
            continue;
        };
        let (stem, kept) = split(name, item.folder, options.extension);
        let new_stem = match &rule {
            Compiled::Replace {
                regex,
                with,
                expand,
            } => {
                if *expand {
                    regex.replace_all(stem, with.as_str()).into_owned()
                } else {
                    regex.replace_all(stem, regex::NoExpand(with)).into_owned()
                }
            }
            Compiled::Case(case) => change_case(stem, *case),
            Compiled::Pattern {
                template,
                start,
                step,
                digits,
            } => {
                let counter = start.saturating_add(step.saturating_mul(number as u64));
                let (old_stem, old_ext) = match name.rfind('.') {
                    Some(dot) if dot > 0 && !item.folder => (&name[..dot], &name[dot + 1..]),
                    _ => (name, ""),
                };
                let parent = item
                    .path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                template
                    .replace("[N]", old_stem)
                    .replace("[E]", old_ext)
                    .replace("[F]", name)
                    .replace("[P]", &parent)
                    .replace("[D]", &local_date(item.modified))
                    .replace("[C]", &format!("{counter:0digits$}"))
            }
        };
        let new_name = format!("{new_stem}{kept}");
        let to = item.path.with_file_name(&new_name);
        planned.push(Planned {
            from: item.path.clone(),
            problem: invalid_name(&new_name).map(str::to_owned),
            to,
        });
    }
    // Names given to two items, and names taken by other files (unless that
    // file is itself renamed, or is the same file with a different case on
    // a case-insensitive drive).
    let sources: HashSet<PathBuf> = planned
        .iter()
        .filter(|p| p.changes())
        .map(|p| p.from.clone())
        .collect();
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    for p in &planned {
        *seen.entry(p.to.clone()).or_default() += 1;
    }
    for p in planned.iter_mut().filter(|p| p.problem.is_none()) {
        if seen[&p.to] > 1 {
            p.problem = Some("Another item would get the same name".into());
        } else if p.from != p.to
            && !sources.contains(&p.to)
            && let Ok(existing) = std::fs::symlink_metadata(&p.to)
            && !same_file(&existing, &p.from)
        {
            p.problem = Some("A file with this name already exists".into());
        }
    }
    Ok(planned)
}
fn same_file(existing: &std::fs::Metadata, path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.dev() == existing.dev() && m.ino() == existing.ino())
}

/// Renames `from` to `to` without replacing anything at `to`.
pub fn rename_path(from: &Path, to: &Path) -> std::io::Result<()> {
    let source = CString::new(from.as_os_str().as_bytes())?;
    let target = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both strings are valid NUL-terminated paths for this call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        // A different case of the same name on a case-insensitive drive:
        // the "existing" target is the file itself. Go through a temporary
        // name.
        Some(libc::EEXIST) if std::fs::symlink_metadata(to).is_ok_and(|m| same_file(&m, from)) => {
            let temporary = temporary_name(from, 0);
            rename_path(from, &temporary)?;
            rename_path(&temporary, to).inspect_err(|_| {
                let _ = rename_path(&temporary, from);
            })
        }
        // Filesystems without RENAME_NOREPLACE: check, then rename.
        Some(libc::EINVAL) | Some(libc::ENOSYS) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
            }
            std::fs::rename(from, to)
        }
        _ => Err(error),
    }
}
fn temporary_name(path: &Path, number: usize) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!(
        ".{name}.minnow-rename-{}-{number}",
        std::process::id()
    ))
}

/// What a batch of renames did: the renames that happened (for undo), and
/// why it stopped early, if it did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub done: Vec<(PathBuf, PathBuf)>,
    pub error: Option<String>,
}
/// Renames each `(from, to)`. When a new name is another item's old name
/// (swapping or shifting numbers), every item first moves to a temporary
/// name in its folder. Stops at the first failure; what was done is kept.
pub fn apply(changes: &[(PathBuf, PathBuf)]) -> Outcome {
    let changes: Vec<&(PathBuf, PathBuf)> = changes.iter().filter(|(a, b)| a != b).collect();
    let sources: HashSet<&PathBuf> = changes.iter().map(|(from, _)| from).collect();
    let mut outcome = Outcome::default();
    let fail = |outcome: &mut Outcome, from: &Path, to: &Path, e: std::io::Error| {
        let name = to
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        outcome.error = Some(match e.raw_os_error() {
            Some(libc::EEXIST) => format!("“{name}” already exists"),
            _ => format!("Cannot rename {}: {e}", from.display()),
        });
    };
    if !changes.iter().any(|(_, to)| sources.contains(to)) {
        for (from, to) in changes {
            if let Err(e) = rename_path(from, to) {
                fail(&mut outcome, from, to, e);
                break;
            }
            outcome.done.push((from.clone(), to.clone()));
        }
        return outcome;
    }
    let mut parked: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();
    for (number, (from, to)) in changes.iter().enumerate() {
        let temporary = temporary_name(from, number);
        if let Err(e) = rename_path(from, &temporary) {
            fail(&mut outcome, from, to, e);
            // Put back what was moved aside; nothing was renamed.
            for (from, temporary, _) in parked.into_iter().rev() {
                let _ = rename_path(&temporary, &from);
            }
            return outcome;
        }
        parked.push((from.clone(), temporary, to.clone()));
    }
    let mut parked = parked.into_iter();
    for (from, temporary, to) in parked.by_ref() {
        if let Err(e) = rename_path(&temporary, &to) {
            fail(&mut outcome, &from, &to, e);
            let _ = rename_path(&temporary, &from);
            break;
        }
        outcome.done.push((from, to));
    }
    // After a failure, the rest go back to their old names.
    for (from, temporary, _) in parked {
        let _ = rename_path(&temporary, &from);
    }
    outcome
}

/// One step File Minnow can take back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Undo {
    /// Files renamed from the first path to the second.
    Renamed(Vec<(PathBuf, PathBuf)>),
    /// Files moved to the Trash from these paths.
    Trashed(Vec<PathBuf>),
}
impl Undo {
    pub fn label(&self) -> String {
        match self {
            Self::Renamed(pairs) if pairs.len() == 1 => "Undo Rename".into(),
            Self::Renamed(pairs) => format!("Undo Rename ({} items)", pairs.len()),
            Self::Trashed(paths) if paths.len() == 1 => "Undo Move to Trash".into(),
            Self::Trashed(paths) => format!("Undo Move to Trash ({} items)", paths.len()),
        }
    }
    /// Takes the step back. Returns a summary, or what went wrong.
    pub fn run(&self, trash: &TrashDirs) -> Result<String> {
        match self {
            Self::Renamed(pairs) => {
                let back: Vec<(PathBuf, PathBuf)> = pairs
                    .iter()
                    .rev()
                    .map(|(from, to)| (to.clone(), from.clone()))
                    .collect();
                let outcome = apply(&back);
                match outcome.error {
                    None => Ok(format!("Restored {} names", back.len())),
                    Some(e) => bail!(
                        "{e} (restored {} of {} names)",
                        outcome.done.len(),
                        back.len()
                    ),
                }
            }
            Self::Trashed(paths) => {
                let mut restored = 0;
                for path in paths {
                    trash
                        .restore(path)
                        .with_context(|| format!("Cannot restore {}", path.display()))?;
                    restored += 1;
                }
                Ok(format!("Restored {restored} items from the Trash"))
            }
        }
    }
}

/// The Trash folders of the freedesktop.org Trash specification.
#[derive(Clone, Debug)]
pub struct TrashDirs {
    /// `$XDG_DATA_HOME/Trash`.
    pub home: PathBuf,
}
impl TrashDirs {
    pub fn from_env() -> Self {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            });
        Self {
            home: data.join("Trash"),
        }
    }
    /// The Trash folders that may hold a file trashed from `original`, with
    /// the folder their recorded paths are relative to (None: absolute).
    fn candidates(&self, original: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
        let mut dirs = vec![(self.home.clone(), None)];
        if let Some(top) = original.parent().and_then(mount_top) {
            // SAFETY: getuid cannot fail.
            let uid = unsafe { libc::getuid() };
            dirs.push((top.join(format!(".Trash/{uid}")), Some(top.clone())));
            dirs.push((top.join(format!(".Trash-{uid}")), Some(top)));
        }
        dirs
    }
    /// Moves the most recently trashed item that came from `original` back.
    pub fn restore(&self, original: &Path) -> Result<()> {
        if std::fs::symlink_metadata(original).is_ok() {
            bail!("Something else now has that name");
        }
        let mut best: Option<(String, PathBuf, PathBuf)> = None;
        for (trash, top) in self.candidates(original) {
            let Ok(entries) = std::fs::read_dir(trash.join("info")) else {
                continue;
            };
            for entry in entries.flatten() {
                let info = entry.path();
                if info.extension().is_none_or(|e| e != "trashinfo") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&info) else {
                    continue;
                };
                let field = |key: &str| {
                    text.lines()
                        .find_map(|l| l.strip_prefix(key))
                        .map(str::trim)
                        .unwrap_or_default()
                };
                let recorded = PathBuf::from(
                    <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(
                        percent_decode(field("Path=")),
                    ),
                );
                let recorded = match &top {
                    Some(top) if recorded.is_relative() => top.join(recorded),
                    _ => recorded,
                };
                let date = field("DeletionDate=").to_owned();
                if recorded == original && best.as_ref().is_none_or(|(d, _, _)| date > *d) {
                    let name = info.file_stem().map(PathBuf::from).unwrap_or_default();
                    best = Some((date, trash.join("files").join(name), info));
                }
            }
        }
        let (_, file, info) = best.context("It is no longer in the Trash")?;
        rename_path(&file, original)?;
        let _ = std::fs::remove_file(info);
        Ok(())
    }
}
/// The top folder of the filesystem holding `path`: the highest ancestor
/// on the same device.
fn mount_top(path: &Path) -> Option<PathBuf> {
    let device = std::fs::metadata(path).ok()?.dev();
    let mut top = path.to_path_buf();
    while let Some(parent) = top.parent() {
        if std::fs::metadata(parent).map_or(true, |m| m.dev() != device) {
            break;
        }
        top = parent.to_path_buf();
    }
    Some(top)
}
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(code) = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(code);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn items(dir: &Path, names: &[&str]) -> Vec<Item> {
        names
            .iter()
            .map(|n| {
                let path = dir.join(n);
                if !path.exists() {
                    fs::write(&path, b"x").unwrap();
                }
                Item {
                    path,
                    folder: false,
                    modified: 0,
                }
            })
            .collect()
    }
    fn names(planned: &[Planned]) -> Vec<String> {
        planned
            .iter()
            .map(|p| p.to.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }
    fn options(rule: Rule) -> Options {
        Options {
            rule,
            extension: false,
        }
    }
    #[test]
    fn rules_build_names() {
        let temp = tempfile::tempdir().unwrap();
        let list = items(
            temp.path(),
            &["IMG_0001.JPG", "holiday photo.jpeg", "notes"],
        );
        let replace = |find: &str, with: &str, regex, case| {
            names(
                &plan(
                    &list,
                    &options(Rule::Replace {
                        find: find.into(),
                        with: with.into(),
                        regex,
                        case,
                    }),
                )
                .unwrap(),
            )
        };
        assert_eq!(
            replace("img", "pic", false, false),
            ["pic_0001.JPG", "holiday photo.jpeg", "notes"]
        );
        assert_eq!(
            replace("img", "pic", false, true),
            ["IMG_0001.JPG", "holiday photo.jpeg", "notes"]
        );
        assert_eq!(
            replace(r"(\w+) (\w+)", "$2-$1", true, false),
            ["IMG_0001.JPG", "photo-holiday.jpeg", "notes"]
        );
        // Plain text: "$1" is literal.
        assert_eq!(replace("notes", "$1", false, false)[2], "$1");
        let pattern = plan(
            &list,
            &options(Rule::Pattern {
                template: "Trip [C] ([N])".into(),
                start: 9,
                step: 1,
                digits: 3,
            }),
        )
        .unwrap();
        assert_eq!(
            names(&pattern),
            [
                "Trip 009 (IMG_0001).JPG",
                "Trip 010 (holiday photo).jpeg",
                "Trip 011 (notes)"
            ]
        );
        let title = plan(&list, &options(Rule::Case(Case::Title))).unwrap();
        assert_eq!(
            names(&title),
            ["Img_0001.JPG", "Holiday Photo.jpeg", "Notes"]
        );
        let upper_all = plan(
            &list,
            &Options {
                rule: Rule::Case(Case::Upper),
                extension: true,
            },
        )
        .unwrap();
        assert_eq!(
            names(&upper_all),
            ["IMG_0001.JPG", "HOLIDAY PHOTO.JPEG", "NOTES"]
        );
        assert_eq!(
            change_case("hello WORLD. again", Case::Sentence),
            "Hello world. again"
        );
        assert!(
            plan(
                &list,
                &options(Rule::Replace {
                    find: "(".into(),
                    with: String::new(),
                    regex: true,
                    case: false
                })
            )
            .is_err()
        );
    }
    #[test]
    fn conflicts_are_reported() {
        let temp = tempfile::tempdir().unwrap();
        let list = items(temp.path(), &["a.txt", "b.txt"]);
        fs::write(temp.path().join("taken.txt"), b"x").unwrap();
        let same = plan(
            &list,
            &options(Rule::Pattern {
                template: "same".into(),
                start: 1,
                step: 1,
                digits: 0,
            }),
        )
        .unwrap();
        assert!(
            same.iter()
                .all(|p| p.problem.as_deref() == Some("Another item would get the same name"))
        );
        let taken = plan(
            &list[..1],
            &options(Rule::Pattern {
                template: "taken".into(),
                start: 1,
                step: 1,
                digits: 0,
            }),
        )
        .unwrap();
        assert_eq!(
            taken[0].problem.as_deref(),
            Some("A file with this name already exists")
        );
        let empty = plan(
            &list[..1],
            &options(Rule::Replace {
                find: "a".into(),
                with: String::new(),
                regex: false,
                case: false,
            }),
        )
        .unwrap();
        assert_eq!(empty[0].to.file_name().unwrap(), ".txt");
        assert!(empty[0].problem.is_none(), "a hidden name is valid");
        let slash = plan(
            &list[..1],
            &options(Rule::Pattern {
                template: "x/y".into(),
                start: 1,
                step: 1,
                digits: 0,
            }),
        )
        .unwrap();
        assert!(slash[0].problem.is_some());
    }
    #[test]
    fn swaps_shifts_and_undo() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        for (name, content) in [("1.txt", "one"), ("2.txt", "two"), ("3.txt", "three")] {
            fs::write(dir.join(name), content).unwrap();
        }
        // Shift numbers up: 3→4 must happen before 2→3, which temporary names allow.
        let list = items(dir, &["1.txt", "2.txt", "3.txt"]);
        let planned = plan(
            &list,
            &options(Rule::Pattern {
                template: "[C]".into(),
                start: 2,
                step: 1,
                digits: 0,
            }),
        )
        .unwrap();
        assert!(planned.iter().all(Planned::changes), "{planned:?}");
        let changes: Vec<_> = planned
            .iter()
            .map(|p| (p.from.clone(), p.to.clone()))
            .collect();
        let outcome = apply(&changes);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(fs::read_to_string(dir.join("4.txt")).unwrap(), "three");
        assert_eq!(fs::read_to_string(dir.join("2.txt")).unwrap(), "one");
        assert!(!dir.join("1.txt").exists());
        let undo = Undo::Renamed(outcome.done);
        assert_eq!(undo.label(), "Undo Rename (3 items)");
        undo.run(&TrashDirs {
            home: dir.join("Trash"),
        })
        .unwrap();
        for (name, content) in [("1.txt", "one"), ("2.txt", "two"), ("3.txt", "three")] {
            assert_eq!(fs::read_to_string(dir.join(name)).unwrap(), content);
        }
        assert_eq!(
            fs::read_dir(dir).unwrap().count(),
            3,
            "no temporary files left"
        );
        // A failure stops the batch and keeps what was done.
        fs::write(dir.join("taken"), b"x").unwrap();
        let outcome = apply(&[
            (dir.join("1.txt"), dir.join("one")),
            (dir.join("2.txt"), dir.join("taken")),
        ]);
        assert_eq!(outcome.done.len(), 1);
        assert!(outcome.error.unwrap().contains("already exists"));
    }
    #[test]
    fn restore_from_trash() {
        let temp = tempfile::tempdir().unwrap();
        let trash = TrashDirs {
            home: temp.path().join("Trash"),
        };
        let original = temp.path().join("docs/My Report.txt");
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::create_dir_all(trash.home.join("files")).unwrap();
        fs::create_dir_all(trash.home.join("info")).unwrap();
        // Two trashed versions of the same file: the newer one comes back.
        for (name, date, content) in [
            ("My Report.txt", "2026-01-01T10:00:00", "old"),
            ("My Report.2.txt", "2026-02-01T10:00:00", "new"),
        ] {
            fs::write(trash.home.join("files").join(name), content).unwrap();
            let encoded = original.to_str().unwrap().replace(' ', "%20");
            fs::write(
                trash.home.join("info").join(format!("{name}.trashinfo")),
                format!("[Trash Info]\nPath={encoded}\nDeletionDate={date}\n"),
            )
            .unwrap();
        }
        let undo = Undo::Trashed(vec![original.clone()]);
        assert_eq!(undo.label(), "Undo Move to Trash");
        undo.run(&trash).unwrap();
        assert_eq!(fs::read_to_string(&original).unwrap(), "new");
        assert!(!trash.home.join("info/My Report.2.txt.trashinfo").exists());
        // The name is taken now: the older copy stays in the Trash.
        assert!(trash.restore(&original).is_err());
        fs::remove_file(&original).unwrap();
        trash.restore(&original).unwrap();
        assert_eq!(fs::read_to_string(&original).unwrap(), "old");
        assert!(trash.restore(&original).is_err());
    }
}
