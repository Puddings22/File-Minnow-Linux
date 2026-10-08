#!/usr/bin/env python3
"""Build a Debian package and a portable tarball without installing anything."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tarfile
import tempfile
from collect_licenses import collect

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser()
parser.add_argument('--binary', default='target/release/file-minnow')
parser.add_argument('--format', choices=['deb', 'tar', 'all'], default='all')
parser.add_argument('--output', default='dist')
parser.add_argument('--maintainer', default='File Minnow contributors')
parser.add_argument('--version')
args = parser.parse_args()
binary = Path(args.binary).resolve()
version = args.version or re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / 'Cargo.toml').read_text(), re.M)[1]
if not re.fullmatch(r'[0-9][0-9A-Za-z.+~:-]*', version):
    raise SystemExit('Invalid package version')
data = binary.read_bytes()
if data[:4] != b'\x7fELF' or data[5] != 1:
    raise SystemExit('Expected a little-endian Linux ELF binary')
machine = struct.unpack_from('<H', data, 18)[0]
architecture = {62: 'amd64', 183: 'arm64'}.get(machine)
if architecture is None:
    raise SystemExit(f'No Debian architecture mapping for ELF machine {machine}')
symbols = subprocess.check_output(['objdump', '-T', str(binary)], text=True)
versions = set(re.findall(r'GLIBC_(\d+\.\d+(?:\.\d+)?)', symbols))
minimum_glibc = max(versions, key=lambda s: tuple(map(int, s.split('.')))) if versions else None
epoch = int(os.environ.get('SOURCE_DATE_EPOCH', '1791331200'))
output = Path(args.output).resolve()
output.mkdir(parents=True, exist_ok=True)
artifacts = []

with tempfile.TemporaryDirectory(prefix='file-minnow-package-') as temp:
    base = Path(temp)
    stage = base / 'stage'
    def put(source, target, mode=0o644):
        dest = stage / target
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, dest)
        dest.chmod(mode)
    put(binary, 'usr/bin/file-minnow', 0o755)
    put(ROOT / 'packaging/file-minnow.desktop', 'usr/share/applications/org.fileminnow.FileMinnow.desktop')
    put(ROOT / 'packaging/file-minnow.svg', 'usr/share/icons/hicolor/scalable/apps/file-minnow.svg')
    for size in [16, 22, 24, 32, 48, 64, 128, 256]:
        put(ROOT / f'packaging/icons/file-minnow-{size}.png', f'usr/share/icons/hicolor/{size}x{size}/apps/file-minnow.png')
    put(ROOT / 'packaging/file-minnow.service', 'usr/lib/systemd/user/file-minnow.service')
    put(ROOT / 'packaging/file-minnow-autostart.desktop', 'usr/share/doc/file-minnow/file-minnow-autostart.desktop')
    put(ROOT / 'LICENSE', 'usr/share/doc/file-minnow/copyright')
    put(ROOT / 'docs/manual.md', 'usr/share/doc/file-minnow/README.md')
    if (ROOT / 'docs/install.md').exists():
        put(ROOT / 'docs/install.md', 'usr/share/doc/file-minnow/install.md')
    collect(ROOT, stage / 'usr/share/doc/file-minnow/licenses', 'aarch64-unknown-linux-gnu' if architecture == 'arm64' else 'x86_64-unknown-linux-gnu')
    for path in stage.rglob('*'):
        if path.is_dir():
            path.chmod(0o755)
        elif path.is_file():
            path.chmod(0o755 if path == stage / 'usr/bin/file-minnow' else 0o644)
        os.utime(path, (epoch, epoch))

    if args.format in ['deb', 'all']:
        control_dir = stage / 'DEBIAN'
        control_dir.mkdir()
        control_dir.chmod(0o755)
        # The binary links GTK 3 and GLib directly; GTK pulls in Pango, Cairo,
        # gdk-pixbuf and its X11/Wayland backends. Distributions that renamed
        # these packages (the t64 transition) still provide the names below.
        dependencies = [f'libc6 (>= {minimum_glibc})' if minimum_glibc else 'libc6', 'libgcc-s1', 'libgtk-3-0 (>= 3.24)', 'libglib2.0-0 (>= 2.56)', 'xdg-utils']
        size_kib = (sum(p.stat().st_size for p in stage.rglob('*') if p.is_file()) + 1023) // 1024
        control = '\n'.join([f'Package: file-minnow', f'Version: {version}', 'Section: utils', 'Priority: optional', f'Architecture: {architecture}', f'Maintainer: {args.maintainer}', f'Installed-Size: {size_kib}', 'Depends: ' + ', '.join(dependencies), 'Recommends: dbus-user-session | dbus-x11, xdg-desktop-portal', 'Description: Instant file search for Linux (Everything alternative)', ' Finds files and folders as you type across millions of files, with a live', ' inotify index, native GTK interface, type filters, file manager actions,', ' global hotkey, tray icon and command-line search.', ' No service or autostart entry is enabled automatically.', ''])
        (control_dir / 'control').write_text(control)
        # No maintainer scripts: install/remove cannot start processes or alter user settings.
        deb = output / f'file-minnow_{version}_{architecture}.deb'
        subprocess.run(['dpkg-deb', '--root-owner-group', '-Zxz', '--build', str(stage), str(deb)], env={**os.environ, 'SOURCE_DATE_EPOCH': str(epoch)}, check=True)
        artifacts.append(deb)
        shutil.rmtree(control_dir)

    if args.format in ['tar', 'all']:
        top = f'file-minnow-{version}-linux-{architecture}'
        archive = output / f'{top}.tar.gz'
        # GNU-compatible tar, with normalized owners/timestamps and deterministic gzip header.
        with archive.open('wb') as raw:
            with gzip.GzipFile(fileobj=raw, mode='wb', filename='', mtime=epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode='w', format=tarfile.PAX_FORMAT) as tar:
                    for path in sorted(stage.rglob('*')):
                        info = tar.gettarinfo(str(path), arcname=top + '/' + str(path.relative_to(stage)))
                        info.uid = info.gid = 0
                        info.uname = info.gname = 'root'
                        info.mtime = epoch
                        info.pax_headers = {}
                        if path.is_file():
                            with path.open('rb') as payload:
                                tar.addfile(info, payload)
                        else:
                            tar.addfile(info)
        artifacts.append(archive)

manifest = {'version': version, 'architecture': architecture, 'minimum_glibc': minimum_glibc, 'binary_sha256': hashlib.sha256(data).hexdigest(), 'source_date_epoch': epoch, 'artifacts': [{'file': p.name, 'bytes': p.stat().st_size, 'sha256': hashlib.sha256(p.read_bytes()).hexdigest()} for p in artifacts]}
(output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps(manifest, indent=2))
