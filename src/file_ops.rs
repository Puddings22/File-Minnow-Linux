//! File operations behind the right-click menu: launching, the file
//! clipboard, rename, trash, terminals and the file manager's D-Bus interface.
use crate::actions::file_uri;
use anyhow::{Context, Result, bail};
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    ffi::{CString, OsString},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The content type (a MIME type on Linux) for a file name, without reading
/// the file, and whether GIO is unsure. The gio binding always passes a data
/// buffer, and an empty buffer makes GIO answer "empty file"; pass none.
pub fn guess_type(name: &Path) -> (glib::GString, bool) {
    use glib::translate::{FromGlibPtrFull, ToGlibPtr};
    let mut uncertain = 0;
    // SAFETY: `name` is converted to a NUL-terminated filename for the call;
    // GIO returns a newly allocated string that we take ownership of.
    unsafe {
        let kind = gio::ffi::g_content_type_guess(
            name.to_glib_none().0,
            std::ptr::null(),
            0,
            &mut uncertain,
        );
        (glib::GString::from_glib_full(kind), uncertain != 0)
    }
}

/// The user's login autostart entry (XDG Autostart, read by Cinnamon, GNOME,
/// KDE, Xfce, MATE, Budgie and others).
pub fn autostart_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| Path::new(d).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("autostart/file-minnow.desktop"))
}

/// Whether File Minnow starts at login: our entry exists and is not
/// switched off by the desktop's own startup settings.
pub fn autostart_enabled() -> bool {
    autostart_path().is_some_and(|p| autostart_enabled_at(&p))
}
fn autostart_enabled_at(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    !text.lines().map(str::trim).any(|line| {
        line.eq_ignore_ascii_case("Hidden=true")
            || line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
    })
}

/// Quotes an argument for a desktop entry's Exec line.
fn exec_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./+,:@".contains(c))
    {
        return arg.to_owned();
    }
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    // `%` is a field code in Exec lines.
    quoted.replace('%', "%%")
}

/// Creates or removes the login entry. It starts this same program (by name
/// when it is the one on PATH) in the tray, with `data_dir` when that is not
/// the default.
pub fn set_autostart(enabled: bool, data_dir: Option<&Path>) -> Result<()> {
    let path = autostart_path().context("Cannot find the autostart folder")?;
    set_autostart_at(&path, enabled, data_dir)
}
fn set_autostart_at(path: &Path, enabled: bool, data_dir: Option<&Path>) -> Result<()> {
    if !enabled {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    let exe = std::env::current_exe().context("Cannot locate the File Minnow program")?;
    let on_path = crate::actions::find_program("file-minnow")
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| exe.canonicalize().is_ok_and(|e| e == p));
    let mut exec = if on_path {
        "file-minnow".to_owned()
    } else {
        exec_arg(&exe.to_string_lossy())
    };
    if let Some(dir) = data_dir {
        exec.push_str(" --data-dir ");
        exec.push_str(&exec_arg(&dir.to_string_lossy()));
    }
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=File Minnow\nComment=Start file search in the background\nExec={exec} gui --hidden\nIcon=file-minnow\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, entry)?;
    Ok(())
}

/// Opens files with an application. Desktop applications get /dev/null
/// for input and output, so a browser or editor started from File Minnow
/// does not write its messages into the terminal File Minnow runs in.
pub fn launch(
    info: &gio::AppInfo,
    paths: &[PathBuf],
    context: Option<&gdk::AppLaunchContext>,
) -> std::result::Result<(), String> {
    if let Some(desktop) = info.downcast_ref::<gio::DesktopAppInfo>() {
        use ::gio::prelude::DesktopAppInfoExtManual;
        let null = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
        };
        if let (Ok(mut input), Ok(mut output), Ok(mut errors)) = (null(), null(), null()) {
            let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
            let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
            return desktop
                .launch_uris_as_manager_with_fds(
                    &uris,
                    context,
                    glib::SpawnFlags::SEARCH_PATH,
                    None,
                    None,
                    &mut input,
                    &mut output,
                    &mut errors,
                )
                .map_err(|e| e.message().to_owned());
        }
    }
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    info.launch(&files, context)
        .map_err(|e| e.message().to_owned())
}

/// Whether a guessed content type names an actual type rather than GIO's
/// "unknown data" or "empty file" answers.
pub fn known_type(kind: &str) -> bool {
    !matches!(
        kind,
        "application/octet-stream" | "application/x-zerosize" | ""
    )
}

/// Starts a program without waiting for it; a thread reaps it on exit.
pub fn spawn(argv: &[OsString], dir: Option<&Path>, env: &[(String, OsString)]) -> Result<()> {
    let (program, args) = argv.split_first().context("Empty command")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = dir.filter(|d| d.is_dir()) {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot start {}", Path::new(program).display()))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn gsetting(schema: &str, key: &str) -> Option<String> {
    let source = gio::SettingsSchemaSource::default()?;
    if !source.lookup(schema, true)?.has_key(key) {
        return None;
    }
    let value = gio::Settings::new(schema).string(key).to_string();
    (!value.is_empty()).then_some(value)
}

/// The desktop's terminal and the option that makes it run a command:
/// Cinnamon's or GNOME's setting, then common terminals.
pub fn terminal() -> Option<(String, String)> {
    for desktop in ["org.cinnamon", "org.gnome"] {
        let schema = format!("{desktop}.desktop.default-applications.terminal");
        if let Some(exec) = gsetting(&schema, "exec")
            && crate::actions::find_program(&exec).is_some()
        {
            let arg = gsetting(&schema, "exec-arg").unwrap_or_else(|| "-e".into());
            return Some((exec, arg));
        }
    }
    [
        ("x-terminal-emulator", "-e"),
        ("gnome-terminal", "--"),
        ("konsole", "-e"),
        ("xfce4-terminal", "-x"),
        ("mate-terminal", "-x"),
        ("tilix", "-e"),
        ("kitty", "--"),
        ("alacritty", "-e"),
        ("xterm", "-e"),
    ]
    .into_iter()
    .find(|(name, _)| crate::actions::find_program(name).is_some())
    .map(|(name, arg)| (name.to_owned(), arg.to_owned()))
}

/// Wraps `argv` so it runs inside the desktop's terminal.
pub fn in_terminal(argv: Vec<OsString>) -> Result<Vec<OsString>> {
    let (exec, arg) = terminal().context("No terminal program found")?;
    let mut wrapped = vec![OsString::from(exec), OsString::from(arg)];
    wrapped.extend(argv);
    Ok(wrapped)
}

/// Opens the desktop's terminal in `dir`.
pub fn open_terminal(dir: &Path) -> Result<()> {
    let (exec, _) = terminal().context("No terminal program found")?;
    spawn(&[OsString::from(exec)], Some(dir), &[])
}

/// Puts files on the clipboard the way file managers do, so pasting in
/// Nemo, Nautilus, Caja, Thunar or Dolphin copies (or moves) them; text
/// fields receive the paths.
pub fn set_clipboard_files(paths: &[PathBuf], cut: bool) {
    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let text = paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    let targets = [
        gtk::TargetEntry::new("x-special/gnome-copied-files", gtk::TargetFlags::empty(), 0),
        gtk::TargetEntry::new("text/uri-list", gtk::TargetFlags::empty(), 1),
        gtk::TargetEntry::new("UTF8_STRING", gtk::TargetFlags::empty(), 2),
        gtk::TargetEntry::new("text/plain;charset=utf-8", gtk::TargetFlags::empty(), 2),
        gtk::TargetEntry::new("TEXT", gtk::TargetFlags::empty(), 2),
        gtk::TargetEntry::new("STRING", gtk::TargetFlags::empty(), 2),
    ];
    gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD).set_with_data(&targets, move |_, data, info| {
        match info {
            0 => data.set(&data.target(), 8, gnome.as_bytes()),
            1 => {
                let refs: Vec<&str> = uris.iter().map(String::as_str).collect();
                data.set_uris(&refs);
            }
            _ => {
                data.set_text(&text);
            }
        }
    });
}

/// Renames within the same folder and never replaces an existing entry.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf> {
    let new_name = new_name.trim_end_matches('\n');
    if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains(['/', '\0'])
    {
        bail!("A name cannot be empty, “.” or “..”, or contain “/”.");
    }
    let target = path.with_file_name(new_name);
    if target == path {
        return Ok(target);
    }
    let from = CString::new(path.as_os_str().as_bytes())?;
    let to = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: both strings are valid NUL-terminated paths for this call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(target);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EEXIST) => bail!("“{new_name}” already exists in this folder."),
        // Filesystems without RENAME_NOREPLACE: check, then rename.
        Some(libc::EINVAL) | Some(libc::ENOSYS) => {
            if std::fs::symlink_metadata(&target).is_ok() {
                bail!("“{new_name}” already exists in this folder.");
            }
            std::fs::rename(path, &target)?;
            Ok(target)
        }
        _ => Err(error.into()),
    }
}

/// Moves entries to the desktop trash. Returns one message per failure.
pub fn trash(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| {
            gio::File::for_path(path)
                .trash(None::<&gio::Cancellable>)
                .err()
                .map(|e| format!("{}: {}", path.display(), e.message()))
        })
        .collect()
}

/// Whether a D-Bus name currently has an owner on the session bus.
pub fn bus_has_owner(name: &str) -> bool {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        return false;
    };
    bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&(name,).to_variant()),
        Some(glib::VariantTy::new("(b)").unwrap()),
        gio::DBusCallFlags::NONE,
        500,
        None::<&gio::Cancellable>,
    )
    .ok()
    .and_then(|reply| reply.get::<(bool,)>())
    .is_some_and(|(owned,)| owned)
}

/// Asks the desktop's file manager (org.freedesktop.FileManager1: Nemo,
/// Nautilus, Caja, Dolphin, Thunar...) to show items selected in their
/// folder (`ShowItems`) or their properties window (`ShowItemProperties`).
/// `done(false)` when no file manager answered, so callers can fall back.
pub fn file_manager(method: &str, paths: &[PathBuf], done: impl FnOnce(bool) + 'static) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        done(false);
        return;
    };
    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    bus.call(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        method,
        Some(&(uris, "").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        10_000,
        None::<&gio::Cancellable>,
        move |result| done(result.is_ok()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_types_from_names_only() {
        assert_eq!(guess_type(Path::new("/x/a.pdf")).0, "application/pdf");
        let (kind, uncertain) = guess_type(Path::new("/x/a.unknownext"));
        assert!(uncertain && !known_type(&kind), "{kind}");
        assert!(known_type(&guess_type(Path::new("/x/a.png")).0));
    }

    #[test]
    fn autostart_entries() {
        assert_eq!(exec_arg("/usr/bin/file-minnow"), "/usr/bin/file-minnow");
        assert_eq!(exec_arg("/home/a b/x"), "\"/home/a b/x\"");
        assert_eq!(exec_arg("/p/$x\"%"), "\"/p/\\$x\\\"%%\"");
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("autostart/file-minnow.desktop");
        assert!(!autostart_enabled_at(&path));
        set_autostart_at(&path, true, Some(Path::new("/data dir"))).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("--data-dir \"/data dir\" gui --hidden"),
            "{text}"
        );
        assert!(autostart_enabled_at(&path));
        std::fs::write(&path, text.replace("enabled=true", "enabled=false")).unwrap();
        assert!(!autostart_enabled_at(&path));
        set_autostart_at(&path, false, None).unwrap();
        assert!(!path.exists());
        set_autostart_at(&path, false, None).unwrap();
    }

    #[test]
    fn rename_never_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        assert!(rename(&a, "b.txt").is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        for bad in ["", "..", "x/y"] {
            assert!(rename(&a, bad).is_err(), "{bad}");
        }
        let c = rename(&a, "c d.txt").unwrap();
        assert_eq!(c, temp.path().join("c d.txt"));
        assert!(!a.exists() && c.exists());
    }
}
