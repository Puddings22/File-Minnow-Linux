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
| Building the name order (first search only) | 120 ms |
| Full scan of the folder (cold walk) | 9–21 s, about 120 MB peak |
| Re-indexing one changed folder | 55–85 ms, in the background |

Paths are stored once per folder and names share one buffer, so memory grows with the
number of entries rather than with path length. File types excluded by default
(temporary files, compiler output) make real indexes smaller still.

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
| Startup rescan finished | 9–19 s |
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
# Indexing and idle cost on generated files (no GUI)
python3 scripts/benchmark_io.py
# Full desktop app on a private virtual X server (needs Xvfb)
python3 scripts/measure_gui.py --help
```
