# File Minnow user manual

File Minnow searches indexed file and folder **names** on Linux; file contents are not
read. It runs with the permissions of your account and never needs root.

## Getting started

Launch **File Minnow** from the application menu or run `file-minnow gui`. Add folders
with **File → Add Folder…** or **Tools → Options → Folders**. Indexing runs in the
background and the status bar shows progress, the number of indexed entries and any
folders that could not be read. Type in the search box: results follow every keystroke.

Launching File Minnow again brings the existing window forward instead of starting a
second copy. A separately started daemon (`file-minnow daemon`) can keep the index running
while no window is open; the window attaches to it automatically.

## The window

- **Columns.** Name, Path, Size, Modified, Type and Created (hidden until you switch it on
  in the **View** menu). Click a header to sort and click again to reverse. Type sorting
  puts folders first, then groups files by extension. Folders show the total size of the
  indexed files inside them. The creation date comes from the filesystem (ext4, Btrfs and
  XFS record it; some network and FUSE filesystems do not, and the cell stays empty).
  Column widths, visibility, sort order, window size and position are remembered.
- **Highlighting.** The parts of a name that matched your search are shown in bold.
- **Type filter.** The drop-down beside the search box limits results to Audio,
  Compressed, Document, Folder, Picture or Video, followed by your own filters from
  **Options → Filters**. It combines with what you type.
- **Preview pane.** **View → Preview Pane** shows the selected picture (scaled to fit), the
  start of a text file, or a summary for other files, on the right of the results.
- **Bookmarks.** The **Bookmarks** menu runs a saved search; **Add Bookmark…** saves the
  current one and **Organize Bookmarks…** edits the list.
- **Search history.** Searches are remembered when you open a result or close the window,
  complete the search box as you type, and are listed under **Search → Recent Searches**
  (with **Clear Search History**). Switch it off in **Options → Search**.
- **Export.** **File → Export Results…** writes every result of the current search to a
  CSV file (name, path, size, dates, type) or a text file with one full path per line.
- **Unavailable drives.** Entries of an unplugged drive or unmounted share are shown greyed
  until it is back (see **Folders** below).
- **Right-click menu.** Open, Open With (the applications your desktop offers for the type),
  Open Containing Folder (your file manager opens with the file selected), Open in
  Terminal, Cut, Copy, Copy Full Path, Copy Name, Rename, Move to Trash and Properties.
  Cut and Copy put the files themselves on the clipboard, so you can paste them in your
  file manager. Pasting into a text field gives the paths.
- **File manager actions.** Below a separator, the menu shows the custom actions of your
  default file manager: Nemo actions and scripts (Cinnamon), Thunar custom actions
  (Xfce), Dolphin service menus (KDE), and Nautilus or Caja scripts (GNOME, MATE). Actions
  that depend on a file manager's internal state, and file managers' compiled
  extensions, cannot be shown.

### Keyboard

| Key | Action |
|---|---|
| Global shortcut (default Ctrl+Alt+Space) | Show or hide the window from anywhere |
| Ctrl+L | Focus the search box |
| ↓ / ↑ | Move into and through the results |
| Enter | Open the selected files |
| Alt+Enter | Properties |
| Ctrl+C / Ctrl+X | Copy / cut the selected files |
| Ctrl+A | Select all results |
| F2 | Rename |
| Delete | Move to trash (asks first when several items are selected) |
| Shift+F10 or Menu | Right-click menu |
| Ctrl+, | Options |
| F5 | Rescan |
| Escape | Close the window: to the tray when the tray is enabled, otherwise quit |
| Ctrl+Q | Quit |

## Search

| Example | Meaning |
|---|---|
| `invoice` | Name contains "invoice" (case-insensitive) |
| `invoice pdf` | Both terms |
| `apple banana \| orange` | apple AND (banana OR orange) |
| `<apple banana> \| orange` | Explicit grouping |
| `!draft` | Exclude names containing "draft" |
| `"hello world"` | Keep spaces and operators literally |
| `*.pdf`, `a?c` | Wildcards on the whole name |
| `ext:rs;toml` | Extension alternatives |
| `audio:` `zip:` `doc:` `pic:` `video:` | Common file types (the lists behind the type filter) |
| `file:` / `folder:` | Entry kind |
| `path:/Projects` | Match the full path; a `/` in a term does the same |
| `case:README` | Case-sensitive |
| `size:>10mb`, `size:1kb..2mb` | File size (binary units); folders count the files inside them |
| `dm:today`, `dm:2026-10-07`, `dm:>2026-01-01` | Date modified, local calendar days |
| `dc:2026-10-07`, `dc:>=2026-01-01` | Date created (same forms as `dm:`) |
| `wholeword:report`, `ww:report` | Whole words only (letters and digits around the match end it) |
| `dupe:` | Names that occur more than once in the index (exact name) |
| `empty:` | Folders with nothing indexed inside |
| `parent:/home/alex/Documents`, `infolder:` | Entries directly inside a folder |
| `child:README.md` | Folders that contain an entry with that name |
| `content:invoice total` | Text inside files (see below) |
| `regex:"^report.*\.pdf$"` | Rust regular expression |

OR has higher precedence than the implicit AND. Quotes protect spaces and operators.
Unknown modifiers are reported as errors; quote file names that contain a colon.

Matching ignores case and accents: `stiinta` finds `Știință`, `ecole` finds `École` and
`ÉCOLE`, and names saved in decomposed Unicode (as macOS writes them) match composed
input. `case:` makes a term exact (bytes as typed). Paths keep their exact bytes, so names
that are not valid UTF-8 can still be opened even if they display with replacement
characters.

`content:` reads every file left by the other terms (up to 16 MB each) and keeps those
containing the text, ASCII case-insensitively. It is slow by nature, so give it a name
term, a type filter or a folder to narrow the files first: `ext:txt content:total`.

## Options

Open **Tools → Options** (Ctrl+,). Changes apply when you press Apply or OK; Escape or
Cancel discards them.

- **Folders.** Add or remove folders, choose whether subfolders are included and
  whether the folder is monitored for live changes, and set a safety rescan schedule
  (daily at a chosen time, every N minutes, or never). Daily at 03:00 is the default;
  folders on a network share (NFS, SMB, SSHFS…) get a 30-minute rescan instead, because
  changes made by other computers do not reach inotify. **Keep its files in the index
  while it is unavailable** keeps an unplugged drive or unmounted share searchable
  (greyed) instead of dropping it. **Index removable drives when they are plugged in**
  adds drives mounted under `/media` or `/run/media` automatically, with that option on.
- **Exclusions.** Hidden files and folders, excluded folders, and include-only or exclude
  file patterns (one per line, case-sensitive).
- **File Types.** A list of extensions that are left out of the index. The defaults are
  temporary files, partial downloads, editor swap files and compiler output. Add, remove
  or restore the defaults, or switch the list off.
- **Filters.** Your own entries for the drop-down beside the search box: a name and a
  search (for example `ext:psd;xcf` or `dm:today pic:`).
- **Bookmarks.** The saved searches of the Bookmarks menu.
- **Search.** Search-as-you-type delay, the search history, and **Show no results until you
  type a search**: with an empty search box the list stays empty (choosing a type filter
  still shows results).
- **Keyboard.** Click the field (or press Enter on it) and press the combination for the
  global show/hide shortcut. Escape cancels recording and Backspace clears the shortcut.
  If nothing happens, your desktop already uses that combination. On Cinnamon, check
  System Settings → Keyboard → Shortcuts.
- **Interface.** Start at login (adds File Minnow to your desktop's startup applications,
  starting in the tray), tray icon, close-to-tray, start hidden and theme.

Removing or excluding a folder never deletes files; it only removes them from the index.
`/proc`, `/sys`, `/dev`, `/run` and File Minnow's own data folder are always skipped.

## Live updates and rescans

File Minnow watches indexed folders with Linux inotify and updates only what changed.
New, renamed and deleted files appear within a fraction of a second. Opening or reading
files never triggers work. A file that is modified in place shows its new size and date
when it is closed. The daily safety rescan catches anything a watch could miss, such as
changes on network mounts or folders beyond the system's watch limit
(`fs.inotify.max_user_watches`). Watch problems are shown in the status bar.

Adding a folder indexes just that folder; the rest of the index is kept as it is.
Pausing indexing keeps searches working against the current index; resuming catches up.

## Searching from the file manager

The package adds **Search Here with File Minnow** to the folder menu of Nemo (Cinnamon)
and Dolphin (KDE). It runs `file-minnow show --in FOLDER`, which opens the window with the
search scoped to that folder. For Thunar, add a custom action with that command, `%f`
as the folder and "Directories" as the file type; for Nautilus, a script in
`~/.local/share/nautilus/scripts` can call it with `$NAUTILUS_SCRIPT_CURRENT_URI`.

## Command line

```bash
file-minnow index --root /path/to/files
file-minnow search 'ext:pdf !draft'
file-minnow search 'size:>10mb' --sort size --descending
file-minnow daemon --root /path/to/files
file-minnow search invoice --live
file-minnow status --live
file-minnow pause | resume | rescan
file-minnow show | toggle | quit
file-minnow show --in /path/to/folder
file-minnow paths
```

`quit` closes the window and the indexer it owns; an independently started daemon keeps
running (`stop` stops it). `--data-dir /path` keeps a separate index and settings.
Search supports `--sort name|path|size|modified|type|created`, `--descending`, `--offset`,
`--limit`, `--json` and `--null`; NUL-delimited output is safe for any file name.
`file-minnow paths` prints where the index, settings and sockets are.

## Files and recovery

Preferences and window state: `$XDG_CONFIG_HOME/file-minnow/` (usually
`~/.config/file-minnow/settings.json` and `window.json`). Index:
`$XDG_CACHE_HOME/file-minnow` (usually `~/.cache/file-minnow`). A custom `--data-dir`
holds all of them.

The index is a cache and can always be rebuilt. An unreadable index file is set aside as
`.corrupt-*` and rebuilt, and your settings are kept. The index is saved after full scans,
within a few minutes of changes, and on exit. Every start also rescans, so a crash only
delays changes and never loses them.

Installation, packaging and autostart are described in [install.md](install.md).
