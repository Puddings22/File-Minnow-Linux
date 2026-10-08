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

- **Columns.** Name, Path, Size, Modified and Type. Click a header to sort and click again
  to reverse. Type sorting puts folders first, then groups files by extension.
  Column widths, sort order, window size and position are remembered.
- **Type filter.** The drop-down beside the search box limits results to Audio,
  Compressed, Document, Folder, Picture or Video. It combines with what you type.
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
| `size:>10mb`, `size:1kb..2mb` | File size (binary units) |
| `dm:today`, `dm:2026-10-07`, `dm:>2026-01-01` | Date modified, local calendar days |
| `regex:"^report.*\.pdf$"` | Rust regular expression |

OR has higher precedence than the implicit AND. Quotes protect spaces and operators.
Unknown modifiers are reported as errors; quote file names that contain a colon.
Case-insensitive matching folds ASCII letters. Paths keep their exact bytes, so names that
are not valid UTF-8 can still be opened even if they display with replacement
characters.

## Options

Open **Tools → Options** (Ctrl+,). Changes apply when you press Apply or OK; Escape or
Cancel discards them.

- **Folders.** Add or remove folders, choose whether subfolders are included and
  whether the folder is monitored for live changes, and set a safety rescan schedule
  (daily at a chosen time, every N minutes, or never). Daily at 03:00 is the default.
- **Exclusions.** Hidden files and folders, excluded folders, and include-only or exclude
  file patterns (one per line, case-sensitive).
- **File Types.** A list of extensions that are left out of the index. The defaults are
  temporary files, partial downloads, editor swap files and compiler output. Add, remove
  or restore the defaults, or switch the list off.
- **Search.** Search-as-you-type delay and related preferences.
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

Pausing indexing keeps searches working against the current index; resuming catches up.

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
file-minnow paths
```

`quit` closes the window and the indexer it owns; an independently started daemon keeps
running (`stop` stops it). `--data-dir /path` keeps a separate index and settings.
Search supports `--sort name|path|size|modified|type`, `--descending`, `--offset`,
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
