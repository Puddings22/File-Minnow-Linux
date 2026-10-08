//! Compact in-memory index.
//!
//! Each parent directory path is stored once and file names share one byte
//! buffer, instead of keeping a separate full path for every entry. Rows are
//! kept in component-wise path order: a depth-first walk with siblings sorted
//! by name. That makes sorting by path free, keeps every subtree in one
//! contiguous range, and lets the builder deduplicate parent directories with
//! a stack of the current ancestors instead of a hash map.
use crate::index::{Entry, Kind};
use std::{cmp::Ordering, path::PathBuf, sync::OnceLock};

/// Names buffer, parent folder paths and rows, for storage and scanning.
pub(crate) type Parts<'a> = (&'a [u8], &'a Dirs, &'a [Row]);

/// Parent index of entries whose path has no parent, such as `/`.
pub const NO_DIR: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub size: u64,
    pub modified: i64,
    name_offset: u32,
    pub dir: u32,
    name_len: u16,
    pub kind: Kind,
}

/// Parent folder paths packed into one buffer (two allocations in total,
/// instead of one per folder).
#[derive(Clone, Debug, Default)]
pub struct Dirs {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}
impl Dirs {
    pub fn len(&self) -> usize {
        self.ends.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
    pub fn get(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }
    /// Returns false if the buffer would exceed 4 GiB.
    pub fn push(&mut self, dir: &[u8]) -> bool {
        if self.bytes.len() + dir.len() > u32::MAX as usize {
            return false;
        }
        self.bytes.extend_from_slice(dir);
        self.ends.push(self.bytes.len() as u32);
        true
    }
    fn shrink_to_fit(&mut self) {
        self.bytes.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
    fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * 4
    }
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    names: Vec<u8>,
    dirs: Dirs,
    rows: Vec<Row>,
    name_order: OnceLock<Vec<u32>>,
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
        let row = &self.rows[i];
        &self.names[row.name_offset as usize..row.name_offset as usize + row.name_len as usize]
    }
    pub fn dir_path(&self, i: usize) -> Option<&[u8]> {
        let dir = self.rows[i].dir;
        (dir != NO_DIR).then(|| self.dirs.get(dir as usize))
    }
    pub fn dirs(&self) -> &Dirs {
        &self.dirs
    }
    /// Writes the full path of row `i` into `out`, replacing its contents.
    pub fn write_path(&self, i: usize, out: &mut Vec<u8>) {
        out.clear();
        if let Some(dir) = self.dir_path(i) {
            out.extend_from_slice(dir);
            if !dir.ends_with(b"/") {
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
        Entry::new(self.path(i), row.kind, row.size, row.modified)
    }
    /// Materializes every entry. Intended for tests and small tools only.
    pub fn entries(&self) -> Vec<Entry> {
        (0..self.len()).map(|i| self.entry(i)).collect()
    }
    pub fn iter(&self) -> impl Iterator<Item = EntryRef<'_>> + '_ {
        (0..self.len()).map(move |index| EntryRef {
            table: self,
            index,
            kind: self.rows[index].kind,
            size: self.rows[index].size,
            modified: self.rows[index].modified,
        })
    }
    pub fn from_entries(mut entries: Vec<Entry>) -> Self {
        entries.sort_by(|a, b| path_cmp(&a.path, &b.path));
        entries.dedup_by(|a, b| a.path == b.path);
        let mut builder = Builder::with_capacity(entries.len());
        for e in &entries {
            builder.push(&e.path, e.kind, e.size, e.modified);
        }
        builder.finish()
    }
    /// Approximate heap bytes held by the table, for status reporting.
    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.rows.capacity() * std::mem::size_of::<Row>()
            + self.dirs.heap_bytes()
            + self.name_order.get().map_or(0, |o| o.capacity() * 4)
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
    /// Row indices sorted by name, ties in path order. Built once per table.
    pub fn name_order(&self) -> &[u32] {
        self.name_order.get_or_init(|| {
            // Sorting fixed-size prefix keys is much faster than comparing
            // names through the buffer; equal prefixes are then refined.
            let prefix = |i: usize| {
                let mut key = [0_u8; 8];
                let name = self.name(i);
                let n = name.len().min(8);
                key[..n].copy_from_slice(&name[..n]);
                u64::from_be_bytes(key)
            };
            let mut keys: Vec<(u64, u32)> =
                (0..self.len()).map(|i| (prefix(i), i as u32)).collect();
            keys.sort_unstable();
            let mut order: Vec<u32> = keys.iter().map(|k| k.1).collect();
            let mut start = 0;
            while start < keys.len() {
                let mut end = start + 1;
                while end < keys.len() && keys[end].0 == keys[start].0 {
                    end += 1;
                }
                if end - start > 1 {
                    order[start..end].sort_unstable_by(|a, b| {
                        self.name(*a as usize)
                            .cmp(self.name(*b as usize))
                            .then(a.cmp(b))
                    });
                }
                start = end;
            }
            order
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
            self.name(*a as usize)
                .cmp(self.name(*b as usize))
                .then(a.cmp(b))
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
        (&self.names, &self.dirs, &self.rows)
    }
    pub(crate) fn from_parts(names: Vec<u8>, dirs: Dirs, rows: Vec<Row>) -> Self {
        Self {
            names,
            dirs,
            rows,
            name_order: OnceLock::new(),
        }
    }
}

impl Row {
    pub(crate) fn name_range(&self) -> (u32, u16) {
        (self.name_offset, self.name_len)
    }
    pub(crate) fn from_raw(
        size: u64,
        modified: i64,
        name_offset: u32,
        dir: u32,
        name_len: u16,
        kind: Kind,
    ) -> Self {
        Self {
            size,
            modified,
            name_offset,
            dir,
            name_len,
            kind,
        }
    }
}

/// Borrowed view of one row, convenient for tests and diagnostics.
pub struct EntryRef<'a> {
    table: &'a Table,
    index: usize,
    pub kind: Kind,
    pub size: u64,
    pub modified: i64,
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
    ancestors: Vec<u32>,
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
        while let Some(&top) = self.ancestors.last() {
            let open = self.dirs.get(top as usize);
            if open == dir {
                return top;
            }
            if within(dir, open) {
                break;
            }
            self.ancestors.pop();
        }
        let id = self.dirs.len() as u32;
        // Over 4 GiB of folder paths is not a realistic index; keep going
        // with the last folder rather than failing the whole scan.
        if !self.dirs.push(dir) {
            return id.saturating_sub(1);
        }
        self.ancestors.push(id);
        id
    }
    /// Returns false when the name is too long to store (over 65535 bytes,
    /// far beyond Linux's 255-byte limit) or the buffer would overflow.
    pub fn push(&mut self, path: &[u8], kind: Kind, size: u64, modified: i64) -> bool {
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
        self.rows.push(Row {
            size,
            modified,
            name_offset,
            dir,
            name_len: name.len() as u16,
            kind,
        });
        true
    }
    /// Copies rows `start..end` of an existing table. Names move as one block
    /// and each parent folder is resolved once per run of siblings.
    pub fn copy_range(&mut self, table: &Table, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let first = table.rows[start].name_offset as usize;
        let last = &table.rows[end - 1];
        let stop = last.name_offset as usize + last.name_len as usize;
        let shift = self.names.len() as i64 - first as i64;
        self.names.extend_from_slice(&table.names[first..stop]);
        let (mut old_dir, mut new_dir) = (NO_DIR, NO_DIR);
        for row in &table.rows[start..end] {
            if row.dir != old_dir {
                old_dir = row.dir;
                new_dir = if row.dir == NO_DIR {
                    NO_DIR
                } else {
                    self.intern(table.dirs.get(row.dir as usize))
                };
            }
            self.rows.push(Row {
                name_offset: (i64::from(row.name_offset) + shift) as u32,
                dir: new_dir,
                ..*row
            });
        }
    }
    /// Copies row `i` of an existing table.
    pub fn push_from(&mut self, table: &Table, i: usize) {
        let row = table.rows[i];
        let dir = match table.dir_path(i) {
            Some(dir) => self.intern(dir),
            None => NO_DIR,
        };
        let name_offset = self.names.len() as u32;
        self.names.extend_from_slice(table.name(i));
        self.rows.push(Row {
            name_offset,
            dir,
            ..row
        });
    }
    pub fn finish(mut self) -> Table {
        self.names.shrink_to_fit();
        self.rows.shrink_to_fit();
        self.dirs.shrink_to_fit();
        Table::from_parts(self.names, self.dirs, self.rows)
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
