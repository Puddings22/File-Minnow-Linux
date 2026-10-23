use crate::{
    index::Snapshot,
    table::{MappedSlice, Row, Table},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use memmap2::Mmap;
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufWriter, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn default_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("XDG_CACHE_HOME").filter(|p| Path::new(p).is_absolute()) {
        PathBuf::from(p).join("file-minnow")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
            .join(".cache/file-minnow")
    }
}
pub fn prepare(dir: &Path) -> Result<PathBuf> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::canonicalize(dir).context("Cannot resolve data directory")
}
/// Where a socket for `dir` lives. Unix socket paths are limited to about
/// 107 bytes, so long data directories put their sockets in the user's
/// private runtime directory instead, under a name derived from the path.
pub fn socket_path(dir: &Path, name: &str) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let direct = dir.join(name);
    if direct.as_os_str().len() < 100 {
        return direct;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    else {
        return direct;
    };
    let hash = dir
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        });
    let folder = runtime.join("file-minnow");
    let _ = DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder);
    folder.join(format!("{hash:016x}-{name}"))
}
pub fn writer_lock(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(dir.join("writer.lock"))?;
    file.try_lock_exclusive().context("Another index writer is running for this data directory. Use a different --data-dir or stop it first")?;
    Ok(file)
}
const INDEX: &str = "index.bin";
const MAGIC: &[u8; 8] = b"FMNIDX07";
/// Older layouts are a cache; they are rebuilt instead of read.
const OLD_MAGICS: [&[u8; 8]; 5] = [
    b"FMNIDX02",
    b"FMNIDX03",
    b"FMNIDX04",
    b"FMNIDX05",
    b"FMNIDX06",
];
const ROW_BYTES: usize = std::mem::size_of::<Row>();
const DIR_NODE_BYTES: usize = std::mem::size_of::<crate::table::DirNode>();
const ITEM_BYTES: usize = 8;
/// Offsets and lengths of the five mapped sections.
const TABLE_VALUES: usize = 10;
/// Files from older versions. They are only a cache, so they are deleted.
const LEGACY: [&str; 4] = [
    "index.sqlite",
    "index.sqlite-journal",
    "index.sqlite-wal",
    "index.sqlite-shm",
];

/// Removes caches written by earlier versions (the first preview used a
/// SQLite file that grew to hundreds of megabytes). Caller holds the writer lock.
pub fn remove_legacy(dir: &Path) -> Result<bool> {
    let mut removed = false;
    for name in LEGACY {
        match fs::remove_file(dir.join(name)) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(removed)
}

/// Preserve an unreadable cache for inspection before rebuilding, keeping
/// only the most recent copy so repeated failures cannot fill the disk.
/// Caller must hold the writer lock.
pub fn quarantine(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        if name.to_string_lossy().starts_with("index.bin.corrupt-") {
            fs::remove_file(dir.join(&name))?;
        }
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = dir.join(INDEX);
    if path.exists() {
        fs::rename(path, dir.join(format!("{INDEX}.corrupt-{suffix}")))?;
    }
    Ok(())
}

/// A fast streaming checksum; it detects truncation and corruption, not
/// deliberate tampering (the file is private to the user).
struct Checksum(xxhash_rust::xxh3::Xxh3);
impl Checksum {
    fn new() -> Self {
        Self(xxhash_rust::xxh3::Xxh3::new())
    }
    fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    fn digest(&self) -> u64 {
        self.0.digest()
    }
}

struct Writer<W: Write> {
    out: W,
    sum: Checksum,
    pending: Vec<u8>,
}
impl<W: Write> Writer<W> {
    fn new(out: W) -> Self {
        Self {
            out,
            sum: Checksum::new(),
            pending: Vec::with_capacity(64 * 1024),
        }
    }
    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= 64 * 1024 {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            self.sum.update(&self.pending);
            self.out.write_all(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
    fn u64(&mut self, value: u64) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
}

const ZEROS: [u8; 8] = [0; 8];
fn pad_to<W: Write>(writer: &mut Writer<W>, at: &mut usize, target: usize) -> Result<()> {
    let count = target
        .checked_sub(*at)
        .context("Index layout is inconsistent")?;
    if count > ZEROS.len() {
        bail!("Index layout is inconsistent");
    }
    if count > 0 {
        writer.bytes(&ZEROS[..count])?;
        *at += count;
    }
    Ok(())
}
fn align8(value: usize) -> usize {
    (value + 7) & !7
}

/// Writes the whole snapshot to a temporary file and renames it into place,
/// so a crash leaves either the previous index or the new one, never a mix.
/// The file is laid out in aligned sections so it can be mapped on load.
pub fn save(dir: &Path, snapshot: &Snapshot) -> Result<()> {
    let target = dir.join(INDEX);
    let temp = dir.join(format!("{INDEX}.tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        let mut writer = Writer::new(BufWriter::with_capacity(1 << 20, file));
        let mut header = snapshot.header();
        header.saved_at = crate::index::now();
        let json = serde_json::to_vec(&header)?;
        let (names, dirs, rows) = snapshot.entries.parts();
        let items = snapshot.entries.item_entries();

        let table_off = align8(16 + json.len());
        let dir_nodes_off = table_off + TABLE_VALUES * 8;
        let dir_nodes_len = dirs.len() * DIR_NODE_BYTES;
        let dir_names_off = align8(dir_nodes_off + dir_nodes_len);
        let dir_names_len = dirs.names().len();
        let names_off = align8(dir_names_off + dir_names_len);
        let names_len = names.len();
        let rows_off = align8(names_off + names_len);
        let rows_len = rows.len() * ROW_BYTES;
        let items_off = align8(rows_off + rows_len);
        let items_len = items.len() * ITEM_BYTES;
        let checksum_off = align8(items_off + items_len);

        let mut at = 0;
        writer.out.write_all(MAGIC)?;
        at += MAGIC.len();
        writer.u64(json.len() as u64)?;
        at += 8;
        writer.bytes(&json)?;
        at += json.len();
        pad_to(&mut writer, &mut at, table_off)?;
        for value in [
            dir_nodes_off,
            dir_nodes_len,
            dir_names_off,
            dir_names_len,
            names_off,
            names_len,
            rows_off,
            rows_len,
            items_off,
            items_len,
        ] {
            writer.u64(value as u64)?;
            at += 8;
        }
        pad_to(&mut writer, &mut at, dir_nodes_off)?;
        let mut node = [0_u8; DIR_NODE_BYTES];
        for dir in dirs.nodes() {
            node[0..4].copy_from_slice(&dir.parent.to_le_bytes());
            node[4..8].copy_from_slice(&dir.name_off.to_le_bytes());
            node[8..10].copy_from_slice(&dir.name_len.to_le_bytes());
            node[10..12].copy_from_slice(&dir._pad.to_le_bytes());
            writer.bytes(&node)?;
            at += DIR_NODE_BYTES;
        }
        pad_to(&mut writer, &mut at, dir_names_off)?;
        writer.bytes(dirs.names())?;
        at += dir_names_len;
        pad_to(&mut writer, &mut at, names_off)?;
        writer.bytes(names)?;
        at += names_len;
        pad_to(&mut writer, &mut at, rows_off)?;
        let mut record = [0_u8; ROW_BYTES];
        for row in rows {
            record[0..8].copy_from_slice(&row.size.to_le_bytes());
            record[8..12].copy_from_slice(&row.modified.to_le_bytes());
            record[12..16].copy_from_slice(&row.created_raw().to_le_bytes());
            record[16..20].copy_from_slice(&row.name_offset().to_le_bytes());
            record[20..24].copy_from_slice(&row.dir_kind().to_le_bytes());
            writer.bytes(&record)?;
            at += ROW_BYTES;
        }
        pad_to(&mut writer, &mut at, items_off)?;
        let mut pair = [0_u8; ITEM_BYTES];
        for (row, count) in items {
            pair[0..4].copy_from_slice(&row.to_le_bytes());
            pair[4..8].copy_from_slice(&count.to_le_bytes());
            writer.bytes(&pair)?;
            at += ITEM_BYTES;
        }
        pad_to(&mut writer, &mut at, checksum_off)?;
        if at != checksum_off {
            bail!("Index layout is inconsistent");
        }
        writer.flush()?;
        let sum = writer.sum.digest();
        writer.out.write_all(&sum.to_le_bytes())?;
        let file = writer.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        fs::rename(&temp, &target)?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Loads the saved index, mapping its data sections instead of copying them.
/// A missing file is an empty index; an unreadable or inconsistent one is an
/// error so the caller can quarantine and rebuild it.
pub fn load(dir: &Path) -> Result<Snapshot> {
    let path = dir.join(INDEX);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Snapshot::default()),
        Err(e) => return Err(e.into()),
    };
    if file.metadata()?.len() < 8 {
        bail!("Index file is truncated");
    }
    // SAFETY: the index file is private to the user and is never written in
    // place; a replacement happens through rename.
    let map = unsafe { Mmap::map(&file)? };
    let magic: &[u8; 8] = map[0..8].try_into().unwrap();
    if OLD_MAGICS.contains(&magic) {
        // A previous version's cache: start empty, the next save replaces it.
        return Ok(Snapshot::default());
    }
    if magic != MAGIC {
        bail!("Unknown index format; it will be rebuilt");
    }
    if map.len() < 24 {
        bail!("Index file is truncated");
    }
    let header_len = u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;
    let header_end = 16_usize
        .checked_add(header_len)
        .context("Index file is inconsistent")?;
    if header_end > map.len() {
        bail!("Index file is truncated");
    }
    let mut snapshot: Snapshot = serde_json::from_slice(&map[16..header_end])?;
    let table_off = align8(header_end);
    let table_end = table_off
        .checked_add(TABLE_VALUES * 8)
        .context("Index file is inconsistent")?;
    if table_end > map.len() {
        bail!("Index file is truncated");
    }
    let mut values = [0_u64; TABLE_VALUES];
    for (index, value) in values.iter_mut().enumerate() {
        let start = table_off + index * 8;
        *value = u64::from_le_bytes(map[start..start + 8].try_into().unwrap());
    }
    let [
        dir_nodes_off,
        dir_nodes_len,
        dir_names_off,
        dir_names_len,
        names_off,
        names_len,
        rows_off,
        rows_len,
        items_off,
        items_len,
    ] = values.map(|value| value as usize);
    let checksum_off = map.len() - 8;
    let check = |offset: usize, len: usize, alignment: usize, multiple: usize| -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("Index file is inconsistent")?;
        if !offset.is_multiple_of(alignment) || !len.is_multiple_of(multiple) || end > checksum_off
        {
            bail!("Index file is inconsistent");
        }
        Ok(())
    };
    check(dir_nodes_off, dir_nodes_len, 4, DIR_NODE_BYTES)?;
    check(dir_names_off, dir_names_len, 1, 1)?;
    check(names_off, names_len, 1, 1)?;
    check(rows_off, rows_len, 8, ROW_BYTES)?;
    check(items_off, items_len, 4, ITEM_BYTES)?;

    let mut checksum = Checksum::new();
    checksum.update(&map[8..checksum_off]);
    let stored = u64::from_le_bytes(map[checksum_off..].try_into().unwrap());
    if checksum.digest() != stored {
        bail!("Index checksum mismatch; it will be rebuilt");
    }

    let row_count = rows_len / ROW_BYTES;
    let mut items = Vec::with_capacity(items_len / ITEM_BYTES);
    // `as_chunks` needs a newer MSRV than the rest of the crate.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    for chunk in map[items_off..items_off + items_len].chunks_exact(ITEM_BYTES) {
        let row = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
        let count = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        if row as usize >= row_count || items.last().is_some_and(|(last, _)| *last >= row) {
            bail!("Index file is inconsistent");
        }
        items.push((row, count));
    }

    let map = Arc::new(map);
    let names = MappedSlice::new(map.clone(), names_off, names_len);
    let dir_names = MappedSlice::new(map.clone(), dir_names_off, dir_names_len);
    let dir_nodes = MappedSlice::new(map.clone(), dir_nodes_off, dir_nodes_len);
    let rows = MappedSlice::new(map.clone(), rows_off, rows_len);
    let dirs = crate::table::Dirs::from_mapped(dir_names, dir_nodes)
        .context("Index file is inconsistent")?;
    snapshot.entries =
        Table::from_mapped(names, dirs, rows, items).context("Index file is inconsistent")?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Entry, Kind};

    fn snapshot() -> Snapshot {
        let entry = |path: &str, kind, size| Entry::new(path.as_bytes().to_vec(), kind, size, 5);
        Snapshot {
            roots: vec![b"/d".to_vec()],
            entries: Table::from_entries(vec![
                entry("/d", Kind::Folder, 0),
                entry("/d/a.txt", Kind::File, 10),
                entry("/d/sub", Kind::Folder, 0),
                entry("/d/sub/b.txt", Kind::File, 20),
            ]),
            generation: 3,
            ..Snapshot::default()
        }
    }

    #[test]
    fn round_trip_maps_the_index() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot();
        save(temp.path(), &snapshot).unwrap();
        let old = load(temp.path()).unwrap();
        assert_eq!(old.entries.entries(), snapshot.entries.entries());
        assert_eq!(old.entries.items(0), 3);
        assert_eq!(old.entries.row(0).size, 30);
        assert_eq!(old.generation, 3);
        old.entries.trim();
        assert_eq!(old.entries.entries(), snapshot.entries.entries());
    }

    #[test]
    fn corrupt_index_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        save(temp.path(), &snapshot()).unwrap();
        let path = temp.path().join(INDEX);
        let mut bytes = fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        fs::write(&path, &bytes).unwrap();
        assert!(load(temp.path()).is_err());

        fs::write(&path, &bytes[..bytes.len() - 4]).unwrap();
        assert!(load(temp.path()).is_err());
    }

    #[test]
    fn old_cache_is_treated_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(INDEX), b"FMNIDX04-old-cache").unwrap();
        let snapshot = load(temp.path()).unwrap();
        assert!(snapshot.entries.is_empty());
    }
}
