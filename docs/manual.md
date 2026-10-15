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

- **Columns.** Name, Path, Size, Modified, Type, Created and Items (the last two hidden
  until you switch them on in the **View** menu). Click a header to sort and click again
  to reverse. Names sort naturally, as file managers do: case and accents do not matter
  and numbers sort by value (`IMG_2` before `IMG_10`). Type sorting puts folders first,
  then groups files by extension. Folders show the total size of the indexed files inside
  them, and Items counts the files and folders inside them at any depth: `folder:` sorted
  by size or items shows where the space goes. The creation date comes from the
  filesystem (ext4, Btrfs and XFS record it; some network and FUSE filesystems do not,
  and the cell stays empty). Column widths, visibility, sort order, window size and
  position are remembered.
- **Highlighting.** The parts of a name that matched your search are shown in bold.
- **Type filter.** The drop-down beside the search box limits results to Audio,
  Compressed, Document, Folder, Picture or Video, followed by your own filters from
  **Options → Filters**. It combines with what you type.
- **Preview pane.** **View → Preview Pane** shows the selected picture (scaled to fit), the
  start of a text file, or the file's thumbnail (videos, PDF and office documents,
  e-books, fonts…, see **Thumbnails**), else a summary, on the right of the results.
- **Thumbnails.** **View → Thumbnails** shows the results as a grid of thumbnails instead
  of a list; everything else (selection, right-click menu, dragging, keys) works the same.
  Thumbnails come from the cache that your file manager already fills
  (`~/.cache/thumbnails`, the freedesktop.org standard). Missing ones are made in the
  background for the items on screen, by decoding the picture or with your desktop's
  thumbnailers (the programs that make thumbnails for your file manager), and saved to
  the same cache so the file manager can use them too.
- **Status bar.** The left shows what File Minnow is doing; the right shows how many
  results are selected and their total size, the number of indexed entries, the number of
  matches and how long the search took.
- **Dragging.** Drag results to a file manager (to copy or move them), the desktop, a
  browser's upload field or a mail. A multiple selection stays selected while you drag it.
- **Bookmarks.** The **Bookmarks** menu runs a saved search; **Add Bookmark…** saves the
  current one and **Organize Bookmarks…** edits the list.
- **Search history.** Searches are remembered when you open a result or close the window,
  complete the search box as you type, and are listed under **Search → Recent Searches**
  (with **Clear Search History**). Switch it off in **Options → Search**.
- **Export.** **File → Export Results…** writes every result of the current search to a
  CSV file (name, path, size, dates, type), a text file with one full path per line, or
  an Everything file list (`.efu`, by giving the name that extension).
- **Unavailable drives.** Entries of an unplugged drive or unmounted share are shown greyed
  until it is back (see **Folders** below).
- **Right-click menu.** Open, Open With (the applications your desktop offers for the type),
  Browse in File Minnow (for a folder: list what is in it), Open Containing Folder (your
  file manager opens with the file selected), Open in Terminal, Cut, Copy, Copy Full
  Path, Copy Name, Rename, Move to Trash, Exclude from Index and Properties. Cut and Copy
  put the files themselves on the clipboard, so you can paste them in your file manager.
  Pasting into a text field gives the paths. **Exclude from Index** leaves the selected
  files and folders out of the index (they stay where they are); they are listed in
  **Options → Exclusions**, where you can take them out again.
- **Renaming several files.** F2 (or Rename…) with several results selected opens a window
  with three ways to make the new names: find and replace (optionally with a regular
  expression, where `$1` inserts the first group), a pattern (`[N]` the old name, `[C]` a
  counter, `[P]` the folder's name, `[D]` the date modified, `[E]` the extension, `[F]` the
  whole old name), or a change of case. A preview lists every new name; names that
  already exist in their folder, or that two files would get, are shown in red and stop
  the rename. The extension is kept unless you tick **Change the extension too**.
- **Undo.** **Edit → Undo** (Ctrl+Z in the results) takes back File Minnow's own renames
  and moves to the Trash, the last one first. Items in the Trash are put back where they
  were, as the freedesktop.org Trash specification describes. (The snap package moves
  files to the Trash through the desktop portal and cannot put them back.)
- **Browsing folders.** With **Options → Search → Open folders inside File Minnow**, Enter
  or a double-click on a folder lists what is in it (a `parent:` search), like a file
  manager: Backspace or Alt+Up goes to the parent folder, Alt+Left back. **Open
  Containing Folder** still opens your file manager.
- **Recent Changes.** **Tools → Recent Changes** lists what was created, deleted, changed,
  moved or renamed in the indexed folders in the last hour, today, or the last 7 or 30
  days, with a filter. Changes made while File Minnow was not running are listed when it
  finds them at the next start. Changes caused by new settings (a new folder or
  exclusion) are not listed. Open a file or its folder from there.
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
| Ctrl+Z | Undo the last rename or move to the Trash |
| F2 | Rename (several selected: rename them all at once) |
| Backspace or Alt+Up / Alt+Left | While browsing a folder: its parent folder / back |
| Ctrl+Space | In the search box: complete the word being typed (operators, filters, bookmarks) |
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
| `dm:today`, `dm:yesterday`, `dm:2026-10-07`, `dm:>2026-01-01` | Date modified, local calendar days |
| `dm:thisweek`, `dm:lastweek`, `dm:thismonth`, `dm:lastmonth`, `dm:thisyear`, `dm:lastyear` | Calendar periods (weeks start on Monday) |
| `dm:2026`, `dm:2026-10`, `dm:2025..2026-03` | A year, a month, or a range of them |
| `dm:last7days`, `dm:past2hours`, `dm:last3months` | Up to now: minutes, hours, days, weeks, months or years |
| `dc:2026-10-07`, `dc:>=thismonth` | Date created (same forms as `dm:`) |
| `wholeword:report`, `ww:report` | Whole words only (letters and digits around the match end it) |
| `dupe:` | Results whose name another result also has (exact name) |
| `dupe:size`, `dupe:content`, `dupe:name;size` | Results with the same size, the same contents, or both name and size as another result |
| `empty:` | Folders with nothing indexed inside |
| `parent:/home/alex/Documents`, `infolder:` | Entries directly inside a folder |
| `child:README.md` | Folders that contain an entry with that name |
| `content:invoice total` | Text inside files (see below) |
| `regex:"^report.*\.pdf$"` | Rust regular expression |

OR has higher precedence than the implicit AND. Quotes protect spaces and operators.
Unknown modifiers are reported as errors; quote file names that contain a colon.

Typing a word that starts with `:` lists the operators above, the type filters and your
own filters and bookmarks; choose one and it replaces that word. Ctrl+Space does the same
for the word you are typing. Recent searches complete the whole search box.

`dupe:` compares the results of the other terms only: `ext:mp4 dupe:content` finds
videos with the same contents as another video. `dupe:content` reads only files whose
sizes are equal, first their start and then the whole file where the starts agree, so
it is fast when few files share a size; it can still read a lot on a large folder of
similar files. Typing again cancels it.

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
  **Add File List…** indexes an Everything file list (see **File lists** below).
- **Exclusions.** Hidden files and folders; **Skip snapshot and system image folders** (on
  by default), which leaves out folders that hold whole extra copies of other files:
  Timeshift snapshots on any drive, Snapper's `.snapshots`, visible ZFS `.zfs` folders,
  mounted snap packages (`/snap`), the Nix store, and Flatpak's and rootless Podman's
  storage; excluded folders and files (**Exclude from Index** adds to this list); folders
  to **always index** even inside an excluded or hidden folder (the folders on the way
  to them are walked but not listed); and include-only or exclude file patterns (one per
  line, case-sensitive). A folder you add yourself is indexed even if it is inside one of
  these.
- **File Types.** A list of extensions that are left out of the index. The defaults are
  temporary files, partial downloads, editor swap files and compiler output. Add, remove
  or restore the defaults, or switch the list off.
- **Filters.** Your own entries for the drop-down beside the search box: a name and a
  search (for example `ext:psd;xcf` or `dm:today pic:`).
- **Bookmarks.** The saved searches of the Bookmarks menu.
- **Search.** Search-as-you-type delay, the search history, **Show no results until you
  type a search** (with an empty search box the list stays empty; choosing a type filter
  still shows results), and **Start with an empty search each time the window opens**
  (on by default). Turn it off to keep the last search, selected, so typing replaces it.
  **Open folders inside File Minnow** (see **Browsing folders**).
- **Keyboard.** Click the field (or press Enter on it) and press the combination for the
  global show/hide shortcut. Escape cancels recording and Backspace clears the shortcut.
  If nothing happens, your desktop already uses that combination. On Cinnamon, check
  System Settings → Keyboard → Shortcuts.
- **Interface.** Start at login (adds File Minnow to your desktop's startup applications,
  starting in the tray), tray icon, close-to-tray, start hidden and theme. Below them,
  **Search Here with File Minnow** can be added to (or removed from) the folder menu of
  each installed file manager, for your account only: GNOME Files (Nautilus), Nemo,
  Caja, Thunar and Dolphin. Thunar keeps your other custom actions. Thunar and Dolphin
  may need a restart to show it. (Not available in the snap package.)

Removing or excluding a folder never deletes files; it only removes them from the index.
`/proc`, `/sys`, `/dev`, `/run` and File Minnow's own data folder are always skipped.

## Live updates and rescans

File Minnow watches indexed folders with Linux inotify and updates only what changed.
New, renamed and deleted files appear within a fraction of a second. Opening or reading
files never triggers work. A file that is modified in place shows its new size and date
when it is closed. The daily safety rescan catches anything a watch could miss, such as
changes on network mounts or folders beyond the system's watch limit
(`fs.inotify.max_user_watches`). Watch problems are shown in the status bar.

Adding a folder, an exclusion or an always-indexed folder updates just those paths; the
rest of the index is kept as it is. Pausing indexing keeps searches working against the
current index; resuming catches up.

At start-up, File Minnow loads the saved index (searches work at once) and then checks
every indexed folder against it, reading again only the folders whose modification time
changed since the index was saved; the folders are watched again at the same time. That
finds files created, deleted, renamed or moved while File Minnow was not running, much
faster than reading every folder. A file changed in place does not change its folder's
time, so its size and date are updated at the next full walk: the scheduled rescan. A
scheduled rescan that was missed because the computer was off runs ten minutes after
start-up, so it does not slow down the login. A full walk happens at start-up instead
when the index settings changed since the index was saved.

## Searching from the file manager

The package adds **Search Here with File Minnow** to the folder menu of Nemo (Cinnamon)
and Dolphin (KDE). It runs `file-minnow show --in FOLDER`, which opens the window with the
search scoped to that folder and the cursor after it, so what you type searches inside
it. For GNOME Files (Nautilus), Caja and Thunar, which keep these per user, add it in
**Options → Interface**; with the portable archive, that also adds it for Nemo and
Dolphin. In Nautilus and Caja it is under **Scripts** in the right-click menu.

## Desktop search: GNOME and KDE

With the `.deb` installed, File Minnow's results also appear when you search in GNOME's
Activities overview and in KDE's KRunner (Alt+Space). The desktop starts
`file-minnow search-provider` when you first search and it stops after a minute without
searches. It answers from the running File Minnow, or from the saved index when File
Minnow is not running. Names that start with what you typed come first, then the most
recently modified files. Choosing a result opens it; in KRunner, **Open Containing
Folder** is offered too. Turn it off in GNOME Settings → Search, or in KDE's System
Settings → Search → KRunner. The snap package and the portable archive do not install
it (see [install.md](install.md)).

## File lists

File Minnow reads and writes Everything file lists (`.efu`): CSV files with a full path,
size, dates and attributes per line, as voidtools documents them.

- **Writing.** **File → Export Results…** with a `.efu` name, or
  `file-minnow search 'QUERY' --efu > list.efu` (every match). On a NAS or server, a
  daily `file-minnow index --root /srv/share` and `file-minnow search --efu` keep a list
  that desktops can index.
- **Indexing a list.** **Options → Folders → Add File List…** chooses a list and asks which
  part of the listed paths stands for which folder of this computer: for example
  `\\nas\share` (or `/srv/share`, or `D:\`) under `/mnt/nas`. The listed files are then
  searched like any folder, and greyed while that folder is not there (the share is not
  mounted, the drive is put away). Exclusions apply. The list is read again when it
  changes (it is checked every five minutes, and on a rescan). Lists made by Everything
  on Windows work too: separators, dates as FILETIME or ISO 8601, and names relative to
  the list are understood.

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
file-minnow changes --since 7d
file-minnow search 'ext:pdf' --efu > documents.efu
file-minnow paths
```

`quit` closes the window and the indexer it owns; an independently started daemon keeps
running (`stop` stops it). `--data-dir /path` keeps a separate index and settings.
Search supports `--sort name|path|size|modified|type|created|items`, `--descending`,
`--offset`, `--limit`, `--json`, `--null` and `--efu`; NUL-delimited output is safe for any
file name. `file-minnow changes` prints the Recent Changes history (`--since 30m`, `12h`,
`7d` or `2w`; `--json`). `file-minnow paths` prints where the index, settings and sockets
are.

## Files and recovery

Preferences and window state: `$XDG_CONFIG_HOME/file-minnow/` (usually
`~/.config/file-minnow/settings.json` and `window.json`). Index:
`$XDG_CACHE_HOME/file-minnow` (usually `~/.cache/file-minnow`), with the Recent Changes
history (`journal.log`, 30 days) and what file lists were last read (`lists.json`).
Thumbnails go to the shared cache, `~/.cache/thumbnails`. A custom `--data-dir` holds all
of File Minnow's own files.

The index is a cache and can always be rebuilt. An unreadable index file is set aside as
`.corrupt-*` and rebuilt, and your settings are kept. The index is saved after full scans
and the start-up check, within a few minutes of changes, and on exit (changes still
waiting are applied first). After a crash, the start-up check finds what changed through
the folders' times, and the next full walk the rest, so changes are delayed, never lost.

Installation, packaging and autostart are described in [install.md](install.md).
