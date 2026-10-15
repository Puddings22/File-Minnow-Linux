#!/usr/bin/env python3
"""A drop target on the test display: writes the URIs dropped on it to a file.

Run: drop_fixture.py OUTPUT X Y
"""
import sys

import gi
gi.require_version('Gtk', '3.0')
gi.require_version('Gdk', '3.0')
from gi.repository import Gdk, Gtk

output, x, y = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
window = Gtk.Window(title='Minnow Drop Target')
window.set_default_size(240, 160)
window.move(x, y)
window.add(Gtk.Label(label='Drop files here'))
window.drag_dest_set(Gtk.DestDefaults.ALL, [Gtk.TargetEntry.new('text/uri-list', 0, 0)],
                     Gdk.DragAction.COPY | Gdk.DragAction.MOVE)


def received(_widget, _context, _x, _y, data, _info, _time):
    with open(output, 'w') as f:
        f.write('\n'.join(data.get_uris()))


def mapped(*_):
    print('ready', flush=True)
    return False


window.connect('drag-data-received', received)
window.connect('map-event', mapped)
window.connect('destroy', Gtk.main_quit)
window.show_all()
Gtk.main()
