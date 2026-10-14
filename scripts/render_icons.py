#!/usr/bin/env python3
"""Renders packaging/file-minnow.svg to the PNG sizes the app embeds and the
package installs, and copies the SVG to the snap.

Run after changing the icon: python3 scripts/render_icons.py
Needs Python's gi with GdkPixbuf and its SVG loader (librsvg).
"""
import shutil
from pathlib import Path

import gi
gi.require_version('GdkPixbuf', '2.0')
from gi.repository import GdkPixbuf

ROOT = Path(__file__).resolve().parent.parent
SVG = ROOT / 'packaging/file-minnow.svg'
SIZES = [16, 22, 24, 32, 48, 64, 128, 256]


def main():
    for size in SIZES:
        pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_size(str(SVG), size, size)
        assert (pixbuf.get_width(), pixbuf.get_height()) == (size, size) and pixbuf.get_has_alpha()
        pixbuf.savev(str(ROOT / f'packaging/icons/file-minnow-{size}.png'), 'png', [], [])
    shutil.copyfile(SVG, ROOT / 'snap/gui/file-minnow.svg')
    print(f'Rendered {len(SIZES)} sizes and updated the snap icon')


if __name__ == '__main__':
    main()
