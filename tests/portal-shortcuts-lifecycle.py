#!/usr/bin/env python3
"""Shortcut preference and upgrade regressions; private bus/config, no desktop.

Uses an existing vellum-ui binary, never builds or installs anything. The new
release is a sentinel that execs an isolated copy of that binary: this tests the
handoff mechanism, not compatibility of two real release builds.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from xml.sax.saxutils import escape

from gi.repository import Gio, GLib

APP = 'ai.vellum.Shortcuts'
PATH = '/ai/vellum/Shortcuts'
DBUS = 'org.freedesktop.DBus'


class Fixture:
    def __init__(self, binary):
        self.scratch = tempfile.TemporaryDirectory(prefix='vellum-shortcuts-lifecycle-')
        self.root = Path(self.scratch.name)
        self.children = []
        self.activated_pid = None
        for directory in ['bin', 'home', 'config/vellum', 'state', 'data', 'runtime', 'services']:
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        (self.root / 'runtime').chmod(0o700)
        self.binary = self.root / 'bin/vellum-ui'
        shutil.copy2(binary, self.binary)
        self.env = {
            'HOME': str(self.root / 'home'), 'XDG_CONFIG_HOME': str(self.root / 'config'),
            'XDG_STATE_HOME': str(self.root / 'state'), 'XDG_DATA_HOME': str(self.root / 'data'),
            'XDG_RUNTIME_DIR': str(self.root / 'runtime'), 'PATH': str(self.root / 'bin'),
            'VELLUM_RUNTIME_DIR': str(self.root / 'runtime'),
            'VELLUM_SHORTCUTS_TEST': '0', 'LANG': 'C.UTF-8',
        }
        self.preference = self.root / 'config/vellum/shortcuts.json'
        self.autostart = self.root / 'config/vellum/autostart.json'
        self.marker = self.root / 'handoff.jsonl'
        config = self.root / 'bus.conf'
        config.write_text('''<busconfig><type>session</type>
<listen>unix:tmpdir={runtime}</listen><auth>EXTERNAL</auth>
<servicedir>{services}</servicedir><policy context="default">
<allow user="*"/><allow own="*"/><allow send_destination="*"/>
<allow receive_sender="*"/></policy></busconfig>'''.format(
            runtime=escape(str(self.root / 'runtime')), services=escape(str(self.root / 'services'))))
        self.broker_log = (self.root / 'broker.log').open('w+')
        self.broker = subprocess.Popen([shutil.which('dbus-daemon'), '--config-file=' + str(config),
                                        '--nofork', '--print-address=1'], env=self.env,
                                       stdout=subprocess.PIPE, stderr=self.broker_log, text=True)
        address = self.broker.stdout.readline().strip()
        assert address, 'private D-Bus did not start'
        self.env['DBUS_SESSION_BUS_ADDRESS'] = address
        self.connection = Gio.DBusConnection.new_for_address_sync(address,
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None, None)

    def call(self, method, args):
        return self.connection.call_sync(DBUS, '/org/freedesktop/DBus', DBUS, method,
                                         args, None, Gio.DBusCallFlags.NO_AUTO_START, 8000, None)

    def has_owner(self):
        return self.call('NameHasOwner', GLib.Variant('(s)', (APP,))).unpack()[0]

    def status(self):
        try:
            reply = self.connection.call_sync(APP, PATH, APP, 'GetStatus', None, None,
                Gio.DBusCallFlags.NO_AUTO_START, 1000, None)
            return json.loads(reply.unpack()[0])
        except GLib.Error:
            return {}

    def wait(self, predicate, description, timeout=6):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(.04)
        raise AssertionError(description)

    def cli(self, action):
        return subprocess.run([str(self.binary), 'shortcuts-control', action], env=self.env,
                              capture_output=True, text=True, timeout=10)

    def start(self, binary=None):
        log = (self.root / ('service-' + str(len(self.children)) + '.log')).open('w+')
        child = subprocess.Popen([str(binary or self.binary), 'shortcuts-service'], env=self.env,
                                 stdout=log, stderr=log)
        self.children.append((child, log))
        return child

    def managed(self, enabled=True, startup=True):
        build = json.loads(subprocess.check_output([str(self.binary), '--build-info-json'],
                                                  env=self.env, text=True, timeout=5))
        self.install = self.root / 'install'
        self.release = self.install / 'releases' / build['build_id']
        (self.release / 'bin').mkdir(parents=True)
        (self.release / 'generated').mkdir()
        self.old = self.release / 'bin/vellum-ui'
        shutil.copy2(self.binary, self.old)
        (self.release / 'manifest.json').write_text(json.dumps({
            'format': 'vellum-release-v1', 'release_id': build['build_id'],
            'version': build['version'], 'build': build, 'files': []}))
        (self.release / 'installed.json').write_text(json.dumps({
            'format': 'vellum-installed-v1', 'build_id': build['build_id'],
            'release_id': build['build_id'], 'generated': []}))
        (self.install / 'state.json').write_text(json.dumps({
            'format': 'vellum-release-state-v1', 'paths': {
                'root': str(self.install), 'config_dir': str(self.root / 'config')}}))
        self.preference.write_text(json.dumps({'enabled': enabled}))
        self.autostart.write_text(json.dumps({'enabled': startup}))
        self.next = self.install / 'releases/synthetic-next'
        (self.next / 'bin').mkdir(parents=True)
        target = self.next / 'bin/vellum-ui'
        target.write_text('#!' + sys.executable + '\n' + '''import json, os, sys
from gi.repository import Gio, GLib
connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
owned = connection.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus',
    'org.freedesktop.DBus', 'NameHasOwner', GLib.Variant('(s)', ('ai.vellum.Shortcuts',)),
    None, Gio.DBusCallFlags.NO_AUTO_START, 2000, None).unpack()[0]
with open({marker!r}, 'a') as marker:
    marker.write(json.dumps({{'args': sys.argv[1:], 'prior_owner': owned}}) + '\\n')
os.execve({binary!r}, [{binary!r}, 'shortcuts-service'], dict(os.environ))
'''.format(marker=str(self.marker), binary=str(self.binary)))
        target.chmod(0o700)
        self.switch(self.release)

    def switch(self, release):
        temporary = self.install / 'next-current'
        temporary.symlink_to(release)
        temporary.replace(self.install / 'current')

    def assert_handoff(self, child=None):
        self.wait(lambda: self.marker.exists() or (child is not None and child.poll() is not None),
                  'superseded service neither handed off nor exited', timeout=22)
        assert self.marker.exists(), 'superseded service exited without starting current'
        self.wait(lambda: self.status().get('enabled') is True, 'current service did not take ownership')
        records = [json.loads(line) for line in self.marker.read_text().splitlines()]
        assert records == [{'args': ['shortcuts-service'], 'prior_owner': False}], records
        owner = self.call('GetNameOwner', GLib.Variant('(s)', (APP,))).unpack()[0]
        pid = self.call('GetConnectionUnixProcessID', GLib.Variant('(s)', (owner,))).unpack()[0]
        if child is None:
            self.activated_pid = pid
        else:
            assert child.poll() is None and pid == child.pid, 'handoff detached from its activation parent'
        assert self.preference.read_text() == json.dumps({'enabled': True})
        assert self.autostart.read_text() == json.dumps({'enabled': True})

    def close(self):
        if self.activated_pid is not None:
            try:
                os.kill(self.activated_pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            self.wait(lambda: not self.has_owner(), 'activated child did not release its bus name')
        for child, log in self.children:
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=4)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=3)
            log.close()
        self.connection.close_sync(None)
        self.broker.terminate()
        try:
            self.broker.communicate(timeout=4)
        except subprocess.TimeoutExpired:
            self.broker.kill()
            self.broker.communicate(timeout=3)
        self.broker_log.close()
        self.scratch.cleanup()


def disable(f):
    # Read-only status neither starts a service nor manufactures a preference.
    status = f.cli('status')
    assert status.returncode == 0, status.stderr
    assert not f.preference.exists() and not f.has_owner()
    f.preference.write_text('{"enabled":true}')
    f.autostart.write_text('{"enabled":true}')
    result = f.cli('disable')
    assert result.returncode == 0, result.stderr
    assert json.loads(f.preference.read_text())['enabled'] is False, 'ownerless Disable claimed success without persisting false'
    reply = json.loads(result.stdout)
    assert reply['enabled'] is False and reply['requested'] is True, reply
    assert f.preference.stat().st_mode & 0o777 == 0o600
    assert not f.has_owner(), 'Disable activated a service'
    assert f.autostart.read_text() == '{"enabled":true}', 'Disable changed login startup'
    assert json.loads(f.cli('status').stdout)['enabled'] is False
    assert f.cli('disable').returncode == 0, 'Disable was not idempotent'
    child = f.start()
    f.wait(lambda: f.status().get('phase') == 'disabled', 'restart did not keep shortcuts disabled')
    assert f.status()['enabled'] is False
    child.terminate()
    child.wait(timeout=4)
    f.wait(lambda: not f.has_owner(), 'test service did not exit')
    # A directory at the destination fails even for root in the CI container.
    f.preference.unlink()
    f.preference.mkdir()
    result = f.cli('disable')
    assert result.returncode != 0, 'failed persistence was reported as successful Disable'
    assert '无法保存快捷键偏好' in result.stderr, result.stderr
    assert f.preference.is_dir() and not f.has_owner()
    assert f.cli('configure').returncode != 0, 'ownerless Configure claimed success'


def handoff_startup(f):
    f.managed()
    f.switch(f.next)
    child = f.start(f.old)
    f.assert_handoff(child)


def handoff_live(f):
    f.managed()
    child = f.start(f.old)
    f.wait(lambda: f.status().get('enabled') is True, 'old service did not start')
    old_owner = f.call('GetNameOwner', GLib.Variant('(s)', (APP,))).unpack()[0]
    f.switch(f.next)
    f.assert_handoff(child)
    assert f.call('GetNameOwner', GLib.Variant('(s)', (APP,))).unpack()[0] != old_owner


def handoff_dbus(f):
    f.managed()
    f.switch(f.next)
    (f.root / 'services/ai.vellum.Shortcuts.service').write_text(
        '[D-BUS Service]\nName=' + APP + '\nExec=' + str(f.old) + ' shortcuts-service\n')
    try:
        f.call('StartServiceByName', GLib.Variant('(su)', (APP, 0)))
    except GLib.Error as error:
        raise AssertionError('private D-Bus activation did not hand the superseded service over to current') from error
    finally:
        # Track an activated child even if a later assertion fails.
        if f.has_owner():
            owner = f.call('GetNameOwner', GLib.Variant('(s)', (APP,))).unpack()[0]
            f.activated_pid = f.call('GetConnectionUnixProcessID', GLib.Variant('(s)', (owner,))).unpack()[0]
    f.assert_handoff()


def no_handoff(f, *, enabled=True, startup=True, live=False, broken=None):
    f.managed(enabled=enabled, startup=startup)
    if broken == 'invalid-preference':
        f.preference.write_text('{')
    if broken == 'invalid-startup':
        f.autostart.write_text('{')
    before = (f.preference.read_bytes(), f.autostart.read_bytes())
    if live:
        child = f.start(f.old)
        f.wait(lambda: bool(f.status()), 'old service did not start')
    if broken == 'missing-target':
        (f.next / 'bin/vellum-ui').unlink()
    if broken == 'escaping-target':
        f.next = f.root / 'outside-install'
        (f.next / 'bin').mkdir(parents=True)
        shutil.copy2(f.binary, f.next / 'bin/vellum-ui')
    f.switch(f.next)
    if broken == 'loop':
        f.env['VELLUM_SHORTCUTS_HANDOFF'] = '1'
    if not live:
        child = f.start(f.old)
    try:
        code = child.wait(timeout=22 if live else 6)
    except subprocess.TimeoutExpired as error:
        raise AssertionError('superseded service failed to exit or entered a handoff loop') from error
    if broken in ['missing-target', 'escaping-target', 'loop', 'invalid-startup']:
        assert code != 0, 'invalid handoff was silently reported as success'
    else:
        assert code == 0, code
    assert not f.marker.exists(), 'disabled/invalid service started current'
    assert not f.has_owner(), 'old service retained ownership after supersession'
    assert (f.preference.read_bytes(), f.autostart.read_bytes()) == before


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', nargs='?', default='target/release/vellum-ui')
    parser.add_argument('--case', choices=['all', 'disable', 'handoff-startup', 'handoff-live', 'handoff-dbus', 'boundaries'], default='all')
    options = parser.parse_args()
    binary = Path(options.binary).resolve(strict=True)
    cases = {'disable': disable, 'handoff-startup': handoff_startup,
             'handoff-live': handoff_live, 'handoff-dbus': handoff_dbus}
    if options.case in ['all', 'boundaries']:
        for label, kwargs in [
            ('disabled-startup', {'enabled': False}),
            ('login-opt-out-startup', {'startup': False}),
            ('disabled-live', {'enabled': False, 'live': True}),
            ('login-opt-out-live', {'startup': False, 'live': True}),
            *[(name, {'broken': name}) for name in ['invalid-preference', 'invalid-startup',
                                                    'missing-target', 'escaping-target', 'loop']],
        ]:
            cases[label] = lambda fixture, kwargs=kwargs: no_handoff(fixture, **kwargs)
    for name, test in cases.items():
        if options.case not in ['all', 'boundaries', name]:
            continue
        if options.case == 'boundaries' and name in ['disable', 'handoff-startup', 'handoff-live', 'handoff-dbus']:
            continue
        fixture = Fixture(binary)
        try:
            test(fixture)
            print('shortcut lifecycle passed:', name, flush=True)
        finally:
            fixture.close()


if __name__ == '__main__':
    main()
