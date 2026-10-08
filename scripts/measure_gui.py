#!/usr/bin/env python3
"""Measure the real GUI on a private Xvfb display and D-Bus session.

Starts File Minnow against a real folder, waits for indexing, then samples
memory and idle CPU and times searches through the local socket. Prints
counts and timings only, never file names.
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
parser.add_argument('--xvfb', required=True)
parser.add_argument('--binary', default='target/release/file-minnow')
parser.add_argument('--root', required=True)
parser.add_argument('--data-dir', required=True, help='index cache to use (may already hold an index)')
parser.add_argument('--output', required=True)
args = parser.parse_args()
if os.environ.get('MINNOW_PRIVATE_TEST_SESSION') != '1':
    with tempfile.TemporaryDirectory(prefix='minnow-measure-') as isolated:
        env = os.environ.copy()
        for name, folder in [('XDG_RUNTIME_DIR', 'runtime'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache')]:
            path = Path(isolated) / folder
            path.mkdir(mode=0o700)
            env[name] = str(path)
        env.update(MINNOW_PRIVATE_TEST_SESSION='1', GIO_USE_VFS='local', GTK_USE_PORTAL='0')
        raise SystemExit(subprocess.run(['dbus-run-session', '--', sys.executable, __file__, *sys.argv[1:]], env=env).returncode)

data = Path(args.data_dir)

def socket_paths(binary, data_dir):
    """Ask File Minnow where its sockets are; long data paths use the runtime dir."""
    try:
        paths = json.loads(subprocess.check_output([str(binary), '--data-dir', str(data_dir), 'paths'], text=True, timeout=10))
        return Path(paths['search_socket']), Path(paths['ui_socket'])
    except (OSError, subprocess.SubprocessError, ValueError, KeyError):
        return Path(data_dir) / 'search.sock', Path(data_dir) / 'ui.sock'

search_socket, ui_socket = socket_paths(args.binary, data)
read_fd, write_fd = os.pipe()
display = subprocess.Popen([args.xvfb, '-displayfd', str(write_fd), '-screen', '0', '1280x800x24', '-nolisten', 'tcp'], pass_fds=(write_fd,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
os.close(write_fd)
app = None
try:
    assert select.select([read_fd], [], [], 10)[0], 'no display'
    env = os.environ.copy()
    env.update(DISPLAY=':' + os.read(read_fd, 80).decode().strip(), XDG_SESSION_TYPE='x11')
    env.pop('WAYLAND_DISPLAY', None)

    def request(message):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(30)
            client.connect(str(search_socket))
            client.sendall(json.dumps(message).encode() + b'\n')
            with client.makefile('rb') as result:
                return json.loads(result.readline())

    def memory(pid):
        out = {}
        for line in Path(f'/proc/{pid}/status').read_text().splitlines():
            if line.startswith(('VmRSS:', 'VmHWM:', 'Threads:')):
                out[line.split(':')[0]] = line.split(':')[1].strip()
        for line in Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines():
            if line.startswith(('Pss:', 'Private_Dirty:')):
                out[line.split(':')[0]] = line.split(':')[1].strip()
        return out

    def ticks(pid):
        fields = Path(f'/proc/{pid}/stat').read_text().split(') ', 1)[1].split()
        return int(fields[11]) + int(fields[12])

    # Own the portal name in this private session so GTK cannot start the
    # desktop's real portal services (they mount into the runtime folder).
    portal = subprocess.Popen([sys.executable, str(Path(__file__).with_name('portal_fixture.py'))], env=env, stdout=subprocess.PIPE, text=True)
    assert select.select([portal.stdout], [], [], 5)[0] and portal.stdout.readline().strip() == 'ready', 'portal stand-in failed'
    started = time.monotonic()
    log = open(Path(args.output).with_suffix('.log'), 'wb')
    app = subprocess.Popen([args.binary, '--data-dir', str(data), 'gui', '--root', args.root], env=env, stdout=log, stderr=log)
    first_results = None
    while True:
        assert app.poll() is None, 'GUI exited'
        try:
            status = request({'command': 'status'})['status']
            if first_results is None and status['entries'] > 0:
                first_results = time.monotonic() - started
            if status['generation'] > 0 and not status['scanning']:
                break
        except (OSError, ValueError):
            pass
        assert time.monotonic() - started < 600, 'indexing took over 10 minutes'
        time.sleep(0.1)
    ready = time.monotonic() - started
    peak_during_index = memory(app.pid)
    time.sleep(2)
    before = ticks(app.pid)
    idle_start = time.monotonic()
    time.sleep(10)
    idle_cpu = (ticks(app.pid) - before) / os.sysconf('SC_CLK_TCK') / (time.monotonic() - idle_start)
    queries = {}
    for query in ['readme', 'a', 'ext:rs', '*.json', 'path:src main', 'zzqqxxnotfound']:
        times = []
        for _ in range(5):
            response = request({'command': 'search', 'query': query, 'sort': 'Name', 'descending': False, 'limit': 1000, 'offset': 0})
            times.append(response['search']['elapsed_ms'])
        queries[query] = {'matches': response['search']['total'], 'median_engine_ms': round(sorted(times)[2], 1)}
    report = {
        'entries': status['entries'], 'watch_errors': len(status['watch_errors']), 'traversal_errors': status['error_count'],
        'seconds_until_saved_index_searchable': round(first_results, 2) if first_results else None,
        'seconds_until_rescan_finished': round(ready, 1),
        'memory_after_indexing': peak_during_index, 'memory_idle': memory(app.pid),
        'idle_cpu_percent_of_one_core': round(idle_cpu * 100, 2), 'searches': queries,
        'note': 'Release build on Xvfb. GTK uses software rendering here; RSS includes shared libraries.'}
    Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
finally:
    if app and app.poll() is None:
        try:
            with socket.socket(socket.AF_UNIX) as client:
                client.connect(str(ui_socket))
                client.sendall(b'{"action":"quit"}\n')
            app.wait(timeout=20)
        except Exception:
            app.kill()
    if 'portal' in globals() and portal.poll() is None:
        portal.terminate()
    display.terminate()
    display.wait(timeout=5)
