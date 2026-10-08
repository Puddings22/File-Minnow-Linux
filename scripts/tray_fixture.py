#!/usr/bin/env python3
"""A StatusNotifierWatcher test host. Use only inside the private UI test bus."""
import os
import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
if os.environ.get('MINNOW_PRIVATE_TEST_SESSION') != '1':
    raise SystemExit('This fixture requires a private test session')
DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName('org.kde.StatusNotifierWatcher', bus)
class Watcher(dbus.service.Object):
    def __init__(self):
        super().__init__(bus, '/StatusNotifierWatcher')
        self.items = []
    @dbus.service.method('org.kde.StatusNotifierWatcher', in_signature='s', out_signature='', sender_keyword='sender')
    def RegisterStatusNotifierItem(self, service, sender=None):
        self.items.append(str(sender) + str(service) if service.startswith('/') else str(service) + '/StatusNotifierItem')
        self.StatusNotifierItemRegistered(self.items[-1])
    @dbus.service.method('org.kde.StatusNotifierWatcher', in_signature='s', out_signature='')
    def RegisterStatusNotifierHost(self, service):
        pass
    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='ss', out_signature='v')
    def Get(self, interface, prop):
        return self.GetAll(interface)[prop]
    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        return {'RegisteredStatusNotifierItems': dbus.Array(self.items, signature='s'), 'IsStatusNotifierHostRegistered': dbus.Boolean(True), 'ProtocolVersion': dbus.Int32(0)}
    @dbus.service.signal('org.kde.StatusNotifierWatcher', signature='s')
    def StatusNotifierItemRegistered(self, service):
        pass
    @dbus.service.signal('org.kde.StatusNotifierWatcher', signature='s')
    def StatusNotifierItemUnregistered(self, service):
        pass
    @dbus.service.signal('org.kde.StatusNotifierWatcher', signature='')
    def StatusNotifierHostRegistered(self):
        pass
watcher = Watcher()
print('ready', flush=True)
GLib.MainLoop().run()
