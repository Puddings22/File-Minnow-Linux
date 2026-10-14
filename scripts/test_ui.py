#!/usr/bin/env python3
"""Native smoke test on Xvfb and a private D-Bus session, never the active desktop.

Run: dbus-run-session -- python3 scripts/test_ui.py --xvfb /path/to/Xvfb
"""
import argparse
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--xvfb', default='Xvfb')
parser.add_argument('--binary', default='target/debug/file-minnow')
parser.add_argument('--output', default='artifacts/ui')
parser.add_argument('--tray', action='store_true')
parser.add_argument('--daemon-first', action='store_true')
parser.add_argument('--portal', action='store_true', help='Test the Wayland shortcut protocol with a private fixture; rendering still uses Xvfb')
parser.add_argument('--confined', action='store_true', help='Run as a snap would: files open, show and trash through the (fixture) desktop portal')
args = parser.parse_args()
if os.environ.get('MINNOW_PRIVATE_TEST_SESSION') != '1':
    # The GTK file chooser can start GVfs in the private session; keep its
    # FUSE mount out of the temporary directory so cleanup works.
    with tempfile.TemporaryDirectory(prefix='minnow-private-session-', ignore_cleanup_errors=True) as isolated:
        private_env=os.environ.copy()
        for name,folder in [('XDG_RUNTIME_DIR','runtime'),('XDG_CONFIG_HOME','config'),('XDG_DATA_HOME','data'),('XDG_CACHE_HOME','cache')]:
            path=Path(isolated)/folder;path.mkdir(mode=0o700);private_env[name]=str(path)
        private_env.update(MINNOW_PRIVATE_TEST_SESSION='1',GIO_USE_VFS='local',GTK_USE_PORTAL='0',GVFS_DISABLE_FUSE='1')
        result=subprocess.run(['dbus-run-session','--',sys.executable,__file__,*sys.argv[1:]],env=private_env)
        raise SystemExit(result.returncode)
if not os.environ.get('DBUS_SESSION_BUS_ADDRESS'):
    raise SystemExit('Run this under dbus-run-session for an isolated bus.')

def socket_paths(binary, data_dir):
    """Ask File Minnow where its sockets are; long data paths use the runtime dir."""
    try:
        paths = json.loads(subprocess.check_output([str(binary), '--data-dir', str(data_dir), 'paths'], text=True, timeout=10))
        return Path(paths['search_socket']), Path(paths['ui_socket'])
    except (OSError, subprocess.SubprocessError, ValueError, KeyError):
        return Path(data_dir) / 'search.sock', Path(data_dir) / 'ui.sock'

output = Path(args.output).resolve()
output.mkdir(parents=True, exist_ok=True)
binary = str(Path(args.binary).resolve())
read_fd, write_fd = os.pipe()
with (output / 'xvfb.log').open('wb') as xvfb_log:
    display = subprocess.Popen([args.xvfb, '-displayfd', str(write_fd), '-screen', '0', '1280x800x24', '-nolisten', 'tcp'], pass_fds=(write_fd,), stdout=xvfb_log, stderr=xvfb_log)
os.close(write_fd)
app = None
tray = None
daemon = None
portal = None
try:
    if not select.select([read_fd], [], [], 10)[0]:
        raise RuntimeError('Virtual display did not start')
    number = os.read(read_fd, 80).decode().strip()
    if not number.isdigit():
        raise RuntimeError('Invalid virtual display identifier')
    env = os.environ.copy()
    env.update(DISPLAY=f':{number}', XDG_SESSION_TYPE='x11', LIBGL_ALWAYS_SOFTWARE='1')
    if args.confined:
        # File Minnow only reads these; nothing runs from this path.
        env.update(SNAP='/snap/file-minnow/x1', SNAP_NAME='file-minnow', SNAP_REAL_HOME=os.environ.get('HOME', '/'))
    env.pop('WAYLAND_DISPLAY', None)
    subprocess.run(['dbus-update-activation-environment', 'DISPLAY=' + env['DISPLAY'], 'XDG_SESSION_TYPE=x11'], env=env, check=True)
    with tempfile.TemporaryDirectory(prefix='file-minnow-ui-') as temp:
        base = Path(temp)
        files, cache = base / 'files', base / 'cache'
        search_socket, ui_socket = socket_paths(binary, cache)
        files.mkdir()
        for name in ['invoice.pdf', 'draft.pdf', 'notes.txt', 'main.rs', 'scratch.tmp']:
            (files / name).write_text('fixture')
        # A Nemo action in the private data dir; File Minnow offers it for .rs files.
        marker = base / 'marker'
        marker.mkdir()
        actions_dir = Path(os.environ['XDG_DATA_HOME']) / 'nemo' / 'actions'
        actions_dir.mkdir(parents=True, exist_ok=True)
        (actions_dir / 'minnow-test.nemo_action').write_text(f'[Nemo Action]\nName=_Zebra test on %N\nExec=cp %F {marker}/\nSelection=notnone\nExtensions=rs;\n')
        # A private desktop application, default for .rs files: it records what
        # it opens and prints noise that must not reach File Minnow's output.
        import gi
        gi.require_version('Gio', '2.0')
        from gi.repository import Gio
        rust_type = Gio.content_type_guess('main.rs', None)[0]
        applications = Path(os.environ['XDG_DATA_HOME']) / 'applications'
        applications.mkdir(parents=True, exist_ok=True)
        (applications / 'minnow-viewer.desktop').write_text(f"[Desktop Entry]\nType=Application\nName=Minnow Test Viewer\nExec=sh -c 'echo noise-from-viewer; echo \"$1\" > {marker}/opened' viewer %f\nMimeType={rust_type};\n")
        (Path(os.environ['XDG_CONFIG_HOME']) / 'mimeapps.list').write_text(f'[Default Applications]\n{rust_type}=minnow-viewer.desktop\n')
        (actions_dir / 'desktop-only.nemo_action').write_text('[Nemo Action]\nName=_Desktop only\nExec=touch /nonexistent\nExtensions=any;\nConditions=desktop;\n')
        if args.tray:
            tray = subprocess.Popen([sys.executable, str(Path(__file__).with_name('tray_fixture.py'))], env=env, stdout=subprocess.PIPE, text=True)
            if not select.select([tray.stdout], [], [], 5)[0] or tray.stdout.readline().strip() != 'ready':
                raise RuntimeError('Private tray host failed to start')
        if args.portal:
            env['XDG_SESSION_TYPE'] = 'wayland'
        # Own the portal name even for X11 tests so GTK cannot auto-start the
        # desktop's full portal/keyring stack in the test bus.
        portal = subprocess.Popen([sys.executable, str(Path(__file__).with_name('portal_fixture.py'))], env=env, stdout=subprocess.PIPE, text=True)
        if not select.select([portal.stdout], [], [], 5)[0] or portal.stdout.readline().strip() != 'ready':
            raise RuntimeError('Private shortcut portal did not start')
        if args.daemon_first:
            daemon = subprocess.Popen([binary, '--data-dir', str(cache), 'daemon', '--root', str(files)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            deadline = time.monotonic() + 10
            while not (search_socket).exists():
                if time.monotonic() > deadline:
                    raise RuntimeError('Daemon did not start')
                time.sleep(.02)
        with (output / 'application.log').open('wb') as log:
            app = subprocess.Popen([binary, '--data-dir', str(cache), 'gui', '--root', str(files)], env=env, stdout=log, stderr=log)

        def request(message):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(str(search_socket))
                client.sendall(json.dumps(message).encode() + b'\n')
                with client.makefile('rb') as result:
                    return json.loads(result.readline())

        deadline = time.monotonic() + 25
        while True:
            if app.poll() is not None:
                raise RuntimeError(f'GUI exited: {app.returncode}')
            try:
                state = request({'command': 'status'})['status']
                if state['generation'] > 0 and not state['scanning']:
                    break
            except (OSError, ValueError):
                pass
            if time.monotonic() > deadline:
                raise RuntimeError('GUI index never became ready')
            time.sleep(.05)

        def xdo(*parts):
            return subprocess.check_output(['xdotool', *parts], env=env, text=True, timeout=10).strip()

        def window_state(action='status', **kwargs):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(str(ui_socket))
                client.sendall(json.dumps({'action': action, **kwargs}).encode() + b'\n')
                with client.makefile('rb') as response:
                    return json.loads(response.readline())

        KEYBOARD_TAB, FILE_TYPES_TAB, INTERFACE_TAB = 6, 2, 7
        def open_tab(options_window, index):
            """Selects an Options tab by keyboard, independent of label widths."""
            xdo('mousemove', '--window', options_window, '40', '29', 'click', '1')
            time.sleep(.1)
            for _ in range(index):
                xdo('key', 'Right')
            time.sleep(.2)

        def wait_window(predicate):
            deadline = time.monotonic() + 8
            while True:
                current = window_state()
                if predicate(current):
                    return current
                if time.monotonic() > deadline:
                    raise RuntimeError(f'Window did not converge: {current}')
                time.sleep(.03)

        window = xdo('search', '--sync', '--onlyvisible', '--name', '^File Minnow$').splitlines()[0]
        xdo('windowfocus', '--sync', window)
        xdo('key', 'ctrl+l', 'ctrl+a')
        xdo('type', '--clearmodifiers', 'ext:pdf !draft')
        wait_window(lambda s: s['query'] == 'ext:pdf !draft' and s['matches'] == 1)
        subprocess.run(['import', '-quiet', '-window', window, str(output / 'search.png')], env=env, check=True)
        result = request({'command': 'search', 'query': 'ext:pdf !draft', 'sort': 'Name', 'descending': False, 'limit': 100, 'offset': 0})
        assert result['search']['total'] == 1
        # A second process must activate the first window, not create another owner.
        subprocess.run([binary, '--data-dir', str(cache), 'show', 'ext:rs'], env=env, check=True, timeout=10)
        wait_window(lambda s: s['query'] == 'ext:rs' and s['matches'] == 1)
        all_named = xdo('search', '--name', '^File Minnow$').splitlines()
        visible_named = xdo('search', '--onlyvisible', '--name', '^File Minnow$').splitlines()
        (output / 'windows.json').write_text(json.dumps({'all_titled': all_named, 'visible_titled': visible_named}, indent=2) + '\n')
        assert len(visible_named) == 1, f'expected one visible window, found {visible_named} (all titled: {all_named})'
        xdo('key', 'ctrl+comma')
        time.sleep(.2)
        options_window = xdo('search', '--sync', '--onlyvisible', '--name', '^File Minnow Options$').splitlines()[0]
        subprocess.run(['import', '-quiet', '-window', options_window, str(output / 'settings.png')], env=env, check=True)
        before_save = request({'command': 'settings'})['settings']['revision']
        xdo('key', 'ctrl+Return')
        deadline = time.monotonic() + 5
        while request({'command': 'settings'})['settings']['revision'] <= before_save:
            if time.monotonic() > deadline:
                raise RuntimeError('Native settings Save shortcut did not persist the draft')
            time.sleep(.03)
        # Record a real key combination in Options > Keyboard (user-reported gap).
        wait_window(lambda s: not s['settings_open'])
        xdo('windowfocus', '--sync', window)
        xdo('key', 'ctrl+comma')
        wait_window(lambda s: s['settings_open'])
        options_window = xdo('search', '--sync', '--onlyvisible', '--name', '^File Minnow Options$').splitlines()[0]
        xdo('windowfocus', '--sync', options_window)
        open_tab(options_window, KEYBOARD_TAB)
        time.sleep(.2)
        xdo('key', 'Tab')  # from the tab label into the shortcut field: focus alone does not record
        time.sleep(.2)
        xdo('key', 'Return')  # keyboard activation starts recording, like a click
        time.sleep(.2)
        xdo('key', 'Escape')  # cancels recording only; the dialog must stay open
        time.sleep(.2)
        assert window_state()['settings_open'], 'Escape while recording closed the Options dialog'
        xdo('key', 'Return')
        time.sleep(.2)
        xdo('key', 'ctrl+KP_Multiply')  # keypad keys, as FSearch users often bind
        time.sleep(.2)
        subprocess.run(['import', '-quiet', '-window', options_window, str(output / 'shortcut.png')], env=env, check=True)
        xdo('key', 'ctrl+Return')
        deadline = time.monotonic() + 5
        while request({'command': 'settings'})['settings']['ui']['global_shortcut'] != 'Ctrl+KP_Multiply':
            if time.monotonic() > deadline:
                raise RuntimeError('Recorded shortcut was not saved: ' + request({'command': 'settings'})['settings']['ui']['global_shortcut'])
            time.sleep(.03)
        wait_window(lambda s: not s['settings_open'])
        # Escape closes Options (user-reported): from the first tab, with the
        # shortcut field merely focused, and after cancelling a recording.
        for case in ['plain', 'focused', 'recording']:
            xdo('windowfocus', '--sync', window)
            xdo('key', 'ctrl+comma')
            wait_window(lambda s: s['settings_open'])
            options_window = xdo('search', '--sync', '--onlyvisible', '--name', '^File Minnow Options$').splitlines()[0]
            xdo('windowfocus', '--sync', options_window)
            if case != 'plain':
                open_tab(options_window, KEYBOARD_TAB)
                time.sleep(.2)
                xdo('key', 'Tab')
                time.sleep(.2)
            if case == 'recording':
                xdo('key', 'Return', 'Escape')
                time.sleep(.2)
                assert window_state()['settings_open'], 'Escape while recording closed the Options dialog'
            xdo('key', 'Escape')
            wait_window(lambda s: not s['settings_open'])
        # Right-click a result row and copy its path through the context menu.
        xdo('windowfocus', '--sync', window)
        xdo('mousemove', '--window', window, '150', '107', 'click', '3')
        time.sleep(.3)
        subprocess.run(['import', '-quiet', '-window', 'root', str(output / 'context-menu.png')], env=env, check=True)
        xdo('key', 'p')  # Copy Full _Path
        time.sleep(.3)
        copied = subprocess.check_output(['xclip', '-o', '-selection', 'clipboard'], env=env, text=True, timeout=5)
        assert copied == str(files / 'main.rs'), f'context menu copied {copied!r}'
        # Live update: a new matching file appears in the open results
        # without losing the user's selection.
        assert window_state()['selected_count'] == 1
        created = time.monotonic()
        (files / 'lib.rs').write_text('fixture')
        live = wait_window(lambda s: s['query'] == 'ext:rs' and s['matches'] == 2)
        live_update_seconds = round(time.monotonic() - created, 3)
        assert live['selected_count'] == 1, f'selection lost on live update: {live}'
        # Window memory: the size is saved about a second after a resize.
        xdo('windowsize', window, '1000', '700')
        memory_file = cache / 'window.json'
        deadline = time.monotonic() + 5
        while True:
            saved = json.loads(memory_file.read_text()) if memory_file.exists() else {}
            if saved.get('width') == 1000 and saved.get('height') == 700:
                break
            assert time.monotonic() < deadline, f'window size not saved: {saved}'
            time.sleep(.05)
        # File manager items in the right-click menu (first row: lib.rs).
        def right_click_first(key):
            xdo('windowfocus', '--sync', window)
            xdo('mousemove', '--window', window, '150', '107', 'click', '1')
            time.sleep(.2)
            xdo('mousemove', '--window', window, '150', '107', 'click', '3')
            time.sleep(.3)
            xdo('key', key)
            time.sleep(.3)
        target = files / 'lib.rs'
        if args.confined:
            # A snap cannot run the file manager's actions, list the desktop's
            # applications or call the file manager: the portal does it.
            right_click_first('z')
            time.sleep(.5)
            assert not (marker / 'lib.rs').exists(), 'Nemo action offered in a snap'
            xdo('key', 'Escape')
            desktop_portal = __import__('dbus').SessionBus().get_object('org.freedesktop.portal.Desktop', '/org/freedesktop/portal/desktop')
            def portal_file_calls():
                return [(str(m), str(p), bool(a)) for m, p, a in desktop_portal.FileCalls(dbus_interface='org.fileminnow.TestPortal')]
            right_click_first('w')  # Open _With…: the portal asks for the application
            right_click_first('f')  # Open Containing _Folder
            right_click_first('o')  # _Open
            expected = [('OpenFile', str(target), True), ('OpenDirectory', str(target), False), ('OpenFile', str(target), False)]
            deadline = time.monotonic() + 5
            while portal_file_calls() != expected:
                assert time.monotonic() < deadline, f'portal calls: {portal_file_calls()}'
                time.sleep(.05)
            right_click_first('e')  # Prop_erties: File Minnow's own summary
            properties = xdo('search', '--sync', '--onlyvisible', '--name', '^File Properties$').splitlines()[0]
            xdo('windowfocus', '--sync', properties)
            xdo('key', 'Escape')
            deadline = time.monotonic() + 5
            while subprocess.run(['xdotool', 'search', '--onlyvisible', '--name', '^File Properties$'], env=env, capture_output=True).returncode == 0:
                assert time.monotonic() < deadline, 'Properties summary did not close'
                time.sleep(.05)
            assert not (marker / 'opened').exists(), 'a host application started from the snap'
        else:
            right_click_first('z')  # the private Nemo action fixture
            deadline = time.monotonic() + 5
            while not (marker / 'lib.rs').exists():
                assert time.monotonic() < deadline, 'Nemo action did not run'
                time.sleep(.05)
            right_click_first('w')  # Open _With: the default application comes first
            xdo('key', 'Return')
            deadline = time.monotonic() + 5
            while not (marker / 'opened').exists() or not (marker / 'opened').read_text().strip():
                assert time.monotonic() < deadline, 'Open With did not start the application'
                time.sleep(.05)
            assert (marker / 'opened').read_text().strip() == str(target)
            time.sleep(.2)
            assert 'noise-from-viewer' not in (output / 'application.log').read_text(), 'launched application wrote into File Minnow output'
            portal_proxy = __import__('dbus').SessionBus().get_object('org.freedesktop.FileManager1', '/org/freedesktop/FileManager1')
            def file_manager_calls():
                return [(str(m), [str(u) for u in uris]) for m, uris in portal_proxy.Calls(dbus_interface='org.fileminnow.TestPortal')]
            right_click_first('e')  # Prop_erties
            right_click_first('f')  # Open Containing _Folder
            deadline = time.monotonic() + 5
            while file_manager_calls() != [('ShowItemProperties', [target.as_uri()]), ('ShowItems', [target.as_uri()])]:
                assert time.monotonic() < deadline, f'file manager calls: {file_manager_calls()}'
                time.sleep(.05)
        # Ctrl+C puts the file itself on the clipboard (pastes in Nemo) and its path as text.
        xdo('key', 'ctrl+c')
        time.sleep(.2)
        clip = lambda target_type: subprocess.check_output(['xclip', '-o', '-selection', 'clipboard', '-t', target_type], env=env, text=True, timeout=5)
        assert clip('x-special/gnome-copied-files') == 'copy\n' + target.as_uri(), clip('x-special/gnome-copied-files')
        assert clip('UTF8_STRING') == str(target)
        # F2 renames (the name without extension is preselected), and the list follows.
        xdo('key', 'F2')
        time.sleep(.4)
        xdo('type', '--delay', '20', 'renamed')
        xdo('key', 'Return')
        deadline = time.monotonic() + 5
        while not (files / 'renamed.rs').exists():
            assert time.monotonic() < deadline, 'rename did not happen'
            time.sleep(.05)
        assert not target.exists()
        wait_window(lambda s: s['matches'] == 2)
        # Delete moves the selection to the (private) Trash.
        xdo('windowfocus', '--sync', window)
        xdo('key', 'ctrl+l')
        xdo('type', '--delay', '10', 'renamed')
        wait_window(lambda s: s['query'] == 'renamed' and s['matches'] == 1)
        xdo('mousemove', '--window', window, '150', '107', 'click', '1')
        time.sleep(.2)
        xdo('key', 'Delete')
        trash = Path(os.environ['XDG_DATA_HOME']) / ('portal-trash' if args.confined else 'Trash/files') / 'renamed.rs'
        deadline = time.monotonic() + 5
        while not trash.exists():
            assert time.monotonic() < deadline, 'Delete did not move the file to the Trash'
            time.sleep(.05)
        wait_window(lambda s: s['matches'] == 0)
        # Type filter next to the search box, and the Type column.
        xdo('windowfocus', '--sync', window)
        xdo('key', 'ctrl+l', 'BackSpace')
        wait_window(lambda s: s['query'] == '')
        width = int(dict(line.split('=', 1) for line in xdo('getwindowgeometry', '--shell', window).splitlines())['WIDTH'])
        def choose_filter(*keys):
            xdo('mousemove', '--window', window, str(width - 50), '42', 'click', '1')
            time.sleep(.4)
            xdo('key', *keys, 'Return')
        choose_filter('Down', 'Down', 'Down')  # All, Audio, Compressed, Document
        wait_window(lambda s: s['filter'] == 'doc:' and s['matches'] == 3)
        time.sleep(.3)
        subprocess.run(['import', '-quiet', '-window', window, str(output / 'type-filter.png')], env=env, check=True)
        choose_filter('Up', 'Up', 'Up')
        wait_window(lambda s: s['filter'] == '')
        # Optional empty list until something is searched (user request).
        def set_ui(**values):
            current = request({'command': 'settings'})['settings']
            current['ui'].update(values)
            assert not request({'command': 'set_settings', 'settings': current})['error']
        all_entries = request({'command': 'search', 'query': '', 'sort': 'Name', 'descending': False, 'limit': 10, 'offset': 0})['search']['total']
        wait_window(lambda s: s['filter'] == '' and s['matches'] == all_entries)
        set_ui(hide_results_until_search=True)
        wait_window(lambda s: s['query'] == '' and s['matches'] == 0 and s['status_text'] == 'Type to search')
        choose_filter('Down', 'Down', 'Down')  # a type filter is a search too
        wait_window(lambda s: s['filter'] == 'doc:' and s['matches'] == 3)
        choose_filter('Up', 'Up', 'Up')
        wait_window(lambda s: s['filter'] == '' and s['matches'] == 0)
        xdo('windowfocus', '--sync', window)
        xdo('key', 'ctrl+l')
        xdo('type', '--delay', '10', 'invoice')
        wait_window(lambda s: s['query'] == 'invoice' and s['matches'] == 1)
        xdo('key', 'ctrl+l', 'BackSpace')
        wait_window(lambda s: s['query'] == '' and s['matches'] == 0)
        set_ui(hide_results_until_search=False)
        wait_window(lambda s: s['query'] == '' and s['matches'] == all_entries)
        # Bookmarks menu (items come from the settings) and a custom filter.
        current = request({'command': 'settings'})['settings']
        current['bookmarks'] = [{'name': 'PDF files', 'query': 'ext:pdf'}]
        current['ui']['filters'] = [{'name': 'Rust code', 'query': 'ext:rs'}]
        assert not request({'command': 'set_settings', 'settings': current})['error']
        time.sleep(.5)
        xdo('windowfocus', '--sync', window)
        xdo('key', 'alt+b')
        time.sleep(.4)
        xdo('key', 'End', 'Return')  # the last item is the bookmark
        wait_window(lambda s: s['query'] == 'ext:pdf' and s['matches'] == 2)
        choose_filter('End')  # the custom filter is listed last
        wait_window(lambda s: s['filter'] == 'ext:rs' and s['matches'] == 0)
        xdo('key', 'ctrl+l', 'BackSpace')
        wait_window(lambda s: s['query'] == '' and s['matches'] == 1)
        choose_filter('Home')
        wait_window(lambda s: s['filter'] == '' and s['matches'] == all_entries)
        # Export the current results to CSV (path typed into the file chooser).
        xdo('type', '--delay', '10', 'pdf')
        wait_window(lambda s: s['query'] == 'pdf' and s['matches'] == 2)
        export_path = base / 'results.csv'
        xdo('key', 'alt+f')
        time.sleep(.4)
        xdo('key', 'e')
        export_dialog = xdo('search', '--sync', '--onlyvisible', '--name', '^Export Results$').splitlines()[0]
        xdo('windowfocus', '--sync', export_dialog)
        time.sleep(.5)
        xdo('key', 'ctrl+a')  # the default name is only partly selected
        xdo('type', '--delay', '10', str(export_path))
        time.sleep(.5)
        subprocess.run(['import', '-quiet', '-window', 'root', str(output / 'export-dialog.png')], env=env, check=True)
        xdo('key', 'Return')
        deadline = time.monotonic() + 8
        while not export_path.exists() or len(export_path.read_text().splitlines()) < 3:
            if time.monotonic() > deadline:
                subprocess.run(['import', '-quiet', '-window', 'root', str(output / 'export-failed.png')], env=env, check=True)
                raise AssertionError('export did not finish')
            time.sleep(.1)
        exported = export_path.read_text().splitlines()
        assert exported[0] == 'Name,Path,Size,Modified,Created,Type' and len(exported) == 3, exported
        assert any(line.startswith('draft.pdf,') for line in exported) and all(',file' in line for line in exported[1:]), exported
        # View menu: Preview Pane on, Created column on; both remembered.
        xdo('windowfocus', '--sync', window)
        xdo('mousemove', '--window', window, '150', '107', 'click', '1')
        for keys in [['End'], ['End', 'Up']]:
            xdo('key', 'alt+v')
            time.sleep(.4)
            xdo('key', *keys, 'Return')
            time.sleep(.4)
        deadline = time.monotonic() + 5
        while True:
            saved = json.loads(memory_file.read_text()) if memory_file.exists() else {}
            if saved.get('preview') is True and 'Created' not in saved.get('hidden_columns', ['Created']):
                break
            assert time.monotonic() < deadline, f'view settings not saved: {saved}'
            time.sleep(.1)
        subprocess.run(['import', '-quiet', '-window', window, str(output / 'preview.png')], env=env, check=True)
        # "Search here" from a file manager: show --in scopes the search to a folder.
        subprocess.run([binary, '--data-dir', str(cache), 'show', '--in', str(files)], env=env, check=True, timeout=10)
        scope = f'"{files.resolve()}/" '
        wait_window(lambda s: s['query'] == scope and s['matches'] == all_entries - 1)
        # The folder stays and the cursor is after it: typing adds a term.
        assert not window_state()['query_selected'], 'Search Here left the folder selected'
        xdo('windowfocus', '--sync', window)
        xdo('type', '--delay', '10', 'invoice')
        wait_window(lambda s: s['query'] == scope + 'invoice' and s['matches'] == 1)
        xdo('key', 'ctrl+l', 'BackSpace')
        wait_window(lambda s: s['query'] == '')
        menu_checks = {'bookmarks_menu': True, 'custom_filter': True, 'export_csv': True, 'view_menu_remembered': True, 'show_in_folder_scope': True, 'search_here_cursor_after_folder': True, 'empty_until_search': True, 'start_at_login_toggle': True, 'type_filter_document': True, 'open_with_default_app': True, 'launched_app_output_detached': True, 'nemo_action_ran': True, 'properties_via_file_manager': True, 'show_items_via_file_manager': True, 'clipboard_files': True, 'rename_f2': True, 'trash_delete': True}
        # Excluded file types: the default list hides *.tmp.
        assert request({'command': 'search', 'query': 'scratch', 'sort': 'Name', 'descending': False, 'limit': 10, 'offset': 0})['search']['total'] == 0, 'default file types were indexed'
        xdo('windowfocus', '--sync', window)
        xdo('key', 'ctrl+comma')
        wait_window(lambda s: s['settings_open'])
        options_window = xdo('search', '--sync', '--onlyvisible', '--name', '^File Minnow Options$').splitlines()[0]
        open_tab(options_window, FILE_TYPES_TAB)
        subprocess.run(['import', '-quiet', '-window', options_window, str(output / 'file-types.png')], env=env, check=True)
        xdo('key', 'Escape')
        wait_window(lambda s: not s['settings_open'])
        # Options > Interface > Start at login writes and removes the XDG
        # autostart entry (private XDG_CONFIG_HOME).
        login_entry = Path(os.environ['XDG_CONFIG_HOME']) / 'autostart' / 'file-minnow.desktop'
        for expected in [True, False]:
            xdo('windowfocus', '--sync', window)
            xdo('key', 'ctrl+comma')
            wait_window(lambda s: s['settings_open'])
            options_window = xdo('search', '--sync', '--onlyvisible', '--name', '^File Minnow Options$').splitlines()[0]
            open_tab(options_window, INTERFACE_TAB)
            xdo('mousemove', '--window', options_window, '35', '168', 'click', '1')  # the login check box
            time.sleep(.2)
            if expected:
                subprocess.run(['import', '-quiet', '-window', options_window, str(output / 'interface.png')], env=env, check=True)
            xdo('key', 'ctrl+Return')
            wait_window(lambda s: not s['settings_open'])
            assert login_entry.exists() == expected, f'login entry exists={login_entry.exists()}'
            if expected:
                text = login_entry.read_text()
                assert 'gui --hidden' in text and '--data-dir' in text, text
        preferences = request({'command': 'settings'})['settings']
        assert preferences['index']['exclude_file_types'] and 'tmp' in preferences['index']['excluded_file_types']
        preferences['index']['excluded_files'] = ['*.txt']
        applied = request({'command': 'set_settings', 'settings': preferences})
        assert not applied['error']
        time.sleep(.35)
        filtered = request({'command': 'search', 'query': 'ext:txt', 'sort': 'Name', 'descending': False, 'limit': 100, 'offset': 0})
        assert filtered['search']['total'] == 0
        # Adding a large folder must show progress immediately (user-reported gap).
        bulk = base / 'bulk'
        for i in range(30000):
            folder = bulk / f'{i % 300:03}'
            folder.mkdir(parents=True, exist_ok=True)
            (folder / f'item-{i:06}.dat').touch()
        preferences = request({'command': 'settings'})['settings']
        preferences['index']['roots'].append({'path': str(bulk), 'enabled': True, 'recursive': True, 'monitor': True, 'schedule': {'kind': 'interval', 'seconds': 300}})
        before_entries = request({'command': 'status'})['status']['entries']
        added_at = time.monotonic()
        assert not request({'command': 'set_settings', 'settings': preferences})['error']
        seen, first_busy = [], None
        while time.monotonic() - added_at < 30:
            text = window_state()['status_text']
            if not seen or seen[-1] != text:
                seen.append(text)
            if first_busy is None and text.startswith(('Preparing', 'Indexing', 'Saving', 'Searching')):
                first_busy = time.monotonic() - added_at
            current = request({'command': 'status'})['status']
            if current['entries'] > before_entries + 30000 and not current['scanning'] and text == 'Ready':
                break
            time.sleep(.01)
        else:
            raise RuntimeError(f'Indexing never finished; status texts seen: {seen}')
        assert first_busy is not None and first_busy < 0.5, f'no immediate progress; status texts seen: {seen}'
        indexing_feedback = {'first_progress_seconds': round(first_busy, 3), 'finished_seconds': round(time.monotonic() - added_at, 2), 'status_texts': seen[:12]}
        desktop_checks = {}
        # Measure after input settles: hotkey/tray integration must not redraw/poll continuously.
        time.sleep(.25)
        def ticks():
            fields = Path(f'/proc/{app.pid}/stat').read_text().split(') ', 1)[1].split()
            return int(fields[11]) + int(fields[12])
        before_ticks = ticks()
        idle_start = time.monotonic()
        time.sleep(3)
        idle_seconds = time.monotonic() - idle_start
        idle_cpu_seconds = (ticks() - before_ticks) / os.sysconf('SC_CLK_TCK')
        gui_memory = {line.split(':')[0]: line.split(':')[1].strip() for line in Path(f'/proc/{app.pid}/status').read_text().splitlines() if line.startswith(('VmRSS:', 'VmHWM:', 'Threads:'))}
        # RSS counts shared desktop libraries in full; PSS and private pages are fairer.
        for line in Path(f'/proc/{app.pid}/smaps_rollup').read_text().splitlines():
            if line.startswith(('Pss:', 'Private_Clean:', 'Private_Dirty:', 'Shared_Clean:')):
                gui_memory[line.split(':')[0]] = line.split(':')[1].strip()
        if args.tray:
            import dbus
            from Xlib import display as xdisplay, protocol, X
            # Hiding the window records the search in the history file.
            xdo('windowfocus', '--sync', window)
            xdo('key', 'ctrl+l')
            xdo('type', '--delay', '10', 'invoice')
            wait_window(lambda s: s['query'] == 'invoice')
            window_state('hide')
            wait_window(lambda s: not s['visible'])
            history_file = cache / 'history.json'
            deadline = time.monotonic() + 5
            while not history_file.exists() or 'invoice' not in history_file.read_text():
                assert time.monotonic() < deadline, 'search history not recorded'
                time.sleep(.1)
            window_state('show')
            # Each opening starts with an empty search (the default).
            wait_window(lambda s: s['visible'] and s['query'] == '')
            # With the option off, the last search stays, selected.
            set_ui(clear_search_on_open=False)
            xdo('windowfocus', '--sync', window)
            xdo('key', 'ctrl+l')
            xdo('type', '--delay', '10', 'notes')
            wait_window(lambda s: s['query'] == 'notes')
            window_state('hide')
            wait_window(lambda s: not s['visible'])
            window_state('show')
            wait_window(lambda s: s['visible'] and s['query'] == 'notes' and s['query_selected'])
            set_ui(clear_search_on_open=True)
            xdo('key', 'ctrl+l', 'BackSpace')
            wait_window(lambda s: s['query'] == '')
            history_recorded = True
            expected = 'desktop portal' if args.portal else '(X11)'
            wait_window(lambda s: s['tray_available'] and expected in s['hotkey_status'])
            if args.portal:
                portal_proxy = dbus.SessionBus().get_object('org.freedesktop.portal.Desktop', '/org/freedesktop/portal/desktop')
                bindings = portal_proxy.Bindings(dbus_interface='org.fileminnow.TestPortal')
                assert str(bindings[0][0]) == 'toggle'
                assert str(bindings[0][1]['trigger_description']) == 'CTRL+KP_Multiply'
                trigger = lambda: portal_proxy.Trigger(dbus_interface='org.fileminnow.TestPortal')
            else:
                trigger = lambda: xdo('key', 'ctrl+KP_Multiply')  # the shortcut recorded above
            # Xvfb has no window manager to return focus after the modal dialog closes.
            xdo('windowfocus', '--sync', window)
            trigger()
            wait_window(lambda s: not s['visible'])
            trigger()
            wait_window(lambda s: s['visible'])
            connection = xdisplay.Display(env['DISPLAY'])
            native = connection.create_resource_object('window', int(window))
            def close_request():
                event = protocol.event.ClientMessage(window=native, client_type=connection.intern_atom('WM_PROTOCOLS'), data=(32, [connection.intern_atom('WM_DELETE_WINDOW'), 0, 0, 0, 0]))
                native.send_event(event, event_mask=X.NoEventMask)
                connection.flush()
            close_request()
            wait_window(lambda s: not s['visible'])
            assert app.poll() is None
            bus = dbus.SessionBus()
            watcher = bus.get_object('org.kde.StatusNotifierWatcher', '/StatusNotifierWatcher')
            items = watcher.Get('org.kde.StatusNotifierWatcher', 'RegisteredStatusNotifierItems', dbus_interface='org.freedesktop.DBus.Properties')
            service_name, item_path = str(items[-1]).split('/', 1)
            item = bus.get_object(service_name, '/' + item_path)
            icon = item.Get('org.kde.StatusNotifierItem', 'IconPixmap', dbus_interface='org.freedesktop.DBus.Properties')
            sizes = {int(width): len(data) for width, height, data in icon}
            assert sizes.get(32) == 32 * 32 * 4 and sizes.get(16) == 16 * 16 * 4, sizes
            item.Activate(0, 0, dbus_interface='org.kde.StatusNotifierItem')
            wait_window(lambda s: s['visible'])
            # Exercise the exported tray menu through the DBusMenu interface.
            menu_path = item.Get('org.kde.StatusNotifierItem', 'Menu', dbus_interface='org.freedesktop.DBus.Properties')
            menu = bus.get_object(service_name, str(menu_path))
            _, layout = menu.GetLayout(0, -1, [], dbus_interface='com.canonical.dbusmenu')
            def find_label(node, label):
                if str(node[1].get('label', '')) == label:
                    return int(node[0])
                for child in node[2]:
                    found = find_label(child, label)
                    if found is not None:
                        return found
                return None
            pause_id = find_label(layout, 'Pause indexing')
            assert pause_id is not None
            menu.Event(pause_id, 'clicked', dbus.Int32(0, variant_level=1), dbus.UInt32(0), dbus_interface='com.canonical.dbusmenu')
            deadline = time.monotonic() + 5
            while not request({'command': 'status'})['status']['paused']:
                assert time.monotonic() < deadline
                time.sleep(.03)
            request({'command': 'pause', 'paused': False})
            # Removing tray support must restore a hidden window.
            window_state('hide')
            wait_window(lambda s: not s['visible'])
            preferences = request({'command': 'settings'})['settings']
            preferences['ui']['tray_enabled'] = False
            request({'command': 'set_settings', 'settings': preferences})
            wait_window(lambda s: s['visible'] and not s['tray_available'])
            close_request()
            app.wait(timeout=8)
            assert app.returncode == 0
            connection.close()
            desktop_checks = {'search_history_recorded': history_recorded, 'empty_search_on_open': history_recorded, 'tray_registration': True, 'tray_icon': True, 'tray_activation': True, 'tray_menu_pause': True, 'x11_shortcut_toggle': not args.portal, 'shortcut_portal_protocol': args.portal, 'native_wayland_rendering': False, 'close_to_tray': True, 'tray_disabled_restores_window': True, 'no_tray_close_quits': True}
        else:
            current = window_state()
            assert not current['tray_available']
            window_state('hide')
            time.sleep(.1)
            assert window_state()['visible']
            window_state('quit')
            app.wait(timeout=8)
            assert app.returncode == 0
        # Relaunch: the window must come back at the saved size.
        assert app.poll() is not None, 'first instance still running'
        with (output / 'application-relaunch.log').open('wb') as relaunch_log:
            app = subprocess.Popen([binary, '--data-dir', str(cache), 'gui', '--root', str(files)], env=env, stdout=relaunch_log, stderr=relaunch_log)
        window = xdo('search', '--sync', '--onlyvisible', '--name', '^File Minnow$').splitlines()[0]
        geometry = dict(line.split('=', 1) for line in xdo('getwindowgeometry', '--shell', window).splitlines())
        assert (geometry['WIDTH'], geometry['HEIGHT']) == ('1000', '700'), f'window size not restored: {geometry}'
        window_state('quit')
        app.wait(timeout=8)
        if args.daemon_first:
            assert daemon.poll() is None
            assert request({'command': 'status'})['status']['generation'] > 0
        (output / 'result.json').write_text(json.dumps({'display':env['DISPLAY'],'isolated_bus':True,'initial_entries':state['entries'],'query_matches':result['search']['total'],'excluded_file_matches':filtered['search']['total'],'native_window_rendered':True,'settings_shortcut_sent':True,'settings_save_verified':True,'shortcut_recorded':'Ctrl+KP_Multiply','escape_closes_options':True,'escape_cancels_recording_only':True,'context_menu_copy_path':True,'live_update_seconds':live_update_seconds,'live_update_keeps_selection':True,**menu_checks,'default_file_types_excluded':True,'window_size_restored':True,'indexing_feedback':indexing_feedback,'single_instance_activation':True,'daemon_coexistence':args.daemon_first,'idle_sample_seconds':idle_seconds,'idle_cpu_seconds':idle_cpu_seconds,'gui_memory':gui_memory,**desktop_checks}, indent=2) + '\n')
        print('Isolated native UI smoke passed; captures are from Xvfb only.')
finally:
    os.close(read_fd)
    if app and app.poll() is None:
        app.terminate()
        try:
            app.wait(timeout=5)
        except subprocess.TimeoutExpired:
            app.kill()
            app.wait()
    for child in [daemon, tray, portal]:
        if child and child.poll() is None:
            child.terminate()
            child.wait(timeout=5)
    display.terminate()
    display.wait(timeout=5)
