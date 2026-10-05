#!/usr/bin/env python3
"""Portal and dispatch integration tests on a private D-Bus and capture socket.

Requires PyGObject/dbus-daemon. No real permission, compositor binding, user
configuration, installed binary, display, or clipboard is changed.
"""
import argparse
import json
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from gi.repository import Gio, GLib

PORTAL = 'org.freedesktop.portal.Desktop'
ROOT = '/org/freedesktop/portal/desktop'
IFACE = 'org.freedesktop.portal.GlobalShortcuts'
APP = 'ai.vellum.Shortcuts'
APP_PATH = '/ai/vellum/Shortcuts'
XML = '''<node><interface name="org.freedesktop.portal.GlobalShortcuts">
<property name="version" type="u" access="read"/>
<method name="CreateSession"><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
<method name="ListShortcuts"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
<method name="BindShortcuts"><arg type="o" direction="in"/><arg type="a(sa{sv})" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
<method name="ConfigureShortcuts"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/></method>
<signal name="Activated"><arg type="o"/><arg type="s"/><arg type="t"/><arg type="a{sv}"/></signal>
<signal name="Deactivated"><arg type="o"/><arg type="s"/><arg type="t"/><arg type="a{sv}"/></signal>
<signal name="ShortcutsChanged"><arg type="o"/><arg type="a(sa{sv})"/></signal>
</interface><interface name="org.freedesktop.host.portal.Registry"><method name="Register"><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/></method></interface></node>'''
SESSION_XML = '<node><interface name="org.freedesktop.portal.Session"><method name="Close"/></interface></node>'


def wait_for(predicate, description):
    deadline = time.monotonic() + 6
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.03)
    raise AssertionError(description)


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('binary', nargs='?', default='target/release/vellum-ui')
parser.add_argument('--dispatch', action='store_true', help='also run the real vellumctl against an isolated capture server')
parser.add_argument('--portal-version', type=int, choices=[1, 2], default=2)
options = parser.parse_args()
workspace_binary = Path(options.binary).resolve()
scratch = tempfile.TemporaryDirectory(prefix='vellum-portal-', dir='/tmp')
root = Path(scratch.name)
for directory in ['bin', 'home', 'config/vellum', 'state', 'data', 'runtime', 'control']:
    (root / directory).mkdir(parents=True, exist_ok=True)
(root / 'runtime').chmod(0o700)
# Real UI and fast client, but a harmless sentinel instead of the full CLI.
# Even an unexpected socket failure cannot escape into a graphical capture.
shutil.copy2(workspace_binary, root / 'bin/vellum-ui')
shutil.copy2(workspace_binary.with_name('vellumctl'), root / 'bin/vellumctl')
fallback_marker = root / 'unexpected-fallback'
(root / 'bin/vellum').write_text(chr(10).join(['#!/bin/sh', 'printf fallback > ' + shlex.quote(str(fallback_marker)), 'exit 17', '']))
(root / 'bin/vellum').chmod(0o700)
(root / 'config/vellum/tray.json').write_text(json.dumps({'save': False, 'copy': False}))
capture_requests = []
capture_errors = []
stop_capture = threading.Event()
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(str(root / 'control/control.sock'))
listener.listen(8)
listener.settimeout(.1)


def capture_server():
    long_running = False
    while not stop_capture.is_set():
        try:
            peer, _ = listener.accept()
        except socket.timeout:
            continue
        except OSError:
            break
        try:
            with peer:
                peer.settimeout(2)
                payload = bytearray()
                while not payload.endswith(b'\n') and len(payload) <= 65536:
                    chunk = peer.recv(4096)
                    if not chunk:
                        break
                    payload.extend(chunk)
                request = json.loads(payload)
                if request.get('command') != 'action':
                    raise AssertionError('unexpected control request: ' + repr(request))
                capture_requests.append(request)
                if request['action'] == 'long':
                    long_running = not long_running
                response = {'ok': True, 'running': True, 'accepted': True,
                            'state': 'busy' if long_running else 'idle',
                            'toggled': request['action'] == 'long' and not long_running}
                if request['action'] == 'region':
                    response.update(accepted=False, busy=True, state='busy', message='synthetic busy refusal')
                peer.sendall((json.dumps(response) + '\n').encode())
        except Exception as error:
            capture_errors.append(str(error))


capture_thread = threading.Thread(target=capture_server, daemon=True)
capture_thread.start()
broker = subprocess.Popen(['dbus-daemon', '--session', '--nofork', '--print-address=1'], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
service = None
loop = GLib.MainLoop()
try:
    address = broker.stdout.readline().strip()
    connection = Gio.DBusConnection.new_for_address_sync(address,
        Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION, None, None)

    def call(name, path, interface, method, args=None):
        return connection.call_sync(name, path, interface, method, args, None, Gio.DBusCallFlags.NONE, 3000, None)

    def own(method):
        args = GLib.Variant('(su)', (PORTAL, 4)) if method == 'RequestName' else GLib.Variant('(s)', (PORTAL,))
        return call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', method, args)

    own('RequestName')
    state = {'session': None, 'creates': 0, 'bind_code': 0, 'configured': 0, 'version': options.portal_version, 'defer_bind': False, 'deferred_request': None, 'empty_bind': False, 'defer_configure': False, 'configure_invocation': None}

    def shortcuts(ids=('region', 'long', 'pin-last')):
        return [(identifier, {'trigger_description': GLib.Variant('s', 'Actual-' + identifier)}) for identifier in ids]

    def emit(member, parameters):
        connection.emit_signal(None, ROOT, IFACE, member, parameters)

    def request_response(path, code, details):
        connection.emit_signal(None, path, 'org.freedesktop.portal.Request', 'Response', GLib.Variant('(ua{sv})', (code, details)))
        return False

    def on_method(conn, sender, path, interface, method, parameters, invocation):
        values = parameters.unpack()
        if method == 'Register':
            assert values[0] == 'ai.vellum'
            invocation.return_value(GLib.Variant('()', ()))
            return
        if method == 'Close':
            invocation.return_value(GLib.Variant('()', ()))
            return
        if method == 'ConfigureShortcuts':
            state['configured'] += 1
            if state['defer_configure']:
                state['configure_invocation'] = invocation
            else:
                invocation.return_value(GLib.Variant('()', ()))
            return
        options = values[0] if method == 'CreateSession' else values[-1]
        token = options['handle_token']
        request = ROOT + '/request/' + sender[1:].replace('.', '_') + '/' + token
        invocation.return_value(GLib.Variant('(o)', (request,)))
        if method == 'CreateSession':
            state['creates'] += 1
            state['session'] = ROOT + '/session/' + sender[1:].replace('.', '_') + '/' + options['session_handle_token']
            session_info = Gio.DBusNodeInfo.new_for_xml(SESSION_XML).interfaces[0]
            connection.register_object(state['session'], session_info, on_method, None, None)
            GLib.idle_add(request_response, request, 0, {'session_handle': GLib.Variant('s', state['session'])})
        elif method == 'ListShortcuts':
            # Exercise a v1 backend that cannot list before its initial binding.
            GLib.idle_add(request_response, request, 2, {})
        elif method == 'BindShortcuts':
            assert [entry[0] for entry in values[1]] == ['region', 'long', 'pin-last']
            assert values[1][0][1]['preferred_trigger'] == 'LOGO+Print'
            if state['defer_bind']:
                state['deferred_request'] = request
            else:
                GLib.idle_add(request_response, request, state['bind_code'], {'shortcuts': GLib.Variant('a(sa{sv})', [] if state['empty_bind'] else shortcuts())})

    info = Gio.DBusNodeInfo.new_for_xml(XML)
    for interface in info.interfaces:
        connection.register_object(ROOT, interface, on_method,
            lambda *args: GLib.Variant('u', state['version']), None)
    thread = threading.Thread(target=loop.run, daemon=True)
    thread.start()
    env = {
        'DBUS_SESSION_BUS_ADDRESS': address,
        'HOME': str(root / 'home'), 'XDG_CONFIG_HOME': str(root / 'config'),
        'XDG_STATE_HOME': str(root / 'state'), 'XDG_DATA_HOME': str(root / 'data'),
        'XDG_RUNTIME_DIR': str(root / 'runtime'), 'VELLUM_RUNTIME_DIR': str(root / 'control'),
        'PATH': str(root / 'bin'), 'LANG': 'C.UTF-8',
        'VELLUM_SHORTCUTS_TEST': '0' if options.dispatch else '1',
    }
    service = subprocess.Popen([str(root / 'bin/vellum-ui'), 'shortcuts-service'], env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def has_app():
        return call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', 'NameHasOwner', GLib.Variant('(s)', (APP,))).unpack()[0]

    def control(method):
        return call(APP, APP_PATH, APP, method)

    def status():
        return json.loads(control('GetStatus').unpack()[0])

    wait_for(has_app, 'application service did not register')
    control('Enable')
    wait_for(lambda: status()['phase'] == 'active', 'binding did not complete after the optional List failure')
    assert status()['bindings']['region'] == 'Actual-region'
    session = state['session']

    def key(member, key_id, session_path=None):
        emit(member, GLib.Variant('(osta{sv})', (session_path or session, key_id, 1, {})))

    key('Activated', 'long')
    key('Activated', 'long')
    wait_for(lambda: status()['activations'] == 1, 'held key was not deduplicated')
    key('Deactivated', 'long')
    key('Activated', 'long')
    wait_for(lambda: status()['activations'] == 2, 'second press after release was lost')
    if options.dispatch:
        wait_for(lambda: len(capture_requests) == 2, 'portal activation did not reach the real control client')
        assert [r['action'] for r in capture_requests] == ['long', 'long']
        assert all(r['args'] == ['--no-save', '--no-copy'] for r in capture_requests), capture_requests
        assert not fallback_marker.exists(), 'valid replies must not fall back to a GUI process'
        key('Deactivated', 'long')
        key('Activated', 'region')
        wait_for(lambda: len(capture_requests) == 3, 'region was not dispatched')
        assert capture_requests[-1]['action'] == 'region'
        assert capture_requests[-1]['args'] == ['--no-save', '--no-copy']
        key('Deactivated', 'region')
        key('Activated', 'pin-last')
        wait_for(lambda: len(capture_requests) == 4, 'pin action was not dispatched')
        assert capture_requests[-1]['action'] == 'pin-last'
        assert capture_requests[-1]['args'] == [], 'pin must not inherit capture output flags'
        key('Deactivated', 'pin-last')
    accepted_count = 4 if options.dispatch else 2
    key('Activated', 'unregistered-command')
    key('Activated', 'region', ROOT + '/session/wrong')
    if options.portal_version >= 2:
        control('Configure')
        wait_for(lambda: state['configured'] == 1, 'ConfigureShortcuts was not forwarded')
    else:
        try:
            control('Configure')
        except GLib.Error as error:
            assert '版本1' in str(error)
        else:
            raise AssertionError('version 1 must not claim ConfigureShortcuts support')
        assert state['configured'] == 0
        assert status()['can_configure'] is False
    emit('ShortcutsChanged', GLib.Variant('(oa(sa{sv}))', (session, shortcuts(('region',)))))
    wait_for(lambda: status()['phase'] == 'partial', 'partial grant still reported active')
    assert status()['activations'] == accepted_count
    old_creates = state['creates']
    own('ReleaseName')
    wait_for(lambda: status()['phase'] == 'reconnecting', 'portal disappearance was not reported')
    own('RequestName')
    wait_for(lambda: state['creates'] > old_creates and status()['phase'] == 'active', 'portal reconnect did not restore the session')
    control('Disable')
    wait_for(lambda: status()['phase'] == 'disabled', 'disable did not close the session')
    assert status()['bindings'] == {}
    # A session may be revoked while its authorization response is still queued.
    # A late success must not resurrect that dead session or block a new enable.
    state['defer_bind'] = True
    control('Enable')
    wait_for(lambda: state['deferred_request'] is not None, 'authorization was not pending')
    revoked_session = state['session']
    connection.emit_signal(None, revoked_session, 'org.freedesktop.portal.Session', 'Closed', GLib.Variant('(a{sv})', ({},)))
    wait_for(lambda: status()['phase'] == 'closed', 'session closure was not reported')
    request_response(state['deferred_request'], 0, {'shortcuts': GLib.Variant('a(sa{sv})', shortcuts())})
    # Calls on the same connection form a message-processing barrier after the signal.
    control('GetStatus')
    assert status()['phase'] == 'closed', 'late authorization revived a closed session'
    assert status()['bindings'] == {}
    state['defer_bind'] = False
    control('Enable')
    wait_for(lambda: status()['phase'] == 'active', 'closed authorization left the next enable stuck')
    key('Activated', 'region', revoked_session)
    control('GetStatus')
    assert status()['activations'] == accepted_count
    control('Disable')
    # Empty grants must allow a fresh explicit retry, including on v1.
    state['empty_bind'] = True
    control('Enable')
    wait_for(lambda: status()['phase'] == 'unavailable', 'empty grant appeared active')
    assert status()['bindings'] == {}
    old_creates = state['creates']
    state['empty_bind'] = False
    control('Enable')
    wait_for(lambda: state['creates'] > old_creates and status()['phase'] == 'active', 'empty grant retry did nothing')
    if options.portal_version >= 2:
        state['defer_configure'] = True
        control('Configure')
        wait_for(lambda: state['configure_invocation'] is not None, 'configure not pending')
        control('Disable')
        disabled_message = status()['message']
        state['configure_invocation'].return_dbus_error('org.freedesktop.portal.Error.Failed', 'late configure error')
        control('GetStatus')
        assert status()['phase'] == 'disabled'
        assert status()['message'] == disabled_message, 'stale configure error replaced disabled status'
    else:
        control('Disable')
    state['defer_bind'] = True
    state['deferred_request'] = None
    control('Enable')
    wait_for(lambda: state['deferred_request'] is not None, 'bind was not deferred')
    control('Disable')
    request_response(state['deferred_request'], 0, {'shortcuts': GLib.Variant('a(sa{sv})', shortcuts())})
    control('GetStatus')
    assert status()['phase'] == 'disabled'
    assert status()['bindings'] == {}
    state['defer_bind'] = False
    state['bind_code'] = 2
    control('Enable')
    wait_for(lambda: status()['phase'] == 'unavailable', 'backend failure not reported')
    assert '兼容性' in status()['message']
    assert status()['bindings'] == {}
    state['bind_code'] = 1
    control('Enable')
    wait_for(lambda: status()['phase'] == 'denied', 'cancelled permission was not reported')
    assert status()['enabled'] is False
    assert not capture_errors, capture_errors
    assert not fallback_marker.exists(), 'no test is allowed to launch a graphical fallback'
    if options.dispatch:
        preference = root / 'config/vellum/shortcuts.json'
        assert json.loads(preference.read_text())['enabled'] is False
        assert preference.stat().st_mode & 0o777 == 0o600
        assert len(capture_requests) == 4, 'unknown/session-mismatched keys must not launch actions'
        print('dispatch integration passed: portal -> real vellumctl -> private control socket; long toggle pair, busy region, pin, output flags, no GUI fallback')
    else:
        assert capture_requests == [], 'dry-run mode must never dispatch'
    print('portal integration passed: real Gio client, private bus, bind/actual keys, repeat guard, partial grant, reconnect, cancellation')
finally:
    if service is not None:
        service.terminate()
        try:
            output, errors = service.communicate(timeout=4)
        except subprocess.TimeoutExpired:
            service.kill()
            output, errors = service.communicate()
        if service.returncode not in (0, -15):
            print(errors, file=sys.stderr)
    loop.quit()
    broker.terminate()
    try:
        broker.communicate(timeout=3)
    except subprocess.TimeoutExpired:
        broker.kill()
        broker.communicate()
    stop_capture.set()
    listener.close()
    capture_thread.join(timeout=2)
    # This is exclusively our freshly-created test tree, after its children stop.
    scratch.cleanup()
