#!/usr/bin/env bash
# Runs inside an ephemeral distro container, never on the user's host.
set -euo pipefail
if [[ ! -f /.dockerenv || "${MINNOW_PACKAGE_TEST:-}" != 1 ]]; then
  echo 'Refusing host installation: use a disposable test container.' >&2
  exit 2
fi
base_package=$1
upgrade_package=$2
export DEBIAN_FRONTEND=noninteractive
# Minimal Ubuntu containers strip /usr/share/doc by policy. Restore normal
# desktop installation behavior so the package's included notices are checked.
for dpkg_policy in /etc/dpkg/dpkg.cfg.d/*; do
  sed -i '\@path-exclude=/usr/share/doc/@d' "$dpkg_policy"
done
apt-get update
apt-get install -y --no-install-recommends "$base_package"
test "$(stat -c '%a' /usr/bin/file-minnow)" = 755
test -f /usr/share/applications/org.fileminnow.FileMinnow.desktop
test -f /usr/share/icons/hicolor/scalable/apps/file-minnow.svg
test -f /usr/share/doc/file-minnow/licenses/dependencies.json
test ! -e /etc/systemd/user/default.target.wants/file-minnow.service
test ! -e /root/.config/autostart/file-minnow.desktop
mkdir -p /tmp/minnow-files
printf 'fixture\n' > /tmp/minnow-files/invoice.pdf
file-minnow --data-dir /tmp/minnow-cache index --root /tmp/minnow-files
test "$(file-minnow --data-dir /tmp/minnow-cache search ext:pdf)" = /tmp/minnow-files/invoice.pdf
settings_before=$(sha256sum /tmp/minnow-cache/settings.json)
dpkg -i "$upgrade_package"
test "$(sha256sum /tmp/minnow-cache/settings.json)" = "$settings_before"
test "$(file-minnow --data-dir /tmp/minnow-cache search ext:pdf)" = /tmp/minnow-files/invoice.pdf

# The GUI's native libraries must be covered by the application package. These
# additional packages are the isolated test infrastructure and software renderer.
apt-get install -y --no-install-recommends xvfb xdotool imagemagick dbus-x11 python3 python3-dbus python3-gi python3-xlib libgl1-mesa-dri
python3 /tests/test_ui.py --binary /usr/bin/file-minnow --tray --output /tmp/minnow-ui-proof
cat /tmp/minnow-ui-proof/result.json
dpkg --remove file-minnow
test ! -e /usr/bin/file-minnow
test ! -e /usr/share/applications/org.fileminnow.FileMinnow.desktop
test "$(sha256sum /tmp/minnow-cache/settings.json)" = "$settings_before"
test -f /tmp/minnow-cache/index.sqlite
printf 'FILE_MINNOW_DEB_QA_PASS: install, upgrade, native UI, remove, user data preserved\n'
