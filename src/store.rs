use crate::{
    index::{Kind, Snapshot},
    table::{Dirs, NO_DIR, Row, Table},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
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
const MAGIC: &[u8; 8] = b"FMNIDX02";
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

/// A fast, order-sensitive checksum; it detects truncation and corruption,
/// not deliberate tampering (the file is private to the user).
struct Checksum(u64);
impl Checksum {
    fn update(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let word = u64::from_le_bytes(chunk.try_into().unwrap());
            self.0 = (self.0 ^ word)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .rotate_left(27);
        }
        for &byte in chunks.remainder() {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3);
        }
        self.0 ^= bytes.len() as u64;
    }
}

struct Writer<W: Write> {
    out: W,
    sum: Checksum,
}
impl<W: Write> Writer<W> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.sum.update(bytes);
        self.out.write_all(bytes)?;
        Ok(())
    }
    fn u64(&mut self, value: u64) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
    fn block(&mut self, bytes: &[u8]) -> Result<()> {
        self.u64(bytes.len() as u64)?;
        self.bytes(bytes)
    }
}

struct Reader<R: Read> {
    input: R,
    sum: Checksum,
    remaining: u64,
}
impl<R: Read> Reader<R> {
    fn bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        if len > self.remaining {
            bail!("Index file is truncated");
        }
        let mut buffer = vec![0; len as usize];
        self.input.read_exact(&mut buffer)?;
        self.remaining -= len;
        self.sum.update(&buffer);
        Ok(buffer)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn block(&mut self) -> Result<Vec<u8>> {
        let len = self.u64()?;
        self.bytes(len)
    }
}

const ROW_BYTES: usize = 27;
fn kind_byte(kind: Kind) -> u8 {
    match kind {
        Kind::File => 0,
        Kind::Folder => 1,
        Kind::Link => 2,
        Kind::Other => 3,
    }
}

/// Writes the whole snapshot to a temporary file and renames it into place,
/// so a crash leaves either the previous index or the new one, never a mix.
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
        let mut w = Writer {
            out: BufWriter::with_capacity(1 << 20, file),
            sum: Checksum(0),
        };
        w.out.write_all(MAGIC)?;
        w.block(&serde_json::to_vec(&snapshot.header())?)?;
        let (names, dirs, rows) = snapshot.entries.parts();
        w.u64(dirs.len() as u64)?;
        for dir in dirs.iter() {
            w.block(dir)?;
        }
        w.block(names)?;
        w.u64(rows.len() as u64)?;
        let mut record = [0_u8; ROW_BYTES];
        for row in rows {
            let (offset, len) = row.name_range();
            record[0..8].copy_from_slice(&row.size.to_le_bytes());
            record[8..16].copy_from_slice(&row.modified.to_le_bytes());
            record[16..20].copy_from_slice(&offset.to_le_bytes());
            record[20..24].copy_from_slice(&row.dir.to_le_bytes());
            record[24..26].copy_from_slice(&len.to_le_bytes());
            record[26] = kind_byte(row.kind);
            w.bytes(&record)?;
        }
        let sum = w.sum.0;
        w.out.write_all(&sum.to_le_bytes())?;
        let file = w.out.into_inner().map_err(|e| e.into_error())?;
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

/// Loads the saved index. A missing file is an empty index; an unreadable or
/// inconsistent one is an error so the caller can quarantine and rebuild it.
pub fn load(dir: &Path) -> Result<Snapshot> {
    let path = dir.join(INDEX);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Snapshot::default()),
        Err(e) => return Err(e.into()),
    };
    let size = file.metadata()?.len();
    let mut input = BufReader::with_capacity(1 << 20, file);
    let mut magic = [0_u8; 8];
    input
        .read_exact(&mut magic)
        .context("Index file is truncated")?;
    if &magic != MAGIC {
        bail!("Unknown index format; it will be rebuilt");
    }
    let mut r = Reader {
        input,
        sum: Checksum(0),
        remaining: size.saturating_sub(16),
    };
    let mut snapshot: Snapshot = serde_json::from_slice(&r.block()?)?;
    let dir_count = r.u64()?;
    if dir_count.saturating_mul(8) > r.remaining || dir_count >= u64::from(NO_DIR) {
        bail!("Index file is inconsistent");
    }
    let mut dirs = Dirs::default();
    for _ in 0..dir_count {
        if !dirs.push(&r.block()?) {
            bail!("Index file is inconsistent");
        }
    }
    let names = r.block()?;
    let row_count = r.u64()?;
    if row_count.saturating_mul(ROW_BYTES as u64) != r.remaining {
        bail!("Index file is inconsistent");
    }
    let mut rows = Vec::with_capacity(row_count as usize);
    let mut next_name = 0_u32;
    // Read rows in large blocks; the checksum still covers each record.
    let mut block = vec![0_u8; ROW_BYTES * 65_536];
    let mut left = row_count as usize;
    while left > 0 {
        let count = left.min(65_536);
        let block = &mut block[..count * ROW_BYTES];
        r.input
            .read_exact(block)
            .context("Index file is truncated")?;
        r.remaining -= block.len() as u64;
        left -= count;
        for record in block.chunks_exact(ROW_BYTES) {
            r.sum.update(record);
            let field = |range: std::ops::Range<usize>| &record[range];
            let size = u64::from_le_bytes(field(0..8).try_into().unwrap());
            let modified = i64::from_le_bytes(field(8..16).try_into().unwrap());
            let offset = u32::from_le_bytes(field(16..20).try_into().unwrap());
            let dir = u32::from_le_bytes(field(20..24).try_into().unwrap());
            let len = u16::from_le_bytes(field(24..26).try_into().unwrap());
            let kind = match record[26] {
                0 => Kind::File,
                1 => Kind::Folder,
                2 => Kind::Link,
                3 => Kind::Other,
                _ => bail!("Index file is inconsistent"),
            };
            // Names are stored back to back in row order; searches rely on it.
            if offset != next_name
                || offset as usize + len as usize > names.len()
                || (dir != NO_DIR && dir as usize >= dirs.len())
            {
                bail!("Index file is inconsistent");
            }
            next_name = offset + u32::from(len);
            rows.push(Row::from_raw(size, modified, offset, dir, len, kind));
        }
    }
    if next_name as usize != names.len() {
        bail!("Index file is inconsistent");
    }
    let expected = r.sum.0;
    let mut stored = [0_u8; 8];
    r.input
        .read_exact(&mut stored)
        .context("Index file is truncated")?;
    if u64::from_le_bytes(stored) != expected {
        bail!("Index checksum mismatch; it will be rebuilt");
    }
    snapshot.entries = Table::from_parts(names, dirs, rows);
    Ok(snapshot)
}
