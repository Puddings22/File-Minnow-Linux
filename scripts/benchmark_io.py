#!/usr/bin/env python3
"""Measure indexing and daemon idle cost on isolated temporary files. No GUI."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

p = argparse.ArgumentParser()
p.add_argument('--binary', default='target/release/file-minnow')
p.add_argument('--files', type=int, default=10000)
p.add_argument('--output', default='artifacts/io-benchmark.json')
args = p.parse_args()
binary = str(Path(args.binary).resolve())

with tempfile.TemporaryDirectory(prefix='file-minnow-bench-') as temp:
    base = Path(temp)
    files, cache = base / 'files', base / 'cache'
    files.mkdir()
    for i in range(args.files):
        parent = files / f'{i % 100:03}'
        parent.mkdir(exist_ok=True)
        (parent / f'report-{i:07}.txt').write_bytes(b'fixture\n')
    command = [binary, '--data-dir', str(cache)]
    started = time.monotonic()
    result = subprocess.run(command + ['index', '--root', str(files)], capture_output=True, text=True, check=True)
    index_seconds = time.monotonic() - started
    started = time.monotonic()
    subprocess.run(command + ['search', 'report-0000001', '--json'], capture_output=True, check=True)
    saved_search_seconds = time.monotonic() - started
    daemon = subprocess.Popen(command + ['daemon', '--root', str(files)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        def request(message):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(str(cache / 'search.sock'))
                client.sendall(json.dumps(message).encode() + b'\n')
                with client.makefile('rb') as response:
                    return json.loads(response.readline())

        deadline = time.monotonic() + 30
        while True:
            try:
                status = request({'command': 'status'})['status']
                if status['generation'] >= 2 and not status['scanning']:
                    break
            except (OSError, ValueError):
                pass
            if time.monotonic() > deadline:
                raise RuntimeError('Daemon did not finish startup')
            time.sleep(.05)

        # Allow remaining fixture startup events to settle before idle sampling.
        time.sleep(.5)
        def ticks():
            fields = Path(f'/proc/{daemon.pid}/stat').read_text().split(') ', 1)[1].split()
            return int(fields[11]) + int(fields[12])
        before = ticks()
        idle_start = time.monotonic()
        time.sleep(3)
        idle_seconds = time.monotonic() - idle_start
        cpu_seconds = (ticks() - before) / os.sysconf('SC_CLK_TCK')
        memory = {line.split(':')[0]: line.split(':')[1].strip() for line in Path(f'/proc/{daemon.pid}/status').read_text().splitlines() if line.startswith(('VmRSS:', 'VmHWM:', 'Threads:'))}
        live = request({'command':'search','query':'report-0000001','sort':'Name','descending':False,'limit':100,'offset':0})
        report = {'fixture_files':args.files,'indexed_entries':status['entries'],'index_and_save_seconds':index_seconds,'saved_cli_search_seconds':saved_search_seconds,'live_query_ms':live['search']['elapsed_ms'],'idle_sample_seconds':idle_seconds,'idle_cpu_seconds':cpu_seconds,'idle_cpu_percent_one_core':100*cpu_seconds/idle_seconds,'daemon_memory':memory,'database_bytes':(cache/'index.bin').stat().st_size,'index_stdout':result.stdout.strip(),'scope':'Temporary local files. GUI memory and rendering are not included.'}
        Path(args.output).parent.mkdir(parents=True, exist_ok=True)
        Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait()
