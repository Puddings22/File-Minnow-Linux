//! Everything file lists (`.efu`): CSV with a header, as voidtools documents
//! them. File Minnow writes them (Export Results, `search --efu`) and can
//! index one as a folder: a list made on a server or of a drive that is
//! put away, with the list's paths shown under a local folder.
use crate::index::{Entry, Kind};
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const HEADER: &str = "Filename,Size,Date Modified,Date Created,Attributes";
/// Seconds between 1601-01-01 (FILETIME) and 1970-01-01 (Unix).
const FILETIME_EPOCH: i64 = 11_644_473_600;
const DIRECTORY: u32 = 0x10;
const ARCHIVE: u32 = 0x20;

fn filetime(seconds: i64) -> i64 {
    (seconds + FILETIME_EPOCH).saturating_mul(10_000_000)
}
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}
/// One line for an entry. Names that are not valid UTF-8 are written with
/// replacement characters (the format is UTF-8).
pub fn line(entry: &Entry) -> String {
    let folder = entry.kind == Kind::Folder;
    let created = if entry.created == crate::table::NO_DATE {
        String::new()
    } else {
        filetime(entry.created).to_string()
    };
    format!(
        "{},{},{},{},{}",
        quoted(&entry.display_path()),
        if matches!(entry.kind, Kind::File | Kind::Folder) {
            entry.size.to_string()
        } else {
            String::new()
        },
        filetime(entry.modified),
        created,
        if folder { DIRECTORY } else { ARCHIVE }
    )
}
pub fn write_header(out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "{HEADER}")
}

/// One entry of a file list, with the path as written in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub path: String,
    pub folder: bool,
    pub size: u64,
    pub modified: Option<i64>,
    pub created: Option<i64>,
}
/// Splits one CSV line; quoted values may contain commas and `""`.
fn fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    out.push(field);
    out
}
/// A date as FILETIME or ISO 8601 (local time when it has no zone), in
/// seconds since the epoch.
fn date(text: &str) -> Option<i64> {
    use chrono::TimeZone;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(ticks) = text.parse::<i64>() {
        return Some(ticks / 10_000_000 - FILETIME_EPOCH);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(t.timestamp());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return chrono::Local
                .from_local_datetime(&t)
                .earliest()
                .map(|t| t.timestamp());
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
        .map(|t| t.timestamp())
}
fn attributes(text: &str) -> u32 {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).unwrap_or(0),
        None => text.parse().unwrap_or(0),
    }
}
/// Reads a file list. Relative names are made absolute against the list's
/// folder (with Windows separators turned into `/` there).
pub fn read(data: &[u8], list_folder: &Path) -> Result<Vec<Record>> {
    let text = std::str::from_utf8(data).context("A file list must be UTF-8 text")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: Vec<String> = fields(lines.next().context("The file list is empty")?)
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    let column = |name: &str| header.iter().position(|h| h == name);
    let Some(name_column) = column("filename") else {
        bail!("Not a file list: the first line has no Filename column");
    };
    let (size, modified, created, attrs) = (
        column("size"),
        column("date modified"),
        column("date created"),
        column("attributes"),
    );
    let mut records = Vec::new();
    for line in lines {
        let values = fields(line);
        let value = |i: Option<usize>| {
            i.and_then(|i| values.get(i))
                .map(String::as_str)
                .unwrap_or("")
        };
        let mut path = value(Some(name_column)).to_owned();
        if path.is_empty() {
            continue;
        }
        let mut folder = attributes(value(attrs)) & DIRECTORY != 0;
        if path.len() > 1 && (path.ends_with('/') || path.ends_with('\\')) {
            folder = true;
            path.pop();
        }
        let windows = path.contains('\\') || path.as_bytes().get(1) == Some(&b':');
        let absolute = path.starts_with('/') || (windows && is_windows_absolute(&path));
        if !absolute {
            // Relative to the list (a leading "\" too).
            let relative = path.trim_start_matches(['\\', '/']).replace('\\', "/");
            path = list_folder.join(relative).to_string_lossy().into_owned();
        }
        records.push(Record {
            path,
            folder,
            size: value(size).trim().parse().unwrap_or(0),
            modified: date(value(modified)),
            created: date(value(created)),
        });
    }
    Ok(records)
}
fn is_windows_absolute(path: &str) -> bool {
    path.starts_with("\\\\") || path.as_bytes().get(1) == Some(&b':')
}
/// The longest folder all records share, as written in the list (for the
/// prefix to show under a local folder).
pub fn common_folder(records: &[Record]) -> String {
    let separator = |p: &str| {
        if p.contains('\\') && !p.starts_with('/') {
            '\\'
        } else {
            '/'
        }
    };
    let Some(first) = records.first() else {
        return String::new();
    };
    let sep = separator(&first.path);
    let mut prefix: Vec<&str> = first.path.split(sep).collect();
    if !first.folder {
        prefix.pop();
    }
    for record in &records[1..] {
        let parts: Vec<&str> = record.path.split(sep).collect();
        let limit = if record.folder {
            parts.len()
        } else {
            parts.len() - 1
        };
        let same = prefix
            .iter()
            .zip(&parts[..limit])
            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
            .count();
        prefix.truncate(same);
    }
    let joined = prefix.join(&sep.to_string());
    if joined.is_empty() {
        sep.to_string()
    } else {
        joined
    }
}
/// Puts a listed path under the local folder `to`, when it starts with
/// `from` (compared without regard to case or separator style).
pub fn map(path: &str, from: &str, to: &Path) -> Option<PathBuf> {
    let normalize = |p: &str| p.replace('\\', "/");
    let (path, from) = (normalize(path), normalize(from));
    let from = from.trim_end_matches('/');
    let rest = if from.is_empty() {
        path.as_str()
    } else {
        let head = path.get(..from.len())?;
        if !head.eq_ignore_ascii_case(from) {
            return None;
        }
        &path[from.len()..]
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let rest = rest.trim_start_matches('/');
    if rest.split('/').any(|part| part == ".." || part == ".") {
        return None;
    }
    Some(if rest.is_empty() {
        to.to_path_buf()
    } else {
        to.join(rest)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_and_reads_lists() {
        let mut out = Vec::new();
        write_header(&mut out).unwrap();
        let file = Entry::new(
            b"/srv/share/a \"b\", c.txt".to_vec(),
            Kind::File,
            1024,
            1_700_000_000,
        )
        .created(1_600_000_000);
        let folder = Entry::new(b"/srv/share/docs".to_vec(), Kind::Folder, 0, 1_700_000_000);
        writeln!(out, "{}", line(&file)).unwrap();
        writeln!(out, "{}", line(&folder)).unwrap();
        let records = read(&out, Path::new("/lists")).unwrap();
        assert_eq!(
            records,
            [
                Record {
                    path: "/srv/share/a \"b\", c.txt".into(),
                    folder: false,
                    size: 1024,
                    modified: Some(1_700_000_000),
                    created: Some(1_600_000_000),
                },
                Record {
                    path: "/srv/share/docs".into(),
                    folder: true,
                    size: 0,
                    modified: Some(1_700_000_000),
                    created: None,
                },
            ]
        );
        assert_eq!(common_folder(&records), "/srv/share");
    }
    #[test]
    fn reads_everything_style_lists() {
        let text = "\u{feff}Filename,Size,Date Modified,Date Created,Attributes\r\n\
                    \"C:\\Folder\\File.txt\",1024,132537600000000000,,32\r\n\
                    \"C:\\Folder\\Subfolder\\\",,,2026-09-23T21:31:00,16\r\n\
                    relative\\note.md,5,,,0x20\r\n";
        let records = read(text.as_bytes(), Path::new("/home/a/lists")).unwrap();
        assert_eq!(records[0].path, "C:\\Folder\\File.txt");
        assert_eq!(records[0].modified, Some(1_609_286_400));
        assert!(records[1].folder && records[1].path == "C:\\Folder\\Subfolder");
        assert!(records[1].created.is_some());
        assert_eq!(records[2].path, "/home/a/lists/relative/note.md");
        assert!(read(b"Name,Size\nx,1\n", Path::new("/")).is_err());
        // Windows paths under a local folder.
        let to = Path::new("/mnt/c");
        assert_eq!(
            map("C:\\Folder\\File.txt", "c:\\folder", to),
            Some(PathBuf::from("/mnt/c/File.txt"))
        );
        assert_eq!(
            map("C:\\Folder", "C:\\Folder\\", to),
            Some(PathBuf::from("/mnt/c"))
        );
        assert_eq!(map("C:\\Folders\\x", "C:\\Folder", to), None);
        assert_eq!(map("C:\\Folder\\..\\secret", "C:\\Folder", to), None);
        assert_eq!(common_folder(&records[..2]), "C:\\Folder");
    }
}
