#!/usr/bin/env python3
"""Focus test under Cinnamon's window manager (Muffin), on Xvfb only.

Checks that the global shortcut, the tray/second launch and Escape behave
with real focus-stealing prevention: after the shortcut the File Minnow
window is the active window and typing goes straight into the search box.
Muffin runs only on the private virtual display, with a private D-Bus
session, in-memory GSettings and no session-manager connection.

Run: python3 scripts/test_focus.py --xvfb /path/to/Xvfb [--binary ...]
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
parser.add_argument('--wm', default='muffin')
parser.add_argument('--shortcut', default='Ctrl+KP_Multiply')
parser.add_argument('--output', default='artifacts/ui-focus')
args = parser.parse_args()
# xdotool: lower-case modifiers, keysym names as X spells them (KP_Multiply, k).
xdotool_chord = '+'.join(part.lower() if part in ('Ctrl', 'Alt', 'Shift', 'Super') or len(part) == 1 else part for part in args.shortcut.split('+'))
if os.environ.get('MINNOW_PRIVATE_TEST_SESSION') != '1':
    with tempfile.TemporaryDirectory(prefix='minnow-private-session-') as isolated:
        private_env = os.environ.copy()
        for name, folder in [('XDG_RUNTIME_DIR', 'runtime'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache')]:
            path = Path(isolated) / folder
            path.mkdir(mode=0o700)
            private_env[name] = str(path)
        private_env.update(MINNOW_PRIVATE_TEST_SESSION='1', GIO_USE_VFS='local', GTK_USE_PORTAL='0', GSETTINGS_BACKEND='memory')
        private_env.pop('SESSION_MANAGER', None)
        result = subprocess.run(['dbus-run-session', '--', sys.executable, __file__, *sys.argv[1:]], env=private_env)
        raise SystemExit(result.returncode)

output = Path(args.output).resolve()
output.mkdir(parents=True, exist_ok=True)
binary = str(Path(args.binary).resolve())
read_fd, write_fd = os.pipe()
with (output / 'xvfb.log').open('wb') as xvfb_log:
    display = subprocess.Popen([args.xvfb, '-displayfd', str(write_fd), '-screen', '0', '1280x800x24', '-nolisten', 'tcp'], pass_fds=(write_fd,), stdout=xvfb_log, stderr=xvfb_log)
os.close(write_fd)
children = []
results = {}
try:
    if not select.select([read_fd], [], [], 10)[0]:
        raise RuntimeError('Virtual display did not start')
    number = os.read(read_fd, 80).decode().strip()
    env = os.environ.copy()
    env.update(DISPLAY=f':{number}', XDG_SESSION_TYPE='x11', LIBGL_ALWAYS_SOFTWARE='1')
    env.pop('WAYLAND_DISPLAY', None)
    subprocess.run(['dbus-update-activation-environment', 'DISPLAY=' + env['DISPLAY'], 'XDG_SESSION_TYPE=x11'], env=env, check=True)

    def xdo(*parts):
        return subprocess.check_output(['xdotool', *parts], env=env, text=True, timeout=10, stderr=subprocess.DEVNULL).strip()

    def active_pid():
        try:
            return int(xdo('getactivewindow', 'getwindowpid'))
        except (subprocess.CalledProcessError, ValueError):
            return None

    with (output / 'wm.log').open('wb') as wm_log:
        wm = subprocess.Popen([args.wm, '--replace', '--sm-disable', '--x11', '--display', env['DISPLAY']] if args.wm == 'muffin' else [args.wm, '--replace', '--sm-disable'], env=env, stdout=wm_log, stderr=wm_log)
    children.append(wm)
    deadline = time.monotonic() + 15
    while subprocess.run(['xprop', '-root', '_NET_SUPPORTING_WM_CHECK'], env=env, capture_output=True, text=True).stdout.find('window id') < 0:
        if wm.poll() is not None or time.monotonic() > deadline:
            raise RuntimeError(f'{args.wm} did not start; see {output / "wm.log"}')
        time.sleep(.1)
    for fixture in ['tray_fixture.py', 'portal_fixture.py']:
        child = subprocess.Popen([sys.executable, str(Path(__file__).with_name(fixture))], env=env, stdout=subprocess.PIPE, text=True)
        children.append(child)
        if not select.select([child.stdout], [], [], 5)[0] or child.stdout.readline().strip() != 'ready':
            raise RuntimeError(f'{fixture} failed to start')
    with tempfile.TemporaryDirectory(prefix='file-minnow-focus-') as temp:
        base = Path(temp)
        files, cache = base / 'files', base / 'cache'
        files.mkdir()
        cache.mkdir()
        (files / 'invoice.pdf').write_text('fixture')
        paths = json.loads(subprocess.check_output([binary, '--data-dir', str(cache), 'paths'], text=True))
        Path(paths['settings']).write_text(json.dumps({'ui': {'global_shortcut': args.shortcut, 'tray_enabled': True, 'close_to_tray': True}}))
        with (output / 'application.log').open('wb') as log:
            app = subprocess.Popen([binary, '--data-dir', str(cache), 'gui', '--root', str(files)], env=env, stdout=log, stderr=log)
        children.append(app)

        def window_state(action='status', **kwargs):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(paths['ui_socket'])
                client.sendall(json.dumps({'action': action, **kwargs}).encode() + b'\n')
                with client.makefile('rb') as response:
                    return json.loads(response.readline())

        def wait(predicate, what, seconds=8):
            deadline = time.monotonic() + seconds
            while True:
                if app.poll() is not None:
                    raise RuntimeError(f'GUI exited: {app.returncode}')
                try:
                    if predicate():
                        return
                except (OSError, ValueError):
                    pass
                if time.monotonic() > deadline:
                    raise AssertionError(what)
                time.sleep(.05)

        wait(lambda: window_state()['visible'] and window_state()['tray_available'] and '(X11)' in window_state()['hotkey_status'], 'window, tray and X11 shortcut ready', 25)
        wait(lambda: active_pid() == app.pid, 'File Minnow active after launch')
        # Another application takes the focus through a user click.
        other = subprocess.Popen(['xmessage', '-geometry', '600x400+50+50', 'other application'], env=env)
        children.append(other)
        other_window = xdo('search', '--sync', '--onlyvisible', '--class', 'Xmessage').splitlines()[0]
        other_active = lambda: xdo('getactivewindow') == other_window
        wait(other_active, 'other window active')
        xdo('mousemove', '--window', other_window, '200', '150', 'click', '1')

        def typed_into_search(text):
            xdo('type', '--delay', '20', text)
            wait(lambda: window_state()['query'].endswith(text), f'typing {text!r} reached the search box (query: {window_state()["query"]!r})', 3)

        def check(name, trigger, start_hidden):
            if start_hidden:
                window_state('hide')
                wait(lambda: not window_state()['visible'], 'hidden')
            xdo('windowactivate', '--sync', other_window)
            xdo('mousemove', '--window', other_window, '200', '150', 'click', '1')
            wait(other_active, 'other window active again')
            time.sleep(.2)
            trigger()
            try:
                wait(lambda: window_state()['visible'] and active_pid() == app.pid, f'{name}: File Minnow became the active window', 4)
                typed_into_search(name[:3])
                results[name] = True
            except AssertionError as error:
                results[name] = f'FAILED: {error}'

        check('shortcut_from_hidden', lambda: xdo('key', xdotool_chord), True)
        check('shortcut_from_behind', lambda: xdo('key', xdotool_chord), False)
        check('second_launch_from_hidden', lambda: subprocess.run([binary, '--data-dir', str(cache), 'show'], env=env, timeout=10, capture_output=True), True)
        # Escape closes (to the tray here).
        window_state('show')
        wait(lambda: window_state()['visible'], 'shown before Escape')
        ours = xdo('search', '--sync', '--onlyvisible', '--pid', str(app.pid), '--name', '^File Minnow$').splitlines()[0]
        xdo('windowactivate', '--sync', ours)
        xdo('key', 'Escape')
        try:
            wait(lambda: not window_state()['visible'], 'Escape hid the window', 3)
            results['escape_closes_window'] = True
        except AssertionError as error:
            results['escape_closes_window'] = f'FAILED: {error}'
    (output / 'result.json').write_text(json.dumps({'window_manager': args.wm, 'binary': binary, **results}, indent=2) + '\n')
    print(json.dumps(results, indent=2))
    if any(value is not True for value in results.values()):
        raise SystemExit(1)
finally:
    for child in reversed(children):
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(5)
            except subprocess.TimeoutExpired:
                child.kill()
    display.terminate()
    display.wait(5)
