# Performance

These are measurements on one development machine, not guarantees. Run the
included benchmarks against your own folders to see what to expect.

**Machine:** AMD Ryzen 7 PRO 7840U, 32 GB RAM, NVMe SSD, Linux Mint (Cinnamon),
release build.
**Data:** a real developer home folder with about 1.6 million entries in about 102,000
folders. Average path length is 175 bytes, with many deep source and build trees.

## Index

| | |
|---|---|
| Entries | 1,596,495 |
| Index memory | 100 MB (about 63 bytes per entry) |
| Index file on disk | 95 MB |
| Loading the saved index at startup | 74 ms |
| Building the name order (first search only) | 120 ms (byte order, 0.8) |
| Full scan of the folder (cold walk) | 9–21 s, about 120 MB peak |
| Re-indexing one changed folder | 55–85 ms, in the background |

Since 0.9, names sort naturally (case- and accent-insensitive, numbers by value). On a
later copy of the same home folder (1.74 million entries, while the computer was in use),
building that order took about 450 ms, against about 290 ms for the byte order in the
same runs; it happens once per loaded index, at the first search sorted by name.

Paths are stored once per folder and names share one buffer, so memory grows with the
number of entries rather than with path length. File types excluded by default
(temporary files, compiler output) make real indexes smaller still.

## Start-up (0.9)

From 0.9, start-up checks the saved index folder by folder and only reads folders whose
modification time changed (see the manual). `examples/bench_startup.rs` compares that with
a full walk, both adding the folder watches the app adds. On `/usr` (1.12 million entries
in 106,000 folders, warm cache, three runs):

| | |
|---|---|
| Full walk with watches | 7.1–7.3 s |
| Start-up check with watches | 1.4 s |

With a cold cache the difference should be larger, as the check reads one inode per
folder instead of one per entry; that was not measured.

## Search (engine time, 1.6 million entries)

Medians of 7 runs, returning the first 10,000 results:

| Query | Matches | Sorted by name | Sorted by size |
|---|---:|---:|---:|
| `a` | 853,586 | 22 ms | 32 ms |
| `readme` | 2,727 | 16 ms | 14 ms |
| `ext:rs` | 693 | 10 ms | 8 ms |
| `ext:pdf;docx` | 185 | 11 ms | 8 ms |
| `*.json` | 44,739 | 9 ms | 8 ms |
| `size:>100mb` | 402 | 4 ms | 2 ms |
| `regex:^test.*\.py$` | 4,672 | 28 ms | 26 ms |
| `path:src main` | 60 | 38 ms | 39 ms |
| `zzqqxxnotfound` | 0 | 10 ms | 10 ms |

Sorting every entry of a 474,000-entry index by type takes 28 ms.

Plain words scan the shared name buffer once, starting from the rarest byte of the term.
`*.ext`-style wildcards become suffix checks, and path terms are evaluated once per
folder rather than once per file. Sorting by size, date or type only orders the page that
is shown.

## Desktop app

Measured with the full window on a virtual X server (software rendering) against the same
1.6-million-entry folder:

| | |
|---|---|
| Searchable after launch (saved index) | 0.4 s |
| Startup rescan finished (0.8: full walk) | 9–19 s |
| Folder watches | 102,000, no errors |
| CPU while idle | 0.0 % |
| Private memory while idle | about 171 MB (index about 100 MB, watch bookkeeping about 30 MB, GTK the rest) |
| New file visible in open results | about 0.25 s |

The window has no animations or timers. It only redraws for input, index changes or
finished searches. The folder watcher subscribes only to real changes (create, delete,
move, close-after-write, attribute change), never to opens or reads. Its own scans
therefore cause no events, and a busy system does not make File Minnow busy. During bursts of
changes, updates are rate-limited to about a tenth of one core.

## Reproduce

```bash
# Engine: load an existing index read-only and time queries
cargo run --release --no-default-features --example bench_index -- ~/.cache/file-minnow
# Engine: synthetic in-memory corpus
cargo run --release --no-default-features --example bench_search
# Cost of re-indexing changed folders
cargo run --release --no-default-features --example bench_update -- ~/.cache/file-minnow
# Start-up: full walk versus the check of changed folders, on a folder you name
cargo run --release --no-default-features --example bench_startup -- /usr
# Indexing and idle cost on generated files (no GUI)
python3 scripts/benchmark_io.py
# Full desktop app on a private virtual X server (needs Xvfb)
python3 scripts/measure_gui.py --help
```

## Compact index (0.10)

Measured on a 16-core machine with 62 GB RAM, an ext4 home folder on NVMe and Linux
6.12. The folder had 5.69 million files and 0.91 million directories (6.26 million
indexed entries, 828 MB of path text). These are one machine's numbers, not guarantees.

| | 0.9 in-memory index | 0.10 compact + mapped |
|---|---:|---:|
| Index memory | 465 MB | 300 MB |
| Index file | 461 MB | 299 MB |
| Build peak RSS | 486 MB | 308 MB |
| Load saved index | 224 ms | 61 ms |
| Daemon idle RSS | about 700 MB | 188 MB |
| Folder monitoring | 908,000 inotify watches | 1 fanotify mark (root mode) |
| `a` (3.9M matches, name sort) | 93 ms | 178 ms |
| `readme` (42,700 matches) | 46 ms | 83 ms |
| `path:src main` | 212 ms | 522 ms |
| `regex:^test.*\.py$` | 96 ms | 210 ms |
| `a` sorted by type | 264 ms | 526 ms |

The compact index stores directory names as parent-linked nodes and packs each row into
24 bytes. Rows no longer store a name length: names are back to back, so a name ends
where the next one begins. The saved file is mapped; after a save the daemon reloads it
and returns mapped pages to the page cache when idle, which is why idle RSS is much
lower than the index size. The trade-off is that path and regex queries rebuild directory
paths or touch the next row, so they are about twice as slow as the old in-memory table.
Narrow name queries are unchanged within noise.

The start-up quick scan still builds a heap copy while it reconciles changed folders; on
this home folder that peak was about 1.1 GB. When no folder changed, the mapped index is
kept and the peak is much lower.

