# Installing and packaging File Minnow

File Minnow uses standard Linux interfaces only: inotify, XDG directories, GTK 3,
X11 or Wayland, and D-Bus desktop protocols. It has no hard dependency on a particular
distribution, init system or package manager. A `.deb` package is provided for
Debian-based systems; every other distribution can use the portable archive or build
from source.

## Build from source (any distribution)

You need a current stable Rust toolchain ([rustup](https://rustup.rs)), a C compiler,
`pkg-config` and the GTK 3 development files:

| Distribution | Command |
|---|---|
| Debian, Ubuntu, Linux Mint, Pop!_OS | `sudo apt install build-essential pkg-config libgtk-3-dev` |
| Fedora | `sudo dnf install gcc pkg-config gtk3-devel` |
| Arch, Manjaro, EndeavourOS | `sudo pacman -S --needed base-devel gtk3` |
| openSUSE | `sudo zypper install gcc pkg-config gtk3-devel` |
| Alpine (musl) | `sudo apk add build-base pkgconf gtk+3.0-dev` |

```bash
cargo build --release --locked
# Command-line tool and index daemon only, without GTK:
cargo build --release --locked --no-default-features
```

The binary is `target/release/file-minnow`.

### Install for your user only

```bash
install -Dm755 target/release/file-minnow ~/.local/bin/file-minnow
install -Dm644 packaging/file-minnow.desktop ~/.local/share/applications/org.fileminnow.FileMinnow.desktop
install -Dm644 packaging/file-minnow.svg ~/.local/share/icons/hicolor/scalable/apps/file-minnow.svg
for size in 16 22 24 32 48 64 128 256; do
  install -Dm644 packaging/icons/file-minnow-$size.png ~/.local/share/icons/hicolor/${size}x${size}/apps/file-minnow.png
done
```

Make sure `~/.local/bin` is on your `PATH`. Remove those files to uninstall. Your
settings (`~/.config/file-minnow`) and index (`~/.cache/file-minnow`) are kept until you
delete them.

## Debian, Ubuntu and Linux Mint package

```bash
cargo build --release --locked
python3 scripts/package.py --format deb
sudo apt install ./dist/file-minnow_*_amd64.deb
```

For a package that also runs on older releases, build inside the provided Docker image
(Ubuntu 22.04 baseline: glibc 2.35, GTK 3.24). The result runs on Linux Mint 21+,
Ubuntu 22.04+, Debian 12+ and similar:

```bash
docker build --file packaging/Dockerfile --target export --output type=local,dest=dist/build .
python3 scripts/package.py --binary dist/build/file-minnow
```

The script reads the binary's architecture and required glibc version, and writes the
package, a portable archive and `dist/manifest.json` with checksums. Pass
`--maintainer 'Name <email>'` for a public release. Installing or removing the package
never starts the application, enables autostart, or deletes user settings and indexes.
Remove it with `sudo apt remove file-minnow`.

## Portable archive (other glibc distributions)

`scripts/package.py --format tar` produces `file-minnow-<version>-linux-<arch>.tar.gz`
with the same `usr/` layout as the package. Run `usr/bin/file-minnow` directly, or copy
the files as in "Install for your user only". The runtime needs GTK 3 (normally
installed with any desktop), a user D-Bus session for the tray and portals, and
`xdg-open`.

A glibc build does not run on musl-based systems such as Alpine; build from source
there.

## Starting automatically

Choose one of these, or neither:

- **Window in the tray at login.** Tick **Options → Interface → Start File Minnow at login**.
  This writes `~/.config/autostart/file-minnow.desktop`, which starts `file-minnow gui --hidden`;
  without a working tray the window stays visible. Untick it to remove the entry.
- **Background index service.** The package installs a systemd *user* unit but does not
  enable it. Enable it with `systemctl --user enable --now file-minnow.service`; the window
  attaches to it when opened. Without systemd, start `file-minnow daemon` with your
  session's autostart mechanism.

Enabling both is only useful if you want the index to keep running after the window
quits.

## Desktop compatibility

- **Global shortcut.** On X11 File Minnow registers the shortcut directly. On Wayland it uses
  the GlobalShortcuts portal where the compositor provides one; the desktop may ask
  you to confirm. You can also bind `file-minnow toggle` to a key in your desktop's
  keyboard settings.
- **Tray icon.** Uses StatusNotifierItem, as on Cinnamon, KDE Plasma, Xfce (with its status
  notifier plugin), MATE and Budgie. GNOME needs the AppIndicator extension. Without a tray,
  the window stays available and closing it quits.
- **Wayland window behaviour.** Showing and hiding the window from the shortcut is fully
  verified on X11. On Wayland the compositor decides about focus, and behaviour can vary.
