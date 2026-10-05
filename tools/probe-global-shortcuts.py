#!/usr/bin/env python3
"""Probe the real portal session without binding or triggering any shortcut.

Requires the system's PyGObject; no dependencies are installed by this script.
It opens and closes a temporary session and writes no compositor configuration.
"""
import json
import sys
import uuid
from gi.repository import Gio, GLib

BUS = 'org.freedesktop.portal.Desktop'
PATH = '/org/freedesktop/portal/desktop'
IFACE = 'org.freedesktop.portal.GlobalShortcuts'
connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
loop = GLib.MainLoop()
result = {'ok': False, 'stage': 'create_session'}
token = 'vellum_probe_' + uuid.uuid4().hex
sender = connection.get_unique_name()[1:].replace('.', '_')
request = '/org/freedesktop/portal/desktop/request/' + sender + '/' + token

# Native, non-sandboxed applications can declare their installed desktop ID.
# This is registration with the portal, not a shortcut grant or a bypass.
try:
    connection.call_sync(BUS, PATH, 'org.freedesktop.host.portal.Registry',
        'Register', GLib.Variant('(sa{sv})', ('ai.vellum', {})), None,
        Gio.DBusCallFlags.NONE, 3000, None)
    result['app_registration'] = 'supported'
except GLib.Error as error:
    result['app_registration'] = str(error)

def response(conn, sender_name, path, interface, member, parameters, data):
    if path != request:
        return
    code, details = parameters.unpack()
    result['response'] = code
    if code == 0 and details.get('session_handle'):
        session = details['session_handle']
        result['ok'] = True
        try:
            connection.call_sync(BUS, session, 'org.freedesktop.portal.Session',
                'Close', None, None, Gio.DBusCallFlags.NONE, 3000, None)
            result['closed'] = True
        except GLib.Error as error:
            result['close_error'] = str(error)
    loop.quit()

subscription = connection.signal_subscribe(BUS, 'org.freedesktop.portal.Request',
    'Response', None, None, Gio.DBusSignalFlags.NONE, response, None)
try:
    connection.call_sync(BUS, PATH, IFACE, 'CreateSession',
        GLib.Variant('(a{sv})', ({'handle_token': GLib.Variant('s', token),
          'session_handle_token': GLib.Variant('s', token)},)), None,
        Gio.DBusCallFlags.NONE, 5000, None)
    def timeout():
        result['timeout'] = True
        loop.quit()
        return False
    GLib.timeout_add_seconds(10, timeout)
    loop.run()
except GLib.Error as error:
    result['error'] = str(error)
connection.signal_unsubscribe(subscription)
print(json.dumps(result, ensure_ascii=False, indent=2))
sys.exit(0 if result['ok'] else 1)
