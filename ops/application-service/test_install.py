#!/usr/bin/env python3
"""Run with python3 -m unittest discover -s ops/application-service."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('installer', Path(__file__).with_name('install.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class Lifecycle(unittest.TestCase):
    def test_verified_artifact_activation_replacement_and_withdrawal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'source'
            source.write_bytes(b'first executable')
            data = dict(name='example', binary=str(source),
                        sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
                        workspace=str(root), node_user='node', account=9, label='app',
                        port=29134, config={'policy_module': 'custom-policy'},
                        memory_max=67108864, cpu_quota=75, tasks_max=32, readonly_paths=[], devices=[])
            commands = []
            user = types.SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())
            state, artifacts, units = root / 'state', root / 'artifacts', root / 'units'
            units.mkdir()
            with patch.multiple(installer, STATE=state, ARTIFACTS=artifacts, UNITS=units), \
                    patch.object(installer, 'command', side_effect=lambda *args: commands.append(args)), \
                    patch.object(installer.os, 'fchown'), \
                    patch.object(installer.pwd, 'getpwnam', return_value=user):
                _, service = installer.render_units(dict(data, devices=['/dev/null']), source, root)
                self.assertIn('StateDirectory=ducktape-application-example', service)
                self.assertIn('DevicePolicy=closed', service)
                self.assertIn('BindPaths=/dev/null\nDeviceAllow=/dev/null rw', service)
                mount = root / 'tenant-store'
                mount.mkdir(mode=0o755)
                manifest = root / 'input.json'
                bound = dict(data, readonly_paths=[{'source': str(mount), 'destination': '/var/lib/application-storage/git'}])
                manifest.write_text(json.dumps(bound))
                self.assertEqual(installer.checked_manifest(manifest), bound)
                _, service = installer.render_units(bound, source, root)
                self.assertIn(f'BindReadOnlyPaths={mount}:/var/lib/application-storage/git', service)
                device_manifest = root / 'devices.json'
                for devices in [['/dev/null'], ['/dev/this-device-does-not-exist'], ['/dev'], ['/dev/null', '/dev/null']]:
                    device_manifest.write_text(json.dumps(dict(data, devices=devices)))
                    if devices == ['/dev/null']:
                        installer.checked_manifest(device_manifest)
                    else:
                        with self.assertRaises((ValueError, FileNotFoundError)):
                            installer.checked_manifest(device_manifest)
                alias = root / 'store-alias'
                alias.symlink_to(mount)
                for invalid in [
                    [{'source': str(alias), 'destination': '/var/lib/application-storage/git'}],
                    [{'source': str(mount), 'destination': '/etc'}],
                    bound['readonly_paths'] * 2,
                ]:
                    manifest.write_text(json.dumps(dict(data, readonly_paths=invalid)))
                    with self.assertRaises(ValueError):
                        installer.checked_manifest(manifest)
                app = installer.Installation('example', '/usr/local/bin/ducktape')
                wrong = dict(data, sha256='0' * 64)
                with self.assertRaisesRegex(ValueError, 'SHA-256 mismatch'):
                    app.install(wrong)
                self.assertEqual(commands, [])
                self.assertFalse((app.state / 'manifest.json').exists())
                app.install(data)
                saved = json.loads((app.state / 'manifest.json').read_bytes())
                with patch.object(app, 'installed', side_effect=lambda: json.loads((app.state / 'manifest.json').read_bytes())):
                    app.activate()
                    token = (app.state / 'upstream-token').read_bytes()
                    self.assertRegex(token.decode(), r'^[0-9a-f]{64}$')
                    self.assertEqual((app.state / 'upstream-token').stat().st_mode & 0o777, 0o600)
                    bind = next(i for i, c in enumerate(commands) if 'bind' in c)
                    start = next(i for i, c in enumerate(commands) if c[:2] == ('systemctl', 'start'))
                    self.assertLess(start, bind)
                    self.assertIn('--credential-file', commands[bind])
                    commands.clear()
                    app.restart()
                    self.assertIn('unbind', commands[1])
                    self.assertEqual(commands[2][:2], ('systemctl', 'restart'))
                    self.assertIn('bind', commands[3])
                    self.assertEqual(token, (app.state / 'upstream-token').read_bytes())
                    commands.clear()
                    app.stop()
                    self.assertIn('unbind', commands[0])
                    self.assertIn('--account', commands[0])
                    self.assertFalse((app.state / 'upstream-token').exists())
                    app.activate()
                    self.assertNotEqual(token, (app.state / 'upstream-token').read_bytes())
                    source.write_bytes(b'replacement executable')
                    replacement = dict(data, sha256=hashlib.sha256(source.read_bytes()).hexdigest())
                    app.install(replacement)
                    self.assertFalse((app.state / 'upstream-token').exists())
                    self.assertEqual(Path(saved['binary']).read_bytes(), b'first executable')
                    app.activate()
                    replacement_path = json.loads((app.state / 'manifest.json').read_bytes())['binary']
                    self.assertEqual(Path(replacement_path).read_bytes(), b'replacement executable')
                    # Failed process activation never leaves route admission or a token.
                    def failed_start(*args):
                        commands.append(args)
                        if args[:2] == ('systemctl', 'start'):
                            raise subprocess.CalledProcessError(1, args)
                    commands.clear()
                    with patch.object(installer, 'command', side_effect=failed_start):
                        with self.assertRaises(subprocess.CalledProcessError):
                            app.activate()
                    self.assertFalse(any('bind' in command for command in commands))
                    self.assertFalse((app.state / 'upstream-token').exists())
                    app.activate()
                    def failed_restart(*args):
                        commands.append(args)
                        if args[:2] == ('systemctl', 'restart'):
                            raise subprocess.CalledProcessError(1, args)
                    commands.clear()
                    with patch.object(installer, 'command', side_effect=failed_restart):
                        with self.assertRaises(subprocess.CalledProcessError):
                            app.restart()
                    self.assertIn('unbind', commands[1])
                    self.assertFalse(any('bind' in command for command in commands))
                    self.assertFalse((app.state / 'upstream-token').exists())
                    other = installer.Installation('other', '/usr/local/bin/ducktape')
                    with self.assertRaisesRegex(ValueError, 'owns this route or port'):
                        other.install(dict(replacement, name='other'))
                    # Tampering refuses before a systemd or route mutation.
                    Path(replacement_path).chmod(0o755)
                    Path(replacement_path).write_bytes(b'altered')
                    commands.clear()
                    with self.assertRaisesRegex(ValueError, 'SHA-256 mismatch'):
                        app.activate()
                    self.assertEqual(commands, [])
                socket_unit, service_unit = installer.render_units(data, '/bin/true', app.state)
                self.assertIn('ListenStream=127.0.0.1:29134', socket_unit)
                for setting in ['Type=notify', 'NotifyAccess=main', 'TimeoutStartSec=30', 'DynamicUser=yes', 'LoadCredential=upstream-token:',
                                'MemoryMax=67108864', 'CPUQuota=75%', 'TasksMax=32']:
                    self.assertIn(setting, service_unit)

    def test_inherited_listener_survives_process_replacement_and_rejects_spoofed_identity(self):
        # The parent stands in for systemd: it owns the listener throughout both
        # child lifetimes. Connect waits on the real accept event, never a sleep.
        with tempfile.TemporaryDirectory() as directory, socket.socket() as listener:
            root = Path(directory)
            (root / 'upstream-token').write_text('a' * 64)
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            address = listener.getsockname()
            fixture = Path(__file__).with_name('test_service.py')
            bootstrap = ('import os,runpy,sys; os.dup2(int(sys.argv[1]),3); '
                         'os.environ["LISTEN_PID"]=str(os.getpid()); '
                         'runpy.run_path(sys.argv[2],run_name="__main__")')
            for token, caller, expected in [('wrong', '9', 403), ('a' * 64, '', 401), ('a' * 64, '9', 200)]:
                environment = dict(os.environ, LISTEN_FDS='1', CREDENTIALS_DIRECTORY=str(root))
                child = subprocess.Popen([sys.executable, '-c', bootstrap, str(listener.fileno()), str(fixture)],
                                         pass_fds=(listener.fileno(),), env=environment)
                try:
                    with socket.create_connection(address, timeout=5) as client:
                        request = (f'GET / HTTP/1.1\r\nHost: localhost\r\n'
                                   f'x-duck-upstream-token: {token}\r\n'
                                   f'x-duck-caller-account: {caller}\r\nConnection: close\r\n\r\n')
                        client.sendall(request.encode())
                        self.assertIn(f' {expected} '.encode(), client.recv(4096).split(b'\r\n')[0])
                    self.assertEqual(child.wait(timeout=5), 0)
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait()
                with socket.socket() as attacker:
                    with self.assertRaises(OSError):
                        attacker.bind(address)


if __name__ == '__main__':
    unittest.main()
