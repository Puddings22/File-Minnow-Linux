#!/usr/bin/env python3
"""Retain dependency license/notice files from the locked local Cargo sources."""
import argparse
import json
import re
from pathlib import Path
import shutil
import subprocess

def collect(root, output, target='x86_64-unknown-linux-gnu'):
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--offline', '--format-version', '1', '--filter-platform', target], cwd=root, text=True))
    # metadata retains optional locked packages that are not linked. cargo tree
    # evaluates the current default feature set as used by the release builder.
    tree = subprocess.check_output(['cargo', 'tree', '--locked', '--offline', '--target', target, '--edges', 'normal,build', '--prefix', 'none', '--format', '{p}'], cwd=root, text=True)
    used = {(m[1], m[2]) for line in tree.splitlines() if (m := re.match(r'^(\S+) v([^\s]+)', line))}
    output.mkdir(parents=True, exist_ok=True)
    records = []
    for package in metadata['packages']:
        if not package.get('source') or (package['name'], package['version']) not in used:
            continue
        base = Path(package['manifest_path']).parent
        candidates = [p for p in base.iterdir() if p.is_file() and p.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE', 'COPYRIGHT', 'UNLICENSE'))]
        for subdir in ['licenses', 'LICENSES', 'license']:
            folder = base / subdir
            if folder.is_dir():
                candidates.extend(p for p in folder.rglob('*') if p.is_file())
        if package.get('license_file'):
            path = base / package['license_file']
            if path.is_file() and path not in candidates:
                candidates.append(path)
        # Some small crates put their copyright/license declaration in README.
        if not candidates:
            candidates = [p for p in base.glob('README*') if p.is_file()]
        destination = output / f"{package['name']}-{package['version']}"
        copied = []
        for source in sorted(set(candidates)):
            relative = source.relative_to(base)
            target_path = destination / relative
            target_path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target_path)
            copied.append(str(relative))
        records.append({'name':package['name'], 'version':package['version'], 'license':package.get('license'), 'notices':copied})
    (output / 'dependencies.json').write_text(json.dumps(records, indent=2) + '\n')
    (output / 'README.txt').write_text('Notices from the locked Cargo normal/build dependency graph for ' + target + '. Build-time dependencies are included conservatively. System libraries are supplied separately by the operating system.\n')
    return records

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('output')
    args = parser.parse_args()
    records = collect(Path(__file__).resolve().parent.parent, Path(args.output))
    print(json.dumps({'packages':len(records), 'without_notice_files':[r['name'] for r in records if not r['notices']]}, indent=2))
