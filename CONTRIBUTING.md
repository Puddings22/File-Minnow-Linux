# Contributing to File Minnow

Thanks for helping. Bug reports with your distribution, desktop (and X11 or Wayland)
and steps to reproduce are the most useful thing you can send.

## Building

See the build-from-source section of the [README](README.md). Without system GTK
development packages, `scripts/with-gtk-sdk.sh <command>` uses an extracted GTK SDK in
`.dev/gtk-sdk` (or `$FILE_MINNOW_GTK_SDK`).

## Before sending a change

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --no-default-features --target-dir target/headless
```

For UI and desktop-integration changes, also run the isolated UI tests. They start
their own X server (Xvfb), D-Bus session, tray host and portal fixtures, and never touch
your desktop:

```bash
python3 scripts/test_ui.py --xvfb /path/to/Xvfb            # window, search, options, menus
python3 scripts/test_ui.py --xvfb /path/to/Xvfb --tray     # tray, global shortcut, close to tray
python3 scripts/test_ui.py --xvfb /path/to/Xvfb --tray --portal --daemon-first
python3 scripts/test_focus.py --xvfb /path/to/Xvfb         # focus with Muffin as window manager
```

They need `xdotool`, `xclip`, ImageMagick's `import`, and Python with `dbus` and `gi`.

## Principles

- **Speed and resource use come first.** The GUI is event-driven: no timers, polling or
  animations. Check idle CPU (the UI tests report it) after touching background loops.
- **Never subscribe to open or access events in the folder watcher** (`src/watch.rs`).
  Our own scans would generate events and trigger endless rescans.
  `tests/settings_runtime.rs` (`own_scans_do_not_trigger_more_scans`) guards this.
- **Paths are bytes.** Keep raw Linux path bytes end to end; never convert paths lossily.
  Directory symlinks are not followed.
- **Search semantics.** OR binds tighter than the implicit AND. Keep the parser and the
  bitset evaluator in agreement (`query::tests`).
- **Work on any distribution.** Do not tie runtime behaviour to Debian, systemd or one
  desktop. Cinnamon is the main test desktop; document X11, Wayland, tray and portal
  differences honestly.
- **Tests use temporary folders and data directories**, never a real home folder.
- Installation, autostart and global settings stay explicit user choices; packages never
  enable them.
