#!/usr/bin/env bash
# Uses GTK development files from the system (libgtk-3-dev / gtk3-devel), or an
# extracted SDK in .dev/gtk-sdk (or $FILE_MINNOW_GTK_SDK) when they are not installed.
set -euo pipefail
minnow_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
minnow_sdk="${FILE_MINNOW_GTK_SDK:-$minnow_root/.dev/gtk-sdk/root}"
if ! pkg-config --exists gtk+-3.0 && [[ -d "$minnow_sdk/usr" ]]; then
  export PKG_CONFIG_PATH="$minnow_sdk/usr/lib/x86_64-linux-gnu/pkgconfig:$minnow_sdk/usr/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  export PKG_CONFIG_SYSROOT_DIR="$minnow_sdk"
fi
exec "$@"
