//! Compact in-memory index.
//!
//! Each parent directory path is stored once and file names share one byte
//! buffer, instead of keeping a separate full path for every entry. Rows are
//! kept in component-wise path order: a depth-first walk with siblings sorted
//! by name. That makes sorting by path free, keeps every subtree in one
//! contiguous range, and lets the builder deduplicate parent directories with
//! a stack of the current ancestors instead of a hash map.
use crate::index::{Entry, Kind};
use bytemuck::{Pod, Zeroable};
use memmap2::Mmap;
use std::{
    cmp::Ordering,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// Names buffer, parent folder paths and rows, for storage and scanning.
pub(crate) type Parts<'a> = (&'a [u8], &'a Dirs, &'a [Row]);

/// Parent index of entries whose path has no parent, such as `/`.
pub const NO_DIR: u32 = u32::MAX;

/// Unknown creation time (the filesystem does not record one).
pub const NO_DATE: i64 = i64::MIN;
/// Creation time that does not fit in the packed row, or is unknown.
const NO_DATE_U32: u32 = u32::MAX;
/// Directory ids use the low 30 bits of `dir_kind`; the top two hold `Kind`.
const DIR_BITS: u32 = 30;
const DIR_MASK: u32 = (1 << DIR_BITS) - 1;

/// A read-only slice of the mmap-backed index file.
#[derive(Clone)]
pub(crate) struct MappedSlice {
    map: Arc<Mmap>,
    start: usize,
    len: usize,
}
impl MappedSlice {
    pub(crate) fn new(map: Arc<Mmap>, start: usize, len: usize) -> Self {
        Self { map, start, len }
    }
    fn bytes(&self) -> &[u8] {
        &self.map[self.start..self.start + self.len]
    }
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    fn trim(&self) {
        // SAFETY: the mapping is page-aligned and `len` is the mapping length.
        // Discarding clean file-backed pages only makes them fault in again.
        unsafe {
            libc::madvise(
                self.map.as_ptr() as *mut libc::c_void,
                self.map.len(),
                libc::MADV_DONTNEED,
            );
        }
    }
}
impl std::fmt::Debug for MappedSlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedSlice")
            .field("start", &self.start)
            .field("len", &self.len)
            .finish()
    }
}

/// A byte buffer that is either owned by the builder or mapped from the
/// saved index.
#[derive(Clone, Debug)]
pub(crate) enum Blob {
    Owned(Vec<u8>),
    Mapped(MappedSlice),
}
impl Default for Blob {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}
impl Blob {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped(mapped) => mapped.bytes(),
        }
    }
    fn as_mut_vec(&mut self) -> &mut Vec<u8> {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped(_) => panic!("mapped index is immutable"),
        }
    }
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.capacity(),
            Self::Mapped(_) => 0,
        }
    }
    fn shrink_to_fit(&mut self) {
        if let Self::Owned(bytes) = self {
            bytes.shrink_to_fit();
        }
    }
    fn trim(&self) {
        if let Self::Mapped(mapped) = self {
            mapped.trim();
        }
    }
}
impl Deref for Blob {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}
impl DerefMut for Blob {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_vec()
    }
}

/// Rows that are either built in memory or mapped from the saved index.
#[derive(Clone, Debug)]
pub(crate) enum RowStore {
    Owned(Vec<Row>),
    Mapped(MappedSlice),
}
impl Default for RowStore {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}
impl RowStore {
    fn as_slice(&self) -> &[Row] {
        match self {
            Self::Owned(rows) => rows,
            Self::Mapped(mapped) => bytemuck::cast_slice(mapped.bytes()),
        }
    }
    fn as_mut_slice(&mut self) -> &mut [Row] {
        match self {
            Self::Owned(rows) => rows,
            Self::Mapped(_) => panic!("mapped index is immutable"),
        }
    }
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Owned(rows) => rows.capacity() * std::mem::size_of::<Row>(),
            Self::Mapped(_) => 0,
        }
    }
    fn trim(&self) {
        if let Self::Mapped(mapped) = self {
            mapped.trim();
        }
    }
}
impl Deref for RowStore {
    type Target = [Row];
    fn deref(&self) -> &[Row] {
        self.as_slice()
    }
}
impl DerefMut for RowStore {
    fn deref_mut(&mut self) -> &mut [Row] {
        self.as_mut_slice()
    }
}

/// Directory nodes that are either built in memory or mapped from the file.
#[derive(Clone, Debug)]
pub(crate) enum DirStore {
    Owned(Vec<DirNode>),
    Mapped(MappedSlice),
}
impl Default for DirStore {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}
impl DirStore {
    fn as_slice(&self) -> &[DirNode] {
        match self {
            Self::Owned(nodes) => nodes,
            Self::Mapped(mapped) => bytemuck::cast_slice(mapped.bytes()),
        }
    }
    fn as_mut_slice(&mut self) -> &mut [DirNode] {
        match self {
            Self::Owned(nodes) => nodes,
            Self::Mapped(_) => panic!("mapped index is immutable"),
        }
    }
    fn as_mut_vec(&mut self) -> &mut Vec<DirNode> {
        match self {
            Self::Owned(nodes) => nodes,
            Self::Mapped(_) => panic!("mapped index is immutable"),
        }
    }
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Owned(nodes) => nodes.capacity() * std::mem::size_of::<DirNode>(),
            Self::Mapped(_) => 0,
        }
    }
    fn shrink_to_fit(&mut self) {
        if let Self::Owned(nodes) = self {
            nodes.shrink_to_fit();
        }
    }
    fn trim(&self) {
        if let Self::Mapped(mapped) = self {
            mapped.trim();
        }
    }
}
impl Deref for DirStore {
    type Target = [DirNode];
    fn deref(&self) -> &[DirNode] {
        self.as_slice()
    }
}
impl DerefMut for DirStore {
    fn deref_mut(&mut self) -> &mut [DirNode] {
        self.as_mut_slice()
    }
}

// A row is 24 bytes: size, modified, created, name offset and dir/kind.
const _: () = assert!(std::mem::size_of::<Row>() == 24);
const _: () = assert!(std::mem::size_of::<DirNode>() == 12);
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct Row {
    /// File size, or for a folder the total size of the indexed files
    /// inside it (computed by the builder).
    pub size: u64,
    pub modified: u32,
    created: u32,
    name_offset: u32,
    dir_kind: u32,
}
impl Row {
    pub(crate) fn new(
        size: u64,
        modified: i64,
        created: i64,
        name_offset: u32,
        dir: u32,
        kind: Kind,
    ) -> Self {
        Self {
            size,
            modified: clamp_time(modified),
            created: if created == NO_DATE {
                NO_DATE_U32
            } else {
                clamp_time(created)
            },
            name_offset,
            dir_kind: encode_dir_kind(dir, kind),
        }
    }
    pub fn dir(&self) -> u32 {
        let dir = self.dir_kind & DIR_MASK;
        if dir == DIR_MASK { NO_DIR } else { dir }
    }
    pub fn kind(&self) -> Kind {
        Kind::from_bits(self.dir_kind >> DIR_BITS)
    }
    pub fn created(&self) -> i64 {
        if self.created == NO_DATE_U32 {
            NO_DATE
        } else {
            i64::from(self.created)
        }
    }
    pub(crate) fn name_offset(&self) -> u32 {
        self.name_offset
    }
    pub(crate) fn dir_kind(&self) -> u32 {
        self.dir_kind
    }
    pub(crate) fn created_raw(&self) -> u32 {
        self.created
    }
}
fn clamp_time(value: i64) -> u32 {
    if value <= 0 {
        0
    } else {
        value.min(i64::from(u32::MAX)) as u32
    }
}
fn encode_dir_kind(dir: u32, kind: Kind) -> u32 {
    let dir = if dir == NO_DIR { DIR_MASK } else { dir };
    (kind.bits() << DIR_BITS) | dir
}

/// One directory in the parent-linked directory table. A root directory
/// (`parent == NO_DIR`) stores its whole path as its name; every other node
/// stores one component and is joined to its parent when a path is built.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct DirNode {
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub(crate) _pad: u16,
}

/// Directory names packed into one buffer plus one parent-linked node per
/// directory (three allocations in total, instead of one per folder).
#[derive(Clone, Debug, Default)]
pub struct Dirs {
    names: Blob,
    nodes: DirStore,
}
impl Dirs {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn name(&self, i: usize) -> &[u8] {
        let node = &self.nodes[i];
        let start = node.name_off as usize;
        &self.names[start..start + node.name_len as usize]
    }
    pub fn parent(&self, i: usize) -> u32 {
        self.nodes[i].parent
    }
    pub fn nodes(&self) -> &[DirNode] {
        self.nodes.as_slice()
    }
    pub fn names(&self) -> &[u8] {
        self.names.as_slice()
    }
    /// Writes the full path of directory `i` into `out`, replacing its contents.
    pub fn write_path(&self, i: usize, out: &mut Vec<u8>) {
        out.clear();
        let mut chain = Vec::with_capacity(16);
        let mut at = i as u32;
        loop {
            chain.push(at);
            let parent = self.nodes[at as usize].parent;
            if parent == NO_DIR {
                break;
            }
            at = parent;
        }
        for &node in chain.iter().rev() {
            let name = self.name(node as usize);
            if !out.is_empty() && !out.ends_with(b"/") && !name.starts_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(name);
        }
    }
    pub fn path(&self, i: usize) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_path(i, &mut out);
        out
    }
    /// Finds or adds `dir`, using the scanner's stack of open ancestors so
    /// unchanged path prefixes are shared instead of stored again.
    pub(crate) fn intern(&mut self, ancestors: &mut Vec<(Vec<u8>, u32)>, dir: &[u8]) -> u32 {
        let mut top = ancestors.len();
        while top > 0 {
            let (open, id) = &ancestors[top - 1];
            if open.as_slice() == dir {
                return *id;
            }
            if within(dir, open) {
                break;
            }
            top -= 1;
        }
        ancestors.truncate(top);
        if let Some((open, id)) = ancestors.last() {
            let mut full = open.clone();
            let mut parent = *id;
            let rest = dir[open.len()..]
                .strip_prefix(b"/")
                .unwrap_or(&dir[open.len()..]);
            for component in rest.split(|byte| *byte == b'/') {
                if component.is_empty() {
                    continue;
                }
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(component);
                let Some(id) = self.push(parent, component) else {
                    return parent;
                };
                parent = id;
                ancestors.push((full.clone(), parent));
            }
            parent
        } else {
            let Some(id) = self.push(NO_DIR, dir) else {
                return NO_DIR;
            };
            ancestors.push((dir.to_vec(), id));
            id
        }
    }
    pub(crate) fn from_mapped(names: MappedSlice, nodes: MappedSlice) -> Option<Self> {
        if !nodes.len().is_multiple_of(std::mem::size_of::<DirNode>())
            || !(nodes.bytes().as_ptr() as usize).is_multiple_of(std::mem::align_of::<DirNode>())
        {
            return None;
        }
        let node_slice: &[DirNode] = bytemuck::cast_slice(nodes.bytes());
        for (index, node) in node_slice.iter().enumerate() {
            if u64::from(node.name_off) + u64::from(node.name_len) > names.len() as u64
                || (node.parent != NO_DIR && node.parent as usize >= index)
            {
                return None;
            }
        }
        Some(Self {
            names: Blob::Mapped(names),
            nodes: DirStore::Mapped(nodes),
        })
    }
    fn push(&mut self, parent: u32, name: &[u8]) -> Option<u32> {
        let names = self.names.as_mut_vec();
        let nodes = self.nodes.as_mut_vec();
        if name.len() > u16::MAX as usize
            || names.len() + name.len() > u32::MAX as usize
            || nodes.len() >= DIR_MASK as usize
        {
            return None;
        }
        let id = nodes.len() as u32;
        let name_off = names.len() as u32;
        nodes.push(DirNode {
            parent,
            name_off,
            name_len: name.len() as u16,
            _pad: 0,
        });
        names.extend_from_slice(name);
        Some(id)
    }
    fn shrink_to_fit(&mut self) {
        self.names.shrink_to_fit();
        self.nodes.shrink_to_fit();
    }
    fn heap_bytes(&self) -> usize {
        self.names.heap_bytes() + self.nodes.heap_bytes()
    }
    fn trim(&self) {
        self.names.trim();
        self.nodes.trim();
    }
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    names: Blob,
    dirs: Dirs,
    rows: RowStore,
    /// Folder item counts only: `(row, items)`, sorted by row.
    items: Vec<(u32, u32)>,
    name_order: OnceLock<Vec<u32>>,
    folded: OnceLock<Folded>,
}

/// Folded forms (see `fold`) of the names that contain non-ASCII bytes,
/// stored back to back like the names buffer, with the row of each.
#[derive(Clone, Debug, Default)]
pub struct Folded {
    pub bytes: Vec<u8>,
    pub ends: Vec<u32>,
    pub rows: Vec<u32>,
}
impl Folded {
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub fn range(&self, k: usize) -> (usize, usize) {
        let start = if k == 0 { 0 } else { self.ends[k - 1] as usize };
        (start, self.ends[k] as usize)
    }
    pub fn get(&self, k: usize) -> &[u8] {
        let (start, end) = self.range(k);
        &self.bytes[start..end]
    }
}

/// Component-wise byte order: `/` sorts before every other byte, so a folder's
/// descendants come right after it and before its next sibling.
pub fn path_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let key = |c: u8| if c == b'/' { 0_u16 } else { u16::from(c) + 1 };
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return key(*x).cmp(&key(*y));
        }
    }
    a.len().cmp(&b.len())
}

/// Appends a name's natural sort key, whose byte order is the natural name
/// order: ASCII letters lowercased, and each run of digits replaced by its
/// length without leading zeros, then those digits, so `2` sorts before
/// `10`. Lengths of 1 to 8 are the bytes `1` to `8`; longer runs are `9`
/// and a length byte. Digits keep their place among other characters this
/// way, and key bytes are never zero, so a zero-padded prefix stands for
/// the key. `name` must be ASCII or already folded (`natural_source`).
fn push_natural_key(name: &[u8], out: &mut Vec<u8>) {
    let mut at = 0;
    while at < name.len() {
        let c = name[at];
        if !c.is_ascii_digit() {
            out.push(c.to_ascii_lowercase());
            at += 1;
            continue;
        }
        let (start, end) = digit_run(name, at);
        let first = run_first_byte(name, at);
        out.push(first);
        if first == b'9' {
            out.push(run_length(start, end) as u8);
        }
        out.extend_from_slice(&name[start..end]);
        at = end;
    }
}
/// The run of digits starting at `at`: the digits without leading zeros
/// (keeping at least one) and the end of the run.
fn digit_run(name: &[u8], at: usize) -> (usize, usize) {
    let end = at + name[at..].iter().take_while(|b| b.is_ascii_digit()).count();
    let mut start = at;
    while start + 1 < end && name[start] == b'0' {
        start += 1;
    }
    (start, end)
}
fn run_length(start: usize, end: usize) -> usize {
    (end - start).min(255)
}
/// The first key byte of the run of digits at `at`.
fn run_first_byte(name: &[u8], at: usize) -> u8 {
    let (start, end) = digit_run(name, at);
    match run_length(start, end) {
        length @ 1..=8 => b'0' + length as u8,
        _ => b'9',
    }
}
/// A name in the form its natural key is built from: non-ASCII names are
/// folded, so accents and case do not change where they sort.
fn natural_source(name: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if name.is_ascii() {
        std::borrow::Cow::Borrowed(name)
    } else {
        std::borrow::Cow::Owned(crate::fold::fold(name))
    }
}
fn natural_key(name: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(name.len() + 8);
    push_natural_key(&natural_source(name), &mut key);
    key
}
/// Compares the natural keys of two names without building them. ASCII
/// text is compared in place; at the first non-ASCII byte the keys of both
/// (folded) names are built and compared.
fn natural_key_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    loop {
        let (Some(&x), Some(&y)) = (a.get(i), b.get(j)) else {
            return (a.len() - i).min(1).cmp(&(b.len() - j).min(1));
        };
        if !x.is_ascii() || !y.is_ascii() {
            return natural_key(a).cmp(&natural_key(b));
        }
        match (x.is_ascii_digit(), y.is_ascii_digit()) {
            (true, true) => {
                let (sa, ea) = digit_run(a, i);
                let (sb, eb) = digit_run(b, j);
                let order = run_length(sa, ea)
                    .cmp(&run_length(sb, eb))
                    .then_with(|| a[sa..ea].cmp(&b[sb..eb]));
                if order.is_ne() {
                    return order;
                }
                (i, j) = (ea, eb);
            }
            // A run's first key byte is a digit, compared with the other
            // name's (non-digit) character.
            (true, false) => return run_first_byte(a, i).cmp(&y.to_ascii_lowercase()),
            (false, true) => return x.to_ascii_lowercase().cmp(&run_first_byte(b, j)),
            (false, false) => {
                let order = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if order.is_ne() {
                    return order;
                }
                (i, j) = (i + 1, j + 1);
            }
        }
    }
}
/// Natural name order, as file managers sort: case- and accent-insensitive,
/// with numbers compared by value (`IMG_2` before `IMG_10`). Names that only
/// differ in case, accents or leading zeros fall back to their bytes, so the
/// order is total and identical names stay next to each other.
pub fn name_cmp(a: &[u8], b: &[u8]) -> Ordering {
    // Skip the identical start, backing up to an ASCII character that is
    // not a digit: a run of digits is compared as a whole number, and a
    // multi-byte character is folded as a whole.
    let mut same = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    if same == a.len() && same == b.len() {
        return Ordering::Equal;
    }
    while same > 0 && (a[same - 1].is_ascii_digit() || !a[same - 1].is_ascii()) {
        same -= 1;
    }
    natural_key_cmp(&a[same..], &b[same..]).then_with(|| a.cmp(b))
}
/// The first eight bytes of a natural key, zero-padded.
fn key_prefix(key: &[u8]) -> u64 {
    let mut prefix = [0_u8; 8];
    let n = key.len().min(8);
    prefix[..n].copy_from_slice(&key[..n]);
    u64::from_be_bytes(prefix)
}

/// `path` equals `root` or lies below it.
pub fn within(path: &[u8], root: &[u8]) -> bool {
    path.starts_with(root)
        && (path.len() == root.len() || root.ends_with(b"/") || path[root.len()] == b'/')
}

fn split(path: &[u8]) -> (Option<&[u8]>, &[u8]) {
    match path.iter().rposition(|b| *b == b'/') {
        Some(_) if path == b"/" => (None, path),
        Some(0) => (Some(&path[..1]), &path[1..]),
        Some(i) => (Some(&path[..i]), &path[i + 1..]),
        None => (None, path),
    }
}

impl Table {
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub fn row(&self, i: usize) -> &Row {
        &self.rows[i]
    }
    pub fn name(&self, i: usize) -> &[u8] {
        let start = self.rows[i].name_offset as usize;
        let end = if i + 1 < self.rows.len() {
            self.rows[i + 1].name_offset as usize
        } else {
            self.names.len()
        };
        &self.names[start..end]
    }
    /// Number of bytes in row `i`'s name.
    pub fn name_len(&self, i: usize) -> usize {
        self.name(i).len()
    }
    /// Items inside folder row `i`; zero for every other row.
    pub fn items(&self, i: usize) -> u32 {
        self.items
            .binary_search_by_key(&(i as u32), |(row, _)| *row)
            .map_or(0, |at| self.items[at].1)
    }
    /// Appends the parent folder path of row `i` to `out`. Returns false when
    /// the row has no parent (a filesystem root).
    pub fn write_dir_path(&self, i: usize, out: &mut Vec<u8>) -> bool {
        let dir = self.rows[i].dir();
        out.clear();
        if dir == NO_DIR {
            return false;
        }
        self.dirs.write_path(dir as usize, out);
        true
    }
    pub fn dir_path(&self, i: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.write_dir_path(i, &mut out).then_some(out)
    }
    pub fn dirs(&self) -> &Dirs {
        &self.dirs
    }
    /// Writes the full path of row `i` into `out`, replacing its contents.
    pub fn write_path(&self, i: usize, out: &mut Vec<u8>) {
        out.clear();
        let dir = self.rows[i].dir();
        if dir != NO_DIR {
            self.dirs.write_path(dir as usize, out);
            if !out.ends_with(b"/") {
                out.push(b'/');
            }
        }
        out.extend_from_slice(self.name(i));
    }
    pub fn path(&self, i: usize) -> Vec<u8> {
        let mut path = Vec::new();
        self.write_path(i, &mut path);
        path
    }
    pub fn entry(&self, i: usize) -> Entry {
        let row = &self.rows[i];
        let mut entry = Entry::new(self.path(i), row.kind(), row.size, i64::from(row.modified))
            .created(row.created());
        entry.items = self.items(i);
        entry
    }
    /// Whether folder row `i` has no indexed entries inside it.
    pub fn is_empty_folder(&self, i: usize) -> bool {
        if self.rows[i].kind() != Kind::Folder {
            return false;
        }
        let Some(next) = self.rows.get(i + 1) else {
            return true;
        };
        let dir = next.dir();
        if dir == NO_DIR {
            return true;
        }
        let mut path = Vec::new();
        self.write_path(i, &mut path);
        let mut dir_path = Vec::new();
        self.dirs.write_path(dir as usize, &mut dir_path);
        dir_path != path
    }
    /// For each parent folder path (by `dir` id), the row of that folder, or
    /// `u32::MAX` when the folder itself is not indexed (a root's parent).
    pub fn dir_rows(&self) -> Vec<u32> {
        let mut dir_rows = vec![u32::MAX; self.dirs.len()];
        let mut path = Vec::new();
        let mut dir_path = Vec::new();
        for j in 1..self.rows.len() {
            let dir = self.rows[j].dir();
            // The first child follows its folder directly in path order, so
            // only that child needs the comparison.
            if dir != NO_DIR
                && dir_rows[dir as usize] == u32::MAX
                && self.rows[j - 1].kind() == Kind::Folder
            {
                self.write_path(j - 1, &mut path);
                self.dirs.write_path(dir as usize, &mut dir_path);
                if dir_path == path {
                    dir_rows[dir as usize] = (j - 1) as u32;
                }
            }
        }
        dir_rows
    }
    /// Materializes every entry. Intended for tests and small tools only.
    pub fn entries(&self) -> Vec<Entry> {
        (0..self.len()).map(|i| self.entry(i)).collect()
    }
    pub fn iter(&self) -> impl Iterator<Item = EntryRef<'_>> + '_ {
        (0..self.len()).map(move |index| EntryRef {
            table: self,
            index,
            kind: self.rows[index].kind(),
            size: self.rows[index].size,
            modified: i64::from(self.rows[index].modified),
            created: self.rows[index].created(),
        })
    }
    pub fn from_entries(mut entries: Vec<Entry>) -> Self {
        entries.sort_by(|a, b| path_cmp(&a.path, &b.path));
        entries.dedup_by(|a, b| a.path == b.path);
        let mut builder = Builder::with_capacity(entries.len());
        for e in &entries {
            builder.push_with(&e.path, e.kind, e.size, e.modified, e.created);
        }
        builder.finish()
    }
    /// Approximate heap bytes held by the table, for status reporting.
    pub fn heap_bytes(&self) -> usize {
        self.names.heap_bytes()
            + self.rows.heap_bytes()
            + self.items.capacity() * std::mem::size_of::<(u32, u32)>()
            + self.dirs.heap_bytes()
            + self.name_order.get().map_or(0, |o| o.capacity() * 4)
            + self.folded.get().map_or(0, |f| {
                f.bytes.capacity() + f.ends.capacity() * 4 + f.rows.capacity() * 4
            })
    }
    /// Drops clean mapped pages; the next query faults them back in.
    pub fn trim(&self) {
        self.names.trim();
        self.dirs.trim();
        self.rows.trim();
    }
    /// First row whose path is not before `path` in component order.
    pub fn lower_bound(&self, path: &[u8]) -> usize {
        let mut scratch = Vec::new();
        let (mut low, mut high) = (0, self.rows.len());
        while low < high {
            let mid = (low + high) / 2;
            self.write_path(mid, &mut scratch);
            if path_cmp(&scratch, path) == Ordering::Less {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }
    /// End of the contiguous range of rows at or below `root`, starting at
    /// `start` (normally `lower_bound(root)`).
    pub fn subtree_end(&self, start: usize, root: &[u8]) -> usize {
        let mut scratch = Vec::new();
        let (mut low, mut high) = (start, self.rows.len());
        while low < high {
            let mid = (low + high) / 2;
            self.write_path(mid, &mut scratch);
            if within(&scratch, root) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }
    /// Row indices in natural name order (`name_cmp`), ties in path order.
    /// Built once per table.
    pub fn name_order(&self) -> &[u32] {
        self.name_order.get_or_init(|| {
            // Every name's natural key, built once and compared as bytes.
            // Every name's natural key, built once and compared as bytes.
            // Lower-case ASCII names without digits are their own key.
            let mut keys = Vec::new();
            let mut spans: Vec<(u32, u32)> = Vec::with_capacity(self.len());
            for i in 0..self.len() {
                let name = self.name(i);
                if name
                    .iter()
                    .all(|b| b.is_ascii() && !b.is_ascii_digit() && !b.is_ascii_uppercase())
                {
                    spans.push((u32::MAX, 0));
                    continue;
                }
                let start = keys.len();
                push_natural_key(&natural_source(name), &mut keys);
                // Keys are about as long as the names, which fit in 4 GiB.
                spans.push((start as u32, (keys.len() - start) as u32));
            }
            let own_key = |i: u32| spans[i as usize].0 == u32::MAX;
            let key = |i: u32| match spans[i as usize] {
                (u32::MAX, _) => self.name(i as usize),
                (start, len) => &keys[start as usize..start as usize + len as usize],
            };
            // Sorting fixed-size prefixes is much faster than comparing
            // keys through the buffer; equal prefixes are then refined.
            let mut prefixes: Vec<(u64, u32)> = (0..self.len() as u32)
                .map(|i| (key_prefix(key(i)), i))
                .collect();
            prefixes.sort_unstable();
            let mut order: Vec<u32> = prefixes.iter().map(|k| k.1).collect();
            let mut start = 0;
            while start < prefixes.len() {
                let mut end = start + 1;
                while end < prefixes.len() && prefixes[end].0 == prefixes[start].0 {
                    end += 1;
                }
                if end - start > 1 {
                    // Stable: rows with equal prefixes are in index order
                    // already, which keeps ties in path order, and runs of
                    // equal names (`index.js` in every package) cost one
                    // comparison each.
                    order[start..end].sort_by(|&a, &b| {
                        let order = key(a).cmp(key(b));
                        // Names that are their own key are equal when their keys are.
                        if order.is_ne() || (own_key(a) && own_key(b)) {
                            order
                        } else {
                            self.name(a as usize).cmp(self.name(b as usize))
                        }
                    });
                }
                start = end;
            }
            order
        })
    }
    /// Folded names of the rows whose names are not plain ASCII. Built once
    /// per table on first use; ASCII names need no folded copy because the
    /// scanner compares them case-insensitively as they are.
    pub fn folded(&self) -> &Folded {
        self.folded.get_or_init(|| {
            let mut folded = Folded::default();
            for i in 0..self.rows.len() {
                let name = self.name(i);
                if !name.is_ascii() {
                    folded.bytes.extend(crate::fold::fold(name));
                    folded.ends.push(folded.bytes.len() as u32);
                    folded.rows.push(i as u32);
                }
            }
            folded
        })
    }
    /// The name order, if a search has already built it.
    pub fn name_order_if_built(&self) -> Option<&[u32]> {
        self.name_order.get().map(Vec::as_slice)
    }
    /// Derives the name order after a splice instead of re-sorting every row.
    ///
    /// `kept` lists `(old_start, old_end, new_start)` ranges copied unchanged
    /// from `old`; every other row of `self` is new. Unchanged rows keep their
    /// relative order (their indices only shift), so the old order is reused
    /// and only the new rows are sorted and inserted by binary search.
    pub fn splice_name_order(&self, old: &Table, kept: &[(usize, usize, usize)]) {
        let Some(old_order) = old.name_order_if_built() else {
            return;
        };
        let mut remap = vec![u32::MAX; old.len()];
        let mut is_new = vec![true; self.len()];
        for &(start, end, new_start) in kept {
            for (offset, old_index) in (start..end).enumerate() {
                remap[old_index] = (new_start + offset) as u32;
                is_new[new_start + offset] = false;
            }
        }
        let fresh: Vec<u32> = (0..self.len() as u32)
            .filter(|i| is_new[*i as usize])
            .collect();
        // Many new rows: a full sort is as cheap, so let it happen lazily.
        if fresh.len() > 65_536 && fresh.len() > self.len() / 8 {
            return;
        }
        let compare = |a: &u32, b: &u32| {
            name_cmp(self.name(*a as usize), self.name(*b as usize)).then(a.cmp(b))
        };
        let kept_order: Vec<u32> = old_order
            .iter()
            .map(|i| remap[*i as usize])
            .filter(|i| *i != u32::MAX)
            .collect();
        let mut fresh = fresh;
        fresh.sort_unstable_by(compare);
        let mut order = Vec::with_capacity(self.len());
        let mut from = 0;
        for row in fresh {
            let at =
                from + kept_order[from..].partition_point(|k| compare(k, &row) == Ordering::Less);
            order.extend_from_slice(&kept_order[from..at]);
            order.push(row);
            from = at;
        }
        order.extend_from_slice(&kept_order[from..]);
        let _ = self.name_order.set(order);
    }
    pub(crate) fn parts(&self) -> Parts<'_> {
        (self.names.as_slice(), &self.dirs, self.rows.as_slice())
    }
    pub(crate) fn item_entries(&self) -> &[(u32, u32)] {
        &self.items
    }
    pub(crate) fn from_parts(
        names: Blob,
        dirs: Dirs,
        rows: RowStore,
        items: Vec<(u32, u32)>,
    ) -> Self {
        Self {
            names,
            dirs,
            rows,
            items,
            name_order: OnceLock::new(),
            folded: OnceLock::new(),
        }
    }
    pub(crate) fn from_mapped(
        names: MappedSlice,
        dirs: Dirs,
        rows: MappedSlice,
        items: Vec<(u32, u32)>,
    ) -> Option<Self> {
        if !rows.len().is_multiple_of(std::mem::size_of::<Row>())
            || !(rows.bytes().as_ptr() as usize).is_multiple_of(std::mem::align_of::<Row>())
        {
            return None;
        }
        let row_slice: &[Row] = bytemuck::cast_slice(rows.bytes());
        let mut previous = 0_u32;
        for (index, row) in row_slice.iter().enumerate() {
            let offset = row.name_offset();
            if offset < previous || offset as usize > names.len() || (index == 0 && offset != 0) {
                return None;
            }
            previous = offset;
            let dir = row.dir();
            if dir != NO_DIR && dir as usize >= dirs.len() {
                return None;
            }
        }
        Some(Self::from_parts(
            Blob::Mapped(names),
            dirs,
            RowStore::Mapped(rows),
            items,
        ))
    }
}

/// Borrowed view of one row, convenient for tests and diagnostics.
pub struct EntryRef<'a> {
    table: &'a Table,
    index: usize,
    pub kind: Kind,
    pub size: u64,
    pub modified: i64,
    pub created: i64,
}
impl EntryRef<'_> {
    pub fn name(&self) -> &[u8] {
        self.table.name(self.index)
    }
    pub fn path(&self) -> Vec<u8> {
        self.table.path(self.index)
    }
    pub fn path_buf(&self) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(self.path()).into()
    }
}

/// Appends rows in component path order. Parent directories are deduplicated
/// through the stack of open ancestors, which is exact for ordered input.
pub struct Builder {
    names: Vec<u8>,
    dirs: Dirs,
    rows: Vec<Row>,
    ancestors: Vec<(Vec<u8>, u32)>,
}
impl Default for Builder {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}
impl Builder {
    pub fn with_capacity(rows: usize) -> Self {
        Self {
            names: Vec::with_capacity(rows * 16),
            dirs: Dirs::default(),
            rows: Vec::with_capacity(rows),
            ancestors: Vec::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    fn intern(&mut self, dir: &[u8]) -> u32 {
        self.dirs.intern(&mut self.ancestors, dir)
    }
    /// Returns false when the name is too long to store (over 65535 bytes,
    /// far beyond Linux's 255-byte limit) or the buffer would overflow.
    pub fn push(&mut self, path: &[u8], kind: Kind, size: u64, modified: i64) -> bool {
        self.push_with(path, kind, size, modified, NO_DATE)
    }
    pub fn push_with(
        &mut self,
        path: &[u8],
        kind: Kind,
        size: u64,
        modified: i64,
        created: i64,
    ) -> bool {
        let (dir, name) = split(path);
        if name.len() > u16::MAX as usize || self.names.len() + name.len() > u32::MAX as usize {
            return false;
        }
        let dir = match dir {
            Some(dir) => self.intern(dir),
            None => NO_DIR,
        };
        let name_offset = self.names.len() as u32;
        self.names.extend_from_slice(name);
        self.rows
            .push(Row::new(size, modified, created, name_offset, dir, kind));
        true
    }
    /// Copies rows `start..end` of an existing table. Names move as one block
    /// and each parent folder is resolved once per run of siblings.
    pub fn copy_range(&mut self, table: &Table, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let first = table.rows[start].name_offset() as usize;
        let stop = table.rows[end - 1].name_offset() as usize + table.name_len(end - 1);
        let shift = self.names.len() as i64 - first as i64;
        self.names.extend_from_slice(&table.names[first..stop]);
        let (mut old_dir, mut new_dir) = (NO_DIR, NO_DIR);
        let mut dir_path = Vec::new();
        for row in &table.rows[start..end] {
            let dir = row.dir();
            if dir != old_dir {
                old_dir = dir;
                new_dir = if dir == NO_DIR {
                    NO_DIR
                } else {
                    table.dirs.write_path(dir as usize, &mut dir_path);
                    self.intern(&dir_path)
                };
            }
            self.rows.push(Row {
                name_offset: (i64::from(row.name_offset()) + shift) as u32,
                dir_kind: encode_dir_kind(new_dir, row.kind()),
                ..*row
            });
        }
    }
    /// Copies row `i` of an existing table.
    pub fn push_from(&mut self, table: &Table, i: usize) {
        let row = table.rows[i];
        let dir = match table.dir_path(i) {
            Some(dir) => self.intern(&dir),
            None => NO_DIR,
        };
        let name_offset = self.names.len() as u32;
        self.names.extend_from_slice(table.name(i));
        self.rows.push(Row {
            name_offset,
            dir_kind: encode_dir_kind(dir, row.kind()),
            ..row
        });
    }
    pub fn finish(mut self) -> Table {
        self.names.shrink_to_fit();
        self.rows.shrink_to_fit();
        self.dirs.shrink_to_fit();
        let mut table = Table::from_parts(
            Blob::Owned(self.names),
            self.dirs,
            RowStore::Owned(self.rows),
            Vec::new(),
        );
        table.sum_folder_sizes();
        table
    }
}

impl Table {
    /// Sets every folder row's size to the total size of the files inside
    /// it, and its item count to the number of entries inside it. One pass
    /// in path order with a stack of open folders; a folder's totals are
    /// passed up to its parent when the walk leaves it. Rows with the same
    /// parent are siblings, so the ancestry check runs once per run of
    /// siblings, not once per row.
    pub(crate) fn sum_folder_sizes(&mut self) {
        struct Open {
            row: usize,
            path: Vec<u8>,
            size: u64,
            items: u64,
        }
        let mut open: Vec<Open> = Vec::new();
        let mut current_dir = u32::MAX - 1;
        let mut dir_path: Vec<u8> = Vec::new();
        let mut item_rows: Vec<(u32, u32)> = Vec::new();
        let close = |open: &mut Vec<Open>, rows: &mut [Row], item_rows: &mut Vec<(u32, u32)>| {
            if let Some(done) = open.pop() {
                rows[done.row].size = done.size;
                item_rows.push((done.row as u32, done.items.min(u64::from(u32::MAX)) as u32));
                if let Some(parent) = open.last_mut() {
                    parent.size += done.size;
                    parent.items += done.items;
                }
            }
        };
        for i in 0..self.rows.len() {
            let row = self.rows[i];
            let dir = row.dir();
            if dir != current_dir {
                current_dir = dir;
                dir_path.clear();
                if dir != NO_DIR {
                    self.dirs.write_path(dir as usize, &mut dir_path);
                }
                while open
                    .last()
                    .is_some_and(|o| dir == NO_DIR || !within(&dir_path, &o.path))
                {
                    close(&mut open, &mut self.rows, &mut item_rows);
                }
            }
            if let Some(parent) = open.last_mut() {
                parent.items += 1;
            }
            match row.kind() {
                Kind::Folder => {
                    let mut path = Vec::new();
                    self.write_path(i, &mut path);
                    open.push(Open {
                        row: i,
                        path,
                        size: 0,
                        items: 0,
                    });
                    // The next row is either this folder's first child or a
                    // later sibling: check again even if its parent id is the same.
                    current_dir = u32::MAX - 1;
                }
                Kind::File => {
                    if let Some(parent) = open.last_mut() {
                        parent.size += row.size;
                    }
                }
                _ => {}
            }
        }
        while !open.is_empty() {
            close(&mut open, &mut self.rows, &mut item_rows);
        }
        item_rows.sort_unstable_by_key(|(row, _)| *row);
        self.items = item_rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn table(paths: &[&str]) -> Table {
        Table::from_entries(
            paths
                .iter()
                .map(|p| Entry::new(p.as_bytes().to_vec(), Kind::File, 1, 2))
                .collect(),
        )
    }
    #[test]
    fn folder_sizes_empty_folders_and_dir_rows() {
        let entry = |path: &str, kind, size| Entry::new(path.as_bytes().to_vec(), kind, size, 0);
        let t = Table::from_entries(vec![
            entry("/d", Kind::Folder, 4096),
            entry("/d/a.txt", Kind::File, 10),
            entry("/d/sub", Kind::Folder, 4096),
            entry("/d/sub/b.txt", Kind::File, 100),
            entry("/d/sub/deep", Kind::Folder, 4096),
            entry("/d/sub/deep/c.txt", Kind::File, 1000),
            entry("/d/sub/link", Kind::Link, 5),
            entry("/d/zed", Kind::Folder, 4096),
            entry("/d/zz.txt", Kind::File, 20),
            entry("/e", Kind::Folder, 4096),
            entry("/e/x.txt", Kind::File, 7),
        ]);
        let size_of = |path: &[u8]| t.row(t.lower_bound(path)).size;
        assert_eq!(size_of(b"/d"), 1130);
        assert_eq!(size_of(b"/d/sub"), 1100);
        assert_eq!(size_of(b"/d/sub/deep"), 1000);
        assert_eq!(size_of(b"/d/zed"), 0);
        assert_eq!(size_of(b"/e"), 7);
        let items_of = |path: &[u8]| t.items(t.lower_bound(path));
        assert_eq!(items_of(b"/d"), 8);
        assert_eq!(items_of(b"/d/sub"), 4);
        assert_eq!(items_of(b"/d/zed"), 0);
        assert_eq!(items_of(b"/d/a.txt"), 0);
        assert_eq!(t.entry(t.lower_bound(b"/d/sub")).items, 4);
        assert!(t.is_empty_folder(t.lower_bound(b"/d/zed")));
        assert!(!t.is_empty_folder(t.lower_bound(b"/d/sub")));
        assert!(!t.is_empty_folder(t.lower_bound(b"/d/a.txt")));
        let dir_rows = t.dir_rows();
        let dir_of = |path: &[u8]| t.row(t.lower_bound(path)).dir() as usize;
        assert_eq!(
            dir_rows[dir_of(b"/d/sub/b.txt")] as usize,
            t.lower_bound(b"/d/sub")
        );
        assert_eq!(dir_rows[dir_of(b"/e/x.txt")] as usize, t.lower_bound(b"/e"));
        assert_eq!(dir_rows[dir_of(b"/d")], u32::MAX, "/ is not indexed");
        assert_eq!(t.entry(1).created, NO_DATE);
    }
    #[test]
    fn component_order_keeps_subtrees_contiguous() {
        let t = table(&["/a-c", "/a/b", "/a", "/a/b/c", "/a b", "/b"]);
        let paths: Vec<_> = (0..t.len())
            .map(|i| String::from_utf8(t.path(i)).unwrap())
            .collect();
        assert_eq!(paths, ["/a", "/a/b", "/a/b/c", "/a b", "/a-c", "/b"]);
        let start = t.lower_bound(b"/a");
        assert_eq!((start, t.subtree_end(start, b"/a")), (0, 3));
        assert_eq!(t.lower_bound(b"/a0"), 5);
    }
    #[test]
    fn parents_are_stored_once_and_paths_round_trip() {
        let t = table(&["/", "/x", "/x/one", "/x/two", "/y/three", "relative"]);
        assert_eq!(t.dirs().len(), 3, "/, /x and /y");
        for (i, expected) in ["/", "/x", "/x/one", "/x/two", "/y/three", "relative"]
            .iter()
            .enumerate()
        {
            assert_eq!(t.path(i), expected.as_bytes());
        }
        assert_eq!(t.dir_path(0), None);
        assert_eq!(t.name(2), b"one");
    }
    #[test]
    fn spliced_name_order_equals_a_full_sort() {
        let old = table(&[
            "/a/zeta",
            "/a/beta",
            "/b/alpha",
            "/b/beta",
            "/c/delta",
            "/c/x/beta",
        ]);
        let _ = old.name_order();
        // Replace /b's subtree with different rows, keeping /a and /c.
        let mut builder = Builder::default();
        let b_start = old.lower_bound(b"/b");
        let b_end = old.subtree_end(b_start, b"/b");
        builder.copy_range(&old, 0, b_start);
        for path in ["/b/aardvark", "/b/beta", "/b/gamma"] {
            builder.push(path.as_bytes(), Kind::File, 1, 2);
        }
        let new_start = builder.len();
        builder.copy_range(&old, b_end, old.len());
        let spliced = builder.finish();
        spliced.splice_name_order(&old, &[(0, b_start, 0), (b_end, old.len(), new_start)]);
        let expected = Table::from_entries(spliced.entries());
        let names = |t: &Table, order: &[u32]| -> Vec<Vec<u8>> {
            order.iter().map(|i| t.path(*i as usize)).collect()
        };
        assert!(spliced.name_order_if_built().is_some());
        assert_eq!(
            names(&spliced, spliced.name_order()),
            names(&expected, expected.name_order())
        );
        assert_eq!(spliced.entries(), expected.entries());
    }
    #[test]
    fn natural_name_order() {
        let names = [
            "IMG_10.jpg",
            "img_2.jpg",
            "IMG_1.jpg",
            "Zebra",
            "apple",
            "École",
            "eclair",
            "file02",
            "file2",
            "file1a",
            "file 9",
            "v1.10",
            "v1.9",
            "007",
            "7",
            "a",
            "A",
            "notes-2026-01-15.txt",
            "notes-2026-1-2.txt",
        ];
        let t = table(
            &names
                .map(|n| format!("/d/{n}"))
                .each_ref()
                .map(String::as_str),
        );
        let sorted: Vec<_> = t
            .name_order()
            .iter()
            .map(|i| String::from_utf8(t.name(*i as usize).to_vec()).unwrap())
            .collect();
        assert_eq!(
            sorted,
            [
                "007",
                "7",
                "A",
                "a",
                "apple",
                "eclair",
                "École",
                "file 9",
                "file1a",
                "file02",
                "file2",
                "IMG_1.jpg",
                "img_2.jpg",
                "IMG_10.jpg",
                "notes-2026-1-2.txt",
                "notes-2026-01-15.txt",
                "v1.9",
                "v1.10",
                "Zebra"
            ]
        );
        // The direct comparison agrees with the key, and prefix keys agree
        // with the full comparison.
        let long = "x".to_owned() + &"1".repeat(300);
        let longer = "x".to_owned() + &"1".repeat(301);
        let mut names = names.to_vec();
        names.extend([
            "x123456789",
            "x99999999",
            "x0000000000123",
            long.as_str(),
            longer.as_str(),
            "x1y",
            "x1Y2",
            "Ärger 10",
            "arger 9",
            "a.b",
            "a1",
            "a_1",
        ]);
        for a in &names {
            for b in &names {
                let (ka, kb) = (natural_key(a.as_bytes()), natural_key(b.as_bytes()));
                assert_eq!(
                    natural_key_cmp(a.as_bytes(), b.as_bytes()),
                    ka.cmp(&kb),
                    "{a} {b}"
                );
                assert_eq!(
                    name_cmp(a.as_bytes(), b.as_bytes()),
                    ka.cmp(&kb).then_with(|| a.cmp(b)),
                    "{a} {b}"
                );
                let (pa, pb) = (key_prefix(&ka), key_prefix(&kb));
                if pa != pb {
                    assert_eq!(pa.cmp(&pb), name_cmp(a.as_bytes(), b.as_bytes()), "{a} {b}");
                }
            }
        }
    }
    #[test]
    fn name_order_breaks_ties_by_path() {
        let t = table(&[
            "/b/same-long-name",
            "/a/same-long-name",
            "/c/x",
            "/c/aaaaaaaaaab",
            "/c/aaaaaaaaaaa",
        ]);
        let names: Vec<_> = t.name_order().iter().map(|i| t.path(*i as usize)).collect();
        assert_eq!(
            names,
            [
                &b"/c/aaaaaaaaaaa"[..],
                b"/c/aaaaaaaaaab",
                b"/a/same-long-name",
                b"/b/same-long-name",
                b"/c/x"
            ]
        );
    }
}
