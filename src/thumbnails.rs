//! Thumbnails as the freedesktop.org Thumbnail Managing Standard defines
//! them: the cache under `$XDG_CACHE_HOME/thumbnails` that GNOME Files,
//! Nemo, Caja, Thunar and Dolphin already fill, and the desktop's
//! thumbnailers (`*.thumbnailer` files) for what is not there yet. New
//! thumbnails go back into the same cache, so the file manager can use them.
use gtk::{gdk_pixbuf::Pixbuf, gio, glib, prelude::*};
use std::{
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Standard sizes: the cache folder and the largest side in pixels.
const SIZES: [(&str, i32); 4] = [
    ("normal", 128),
    ("large", 256),
    ("x-large", 512),
    ("xx-large", 1024),
];
/// Files larger than this are not decoded here (a thumbnailer may still
/// read them, as it streams).
const IMAGE_BYTES: u64 = 80 << 20;
const TIMEOUT: Duration = Duration::from_secs(20);

/// The desktop's thumbnailers: for each, the MIME types and the command.
#[derive(Default)]
pub struct Thumbnailers {
    entries: Vec<(Vec<String>, String)>,
}
impl Thumbnailers {
    /// Reads `thumbnailers/*.thumbnailer` from the XDG data folders; ones
    /// whose program is missing (`TryExec`) are left out.
    pub fn load() -> Self {
        let mut dirs = vec![glib::user_data_dir()];
        dirs.extend(glib::system_data_dirs());
        let mut entries = Vec::new();
        for dir in dirs {
            let Ok(list) = std::fs::read_dir(dir.join("thumbnailers")) else {
                continue;
            };
            let mut files: Vec<PathBuf> = list.flatten().map(|e| e.path()).collect();
            files.sort();
            for file in files {
                if file.extension().is_none_or(|e| e != "thumbnailer") {
                    continue;
                }
                let keys = glib::KeyFile::new();
                if keys
                    .load_from_file(&file, glib::KeyFileFlags::NONE)
                    .is_err()
                {
                    continue;
                }
                let group = "Thumbnailer Entry";
                let (Ok(exec), Ok(types)) = (
                    keys.string(group, "Exec"),
                    keys.string_list(group, "MimeType"),
                ) else {
                    continue;
                };
                if let Ok(program) = keys.string(group, "TryExec")
                    && glib::find_program_in_path(program.as_str()).is_none()
                {
                    continue;
                }
                entries.push((
                    types.iter().map(|t| t.to_string()).collect(),
                    exec.to_string(),
                ));
            }
        }
        Self { entries }
    }
    fn command(&self, mime: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(types, _)| types.iter().any(|t| t == mime))
            .map(|(_, exec)| exec.as_str())
    }
}

fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".cache"))
        .join("thumbnails")
}
fn modified_seconds(path: &Path) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.mtime())
}
/// A cached thumbnail, if it still matches the file's modification time.
fn load_valid(file: &Path, mtime: i64) -> Option<Pixbuf> {
    let pixbuf = Pixbuf::from_file(file).ok()?;
    match pixbuf.option("tEXt::Thumb::MTime") {
        Some(stored) if stored.parse::<i64>().ok() != Some(mtime) => None,
        _ => Some(pixbuf),
    }
}
/// Scales down to fit `size` × `size`, keeping the proportions.
fn fit(pixbuf: Pixbuf, size: i32) -> Pixbuf {
    let (w, h) = (pixbuf.width(), pixbuf.height());
    if w <= size && h <= size {
        return pixbuf;
    }
    let scale = f64::from(size) / f64::from(w.max(h));
    let (nw, nh) = (
        ((f64::from(w) * scale).round() as i32).max(1),
        ((f64::from(h) * scale).round() as i32).max(1),
    );
    pixbuf
        .scale_simple(nw, nh, gtk::gdk_pixbuf::InterpType::Bilinear)
        .unwrap_or(pixbuf)
}

/// The thumbnail of a file, at most `size` pixels on its longer side: from
/// the cache, or made now (by decoding a picture, or with a thumbnailer)
/// and cached. None when there is none to be had.
pub fn thumbnail(
    path: &Path,
    mime: &str,
    size: i32,
    thumbnailers: &Thumbnailers,
) -> Option<Pixbuf> {
    let root = cache_root();
    // Thumbnails of the thumbnails themselves are never made.
    if path.starts_with(&root) {
        return None;
    }
    let mtime = modified_seconds(path)?;
    let uri = gio::File::for_path(path).uri();
    let name = format!(
        "{}.png",
        glib::compute_checksum_for_data(glib::ChecksumType::Md5, uri.as_bytes())?
    );
    // The smallest cached size that is large enough, else the largest there is.
    let wanted = SIZES
        .iter()
        .position(|(_, s)| *s >= size)
        .unwrap_or(SIZES.len() - 1);
    for (folder, _) in SIZES[wanted..].iter().chain(SIZES[..wanted].iter().rev()) {
        if let Some(pixbuf) = load_valid(&root.join(folder).join(&name), mtime) {
            return Some(fit(pixbuf, size));
        }
    }
    let failed = root.join("fail/file-minnow").join(&name);
    if load_valid(&failed, mtime).is_some() {
        return None;
    }
    let (folder, pixels) = SIZES[wanted.min(1)];
    let made = make(path, mime, &uri, pixels, thumbnailers);
    match made {
        Some(pixbuf) => {
            store(&pixbuf, &root.join(folder).join(&name), &uri, mtime);
            Some(fit(pixbuf, size))
        }
        None => {
            if let Some(marker) = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 1, 1) {
                marker.fill(0);
                store(&marker, &failed, &uri, mtime);
            }
            None
        }
    }
}
fn make(
    path: &Path,
    mime: &str,
    uri: &str,
    size: i32,
    thumbnailers: &Thumbnailers,
) -> Option<Pixbuf> {
    let readable = gtk::gdk_pixbuf::Pixbuf::formats()
        .iter()
        .any(|f| f.mime_types().iter().any(|t| t == mime));
    if readable
        && std::fs::metadata(path).is_ok_and(|m| m.len() <= IMAGE_BYTES)
        && let Ok(pixbuf) = Pixbuf::from_file_at_scale(path, size, size, true)
    {
        return Some(pixbuf.apply_embedded_orientation().unwrap_or(pixbuf));
    }
    run_thumbnailer(thumbnailers.command(mime)?, path, uri, size)
}
/// Runs a thumbnailer's `Exec` line (`%i` path, `%u` URI, `%o` output,
/// `%s` size), with a time limit, and reads what it wrote.
fn run_thumbnailer(exec: &str, path: &Path, uri: &str, size: i32) -> Option<Pixbuf> {
    let work = tempfile_dir()?;
    let output = work.join("thumbnail.png");
    let argv = glib::shell_parse_argv(exec).ok()?;
    let mut words: Vec<std::ffi::OsString> = Vec::new();
    for word in argv {
        let word = word.to_string_lossy().into_owned();
        match word.as_str() {
            "%i" => words.push(path.as_os_str().to_owned()),
            "%o" => words.push(output.as_os_str().to_owned()),
            _ => words.push(
                word.replace("%u", uri)
                    .replace("%s", &size.to_string())
                    .replace("%%", "%")
                    .into(),
            ),
        }
    }
    let (program, args) = words.split_first()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    let finished = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let pixbuf = finished.then(|| Pixbuf::from_file(&output).ok()).flatten();
    let _ = std::fs::remove_dir_all(&work);
    pixbuf
}
/// A private folder for one thumbnailer run.
fn tempfile_dir() -> Option<PathBuf> {
    let base = glib::user_runtime_dir();
    let dir = base.join(format!(
        "file-minnow-thumbnail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    Some(dir)
}
/// Saves a thumbnail with the URI and time of its file, written to a
/// temporary name and renamed, as the standard asks.
fn store(pixbuf: &Pixbuf, file: &Path, uri: &str, mtime: i64) {
    let Some(folder) = file.parent() else {
        return;
    };
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
        .is_err()
    {
        return;
    }
    let temporary = file.with_extension(format!("png.minnow-{}", std::process::id()));
    let saved = pixbuf.savev(
        &temporary,
        "png",
        &[
            ("tEXt::Thumb::URI", uri),
            ("tEXt::Thumb::MTime", &mtime.to_string()),
            ("tEXt::Software", "File Minnow"),
        ],
    );
    if saved.is_ok() {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .and_then(|f| f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600)));
        let _ = std::fs::rename(&temporary, file);
    } else {
        let _ = std::fs::remove_file(&temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_made_and_failed_thumbnails() {
        let temp = tempfile::tempdir().unwrap();
        // SAFETY: tests in this module run on their own; the cache folder
        // must point into the temporary folder before GLib reads it.
        unsafe { std::env::set_var("XDG_CACHE_HOME", temp.path().join("cache")) };
        let picture = temp.path().join("photo.png");
        let image = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 600, 300).unwrap();
        image.fill(0x3366_99ff);
        image.savev(&picture, "png", &[]).unwrap();
        let none = Thumbnailers::default();
        let small = thumbnail(&picture, "image/png", 100, &none).unwrap();
        assert_eq!((small.width(), small.height()), (100, 50));
        // Saved in the standard place, with the file's URI and time.
        let uri = gio::File::for_path(&picture).uri();
        let md5 = glib::compute_checksum_for_data(glib::ChecksumType::Md5, uri.as_bytes()).unwrap();
        let cached = temp
            .path()
            .join(format!("cache/thumbnails/normal/{md5}.png"));
        let stored = Pixbuf::from_file(&cached).unwrap();
        assert_eq!(
            stored.option("tEXt::Thumb::URI").as_deref(),
            Some(uri.as_str())
        );
        assert_eq!((stored.width(), stored.height()), (128, 64));
        // Something without a thumbnailer is marked as failed, once.
        let other = temp.path().join("data.bin");
        std::fs::write(&other, b"x").unwrap();
        assert!(thumbnail(&other, "application/octet-stream", 128, &none).is_none());
        assert!(
            temp.path()
                .join("cache/thumbnails/fail/file-minnow")
                .read_dir()
                .unwrap()
                .count()
                == 1
        );
        // A thumbnailer command with %i, %o and %s.
        let script = temp.path().join("fake-thumbnailer");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncp '{}' \"$2\"\n", picture.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let fake = Thumbnailers {
            entries: vec![(
                vec!["application/x-test".into()],
                format!("{} %i %o %s", script.display()),
            )],
        };
        let document = temp.path().join("doc.test");
        std::fs::write(&document, b"x").unwrap();
        let made = thumbnail(&document, "application/x-test", 64, &fake).unwrap();
        assert_eq!(made.width(), 64);
    }
}
