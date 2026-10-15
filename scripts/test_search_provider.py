#!/usr/bin/env python3
"""GNOME Shell and KRunner search through File Minnow's D-Bus provider, on a
private D-Bus session started by D-Bus activation like the desktops do.

Run: python3 scripts/test_search_provider.py [--binary target/debug/file-minnow]
"""
import argparse
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--binary', default='target/debug/file-minnow')
args = parser.parse_args()
binary = str(Path(args.binary).resolve())
if os.environ.get('MINNOW_PRIVATE_TEST_SESSION') != '1':
    with tempfile.TemporaryDirectory(prefix='minnow-search-provider-') as isolated:
        env = os.environ.copy()
        for name, folder in [('XDG_RUNTIME_DIR', 'runtime'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache')]:
            path = Path(isolated) / folder
            path.mkdir(mode=0o700)
            env[name] = str(path)
        files, index = Path(isolated) / 'files', Path(isolated) / 'index'
        files.mkdir()
        index.mkdir()
        # The provider's D-Bus activation file, as the package installs it,
        # pointing at this build and a private index.
        services = Path(env['XDG_DATA_HOME']) / 'dbus-1' / 'services'
        services.mkdir(parents=True)
        (services / 'org.fileminnow.FileMinnow.SearchProvider.service').write_text(
            f'[D-BUS Service]\nName=org.fileminnow.FileMinnow.SearchProvider\nExec={binary} --data-dir {index} search-provider\n')
        env.update(MINNOW_PRIVATE_TEST_SESSION='1', MINNOW_FILES=str(files), MINNOW_INDEX=str(index), GIO_USE_VFS='local', HOME=isolated)
        env.pop('DBUS_SESSION_BUS_ADDRESS', None)
        result = subprocess.run(['dbus-run-session', '--', sys.executable, __file__, '--binary', binary], env=env)
        raise SystemExit(result.returncode)

import gi
gi.require_version('Gio', '2.0')
from gi.repository import Gio, GLib

files, index = Path(os.environ['MINNOW_FILES']), Path(os.environ['MINNOW_INDEX'])
(files / 'reports').mkdir()
for name in ['reports/invoice-2026.pdf', 'reports/old-invoice.pdf', 'notes.txt', 'Invoices to send.md']:
    (files / name).write_text('fixture')
subprocess.run([binary, '--data-dir', str(index), 'index', '--root', str(files)], check=True, stdout=subprocess.DEVNULL)
# A private default application for PDF files records what it opens.
marker = files.parent / 'opened'
applications = Path(os.environ['XDG_DATA_HOME']) / 'applications'
applications.mkdir(parents=True)
(applications / 'minnow-viewer.desktop').write_text(f"[Desktop Entry]\nType=Application\nName=Viewer\nExec=sh -c 'echo \"$1\" > {marker}' viewer %f\nMimeType=application/pdf;\n")
(Path(os.environ['XDG_CONFIG_HOME']) / 'mimeapps.list').write_text('[Default Applications]\napplication/pdf=minnow-viewer.desktop\n')
fixture = subprocess.Popen([sys.executable, str(Path(__file__).with_name('portal_fixture.py'))], stdout=subprocess.PIPE, text=True)
if not select.select([fixture.stdout], [], [], 5)[0] or fixture.stdout.readline().strip() != 'ready':
    raise SystemExit('file manager fixture did not start')

bus = Gio.bus_get_sync(Gio.BusType.SESSION)
NAME = 'org.fileminnow.FileMinnow.SearchProvider'
def call(path, interface, method, parameters, reply):
    result = bus.call_sync(NAME, path, interface, method, parameters, GLib.VariantType(reply) if reply else None, Gio.DBusCallFlags.NONE, 10000)
    return result.unpack() if result else None
def gnome(method, parameters, reply):
    return call('/org/fileminnow/FileMinnow/SearchProvider', 'org.gnome.Shell.SearchProvider2', method, parameters, reply)
def krunner(method, parameters, reply):
    return call('/org/fileminnow/FileMinnow/KRunner', 'org.kde.krunner1', method, parameters, reply)
checks = {}
try:
    # GNOME Shell: names starting with the word come first.
    (ids,) = gnome('GetInitialResultSet', GLib.Variant('(as)', (['invoice'],)), '(as)')
    names = [Path(GLib.filename_from_uri(i)[0]).name for i in ids]
    assert names[:2] == ['invoice-2026.pdf', 'Invoices to send.md'] and 'old-invoice.pdf' in names, names
    (narrowed,) = gnome('GetSubsearchResultSet', GLib.Variant('(asas)', (ids, ['invoice', '2026'])), '(as)')
    assert [Path(GLib.filename_from_uri(i)[0]).name for i in narrowed] == ['invoice-2026.pdf'], narrowed
    (metas,) = gnome('GetResultMetas', GLib.Variant('(as)', (narrowed,)), '(aa{sv})')
    assert metas[0]['name'] == 'invoice-2026.pdf' and metas[0]['description'] == '~/files/reports', metas
    assert metas[0]['icon'], 'no icon'
    checks['gnome_results'] = True
    gnome('ActivateResult', GLib.Variant('(sasu)', (narrowed[0], ['invoice'], 0)), None)
    deadline = time.monotonic() + 5
    while not marker.exists() or not marker.read_text().strip():
        assert time.monotonic() < deadline, 'ActivateResult did not open the file'
        time.sleep(.05)
    assert marker.read_text().strip() == str(files / 'reports/invoice-2026.pdf')
    checks['gnome_activate_opens_default_app'] = True
    assert gnome('GetInitialResultSet', GLib.Variant('(as)', (['x'],)), '(as)') == ([],), 'one letter searched'
    # KRunner: matches with a subtext and URLs, and a folder action.
    (matches,) = krunner('Match', GLib.Variant('(s)', ('invoice 2026',)), '(a(sssida{sv}))')
    assert len(matches) == 1, matches
    match_id, text, icon, kind, relevance, properties = matches[0]
    assert text == 'invoice-2026.pdf' and icon and 0 < relevance <= 1, matches
    assert properties['subtext'] == '~/files/reports' and properties['urls'] == [match_id], properties
    (actions,) = krunner('Actions', None, '(a(sss))')
    assert actions[0][0] == 'show-in-folder', actions
    krunner('SetActivationToken', GLib.Variant('(s)', ('token-1',)), None)
    krunner('Run', GLib.Variant('(ss)', (match_id, 'show-in-folder')), None)
    file_manager = bus.call_sync('org.freedesktop.FileManager1', '/org/freedesktop/FileManager1', 'org.fileminnow.TestPortal', 'Calls', None, GLib.VariantType('(a(sas))'), Gio.DBusCallFlags.NONE, 5000).unpack()[0]
    assert file_manager == [('ShowItems', [match_id])], file_manager
    krunner('Teardown', None, None)
    checks['krunner_match_and_actions'] = True
    # With File Minnow running, results are live.
    daemon = subprocess.Popen([binary, '--data-dir', str(index), 'daemon', '--root', str(files)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        (files / 'invoice-live.pdf').write_text('new')
        while True:
            (ids,) = gnome('GetInitialResultSet', GLib.Variant('(as)', (['invoice-live'],)), '(as)')
            if ids:
                break
            assert time.monotonic() < deadline, 'live result never appeared'
            time.sleep(.1)
        checks['live_results_from_running_index'] = True
    finally:
        daemon.terminate()
        daemon.wait(timeout=10)
finally:
    fixture.terminate()
    fixture.wait(timeout=5)
print(checks)
print('Search provider tests passed on a private D-Bus session.')
