#!/usr/bin/env python3
"""Protocol fixture for GlobalShortcuts, never a replacement for compositor QA."""
import os
import time
import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
if os.environ.get('MINNOW_PRIVATE_TEST_SESSION') != '1':
    raise SystemExit('A private test bus is required')
DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName('org.freedesktop.portal.Desktop', bus)
objects = []
class Request(dbus.service.Object):
    @dbus.service.signal('org.freedesktop.portal.Request', signature='ua{sv}')
    def Response(self, code, results):
        pass
    @dbus.service.method('org.freedesktop.portal.Request', in_signature='', out_signature='')
    def Close(self):
        pass
class Session(dbus.service.Object):
    @dbus.service.method('org.freedesktop.portal.Session', in_signature='', out_signature='')
    def Close(self):
        self.remove_from_connection()
class Portal(dbus.service.Object):
    def __init__(self):
        super().__init__(bus, '/org/freedesktop/portal/desktop')
        self.session = None
        self.shortcuts = []
    def response(self, sender, options, values):
        path = '/org/freedesktop/portal/desktop/request/' + sender[1:].replace('.', '_') + '/' + str(options['handle_token'])
        request = Request(bus, path)
        objects.append(request)
        GLib.timeout_add(20, lambda: (request.Response(0, values), False)[1])
        return dbus.ObjectPath(path)
    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='ss', out_signature='v')
    def Get(self, interface, key):
        return dbus.UInt32(2)
    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        return {'version': dbus.UInt32(2)}
    @dbus.service.method('org.freedesktop.portal.GlobalShortcuts', in_signature='a{sv}', out_signature='o', sender_keyword='sender')
    def CreateSession(self, options, sender=None):
        path = '/org/freedesktop/portal/desktop/session/' + sender[1:].replace('.', '_') + '/' + str(options['session_handle_token'])
        objects.append(Session(bus, path))
        self.session = dbus.ObjectPath(path)
        return self.response(sender, options, {'session_handle': dbus.String(path)})
    @dbus.service.method('org.freedesktop.portal.GlobalShortcuts', in_signature='oa(sa{sv})sa{sv}', out_signature='o', sender_keyword='sender')
    def BindShortcuts(self, session, shortcuts, parent, options, sender=None):
        self.shortcuts = [(str(key), {'description': str(info['description']), 'trigger_description': str(info.get('preferred_trigger', ''))}) for key, info in shortcuts]
        return self.response(sender, options, {'shortcuts': dbus.Array(self.shortcuts, signature='(sa{sv})')})
    @dbus.service.signal('org.freedesktop.portal.GlobalShortcuts', signature='osta{sv}')
    def Activated(self, session, shortcut, timestamp, options):
        pass
    @dbus.service.method('org.fileminnow.TestPortal', in_signature='', out_signature='')
    def Trigger(self):
        self.Activated(self.session, 'toggle', dbus.UInt64(int(time.monotonic() * 1000)), {})
    @dbus.service.method('org.fileminnow.TestPortal', in_signature='', out_signature='a(sa{sv})')
    def Bindings(self):
        return dbus.Array(self.shortcuts, signature='(sa{sv})')
class FileManager(dbus.service.Object):
    """Records org.freedesktop.FileManager1 calls (Nemo, Nautilus... in real sessions)."""
    def __init__(self):
        super().__init__(bus, '/org/freedesktop/FileManager1')
        self.calls = []
    @dbus.service.method('org.freedesktop.FileManager1', in_signature='ass', out_signature='')
    def ShowItems(self, uris, startup_id):
        self.calls.append(('ShowItems', [str(u) for u in uris]))
    @dbus.service.method('org.freedesktop.FileManager1', in_signature='ass', out_signature='')
    def ShowFolders(self, uris, startup_id):
        self.calls.append(('ShowFolders', [str(u) for u in uris]))
    @dbus.service.method('org.freedesktop.FileManager1', in_signature='ass', out_signature='')
    def ShowItemProperties(self, uris, startup_id):
        self.calls.append(('ShowItemProperties', [str(u) for u in uris]))
    @dbus.service.method('org.fileminnow.TestPortal', in_signature='', out_signature='a(sas)')
    def Calls(self):
        return dbus.Array(self.calls, signature='(sas)')
file_manager_name = dbus.service.BusName('org.freedesktop.FileManager1', bus)
file_manager = FileManager()
portal = Portal()
print('ready', flush=True)
GLib.MainLoop().run()
