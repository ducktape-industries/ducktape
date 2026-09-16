#!/usr/bin/env python3
"""Install isolated Gateway applications using systemd socket activation."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import stat
import shutil
import subprocess
import tempfile

STATE = Path('/var/lib/ducktape-applications')
ARTIFACTS = Path('/usr/local/lib/ducktape-applications')
UNITS = Path('/etc/systemd/system')
NAME = re.compile(r'[a-z][a-z0-9-]{0,47}\Z')
SHA256 = re.compile(r'[0-9a-f]{64}\Z')


def command(*args):
    subprocess.run(args, check=True)


def checked_manifest(path):
    data = json.loads(path.read_bytes())
    required = {'name', 'binary', 'sha256', 'workspace', 'node_user', 'account',
                'label', 'port', 'config', 'memory_max', 'cpu_quota', 'tasks_max',
                'readonly_paths', 'devices'}
    if set(data) != required:
        raise ValueError('manifest fields must match the documented schema')
    if not isinstance(data['name'], str) or not NAME.fullmatch(data['name']):
        raise ValueError('invalid application name')
    if not isinstance(data['sha256'], str) or not SHA256.fullmatch(data['sha256']):
        raise ValueError('invalid artifact SHA-256')
    if not isinstance(data['label'], str) or not NAME.fullmatch(data['label']):
        raise ValueError('invalid route label')
    for key, low, high in [('account', 1, 2**64 - 1), ('port', 1024, 65535),
                           ('memory_max', 16 * 1024**2, 2**50),
                           ('cpu_quota', 1, 10000), ('tasks_max', 1, 65535)]:
        if type(data[key]) is not int or not low <= data[key] <= high:
            raise ValueError(f'invalid {key}')
    for key in ['binary', 'workspace']:
        if not isinstance(data[key], str) or not Path(data[key]).is_absolute():
            raise ValueError(f'{key} must be an absolute path')
    if not isinstance(data['config'], dict):
        raise ValueError('config must be an application-owned JSON object')
    if len(json.dumps(data['config']).encode()) > 1024 * 1024:
        raise ValueError('application config exceeds one MiB')
    mounts = data['readonly_paths']
    if not isinstance(mounts, list) or len(mounts) > 16:
        raise ValueError('readonly_paths must contain at most 16 mounts')
    destinations = set()
    for mount in mounts:
        if not isinstance(mount, dict) or set(mount) != {'source', 'destination'}:
            raise ValueError('each readonly path needs source and destination')
        source, destination = mount['source'], mount['destination']
        if not isinstance(source, str) or not re.fullmatch(r'/[A-Za-z0-9_./-]+', source):
            raise ValueError('readonly source must be an absolute safe path')
        path = Path(source)
        if not path.is_dir() or str(path.resolve(strict=True)) != source:
            raise ValueError('readonly source must name an exact existing directory without symlinks')
        if path.stat().st_mode & 0o005 != 0o005:
            raise ValueError('readonly source must permit isolated process read and traversal')
        if not isinstance(destination, str) or not re.fullmatch(r'/var/lib/application-storage/[a-z][a-z0-9-]{0,47}', destination):
            raise ValueError('readonly destination must be a named application-storage directory')
        if destination in destinations:
            raise ValueError('duplicate readonly destination')
        destinations.add(destination)
    devices = data['devices']
    if not isinstance(devices, list) or len(devices) > 8 or not all(isinstance(device, str) for device in devices):
        raise ValueError('devices must name at most 8 distinct character devices')
    if len(set(devices)) != len(devices):
        raise ValueError('devices must name at most 8 distinct character devices')
    for device in devices:
        if not isinstance(device, str) or not re.fullmatch(r'/dev/[A-Za-z0-9_/-]+', device):
            raise ValueError('device must be an exact path under /dev')
        path = Path(device)
        if str(path.resolve(strict=True)) != device or not stat.S_ISCHR(path.stat().st_mode):
            raise ValueError('device must name an existing character device without symlinks')
        metadata = path.stat()
        public_rw = metadata.st_mode & 0o006 == 0o006
        group_rw = metadata.st_gid != 0 and metadata.st_mode & 0o060 == 0o060
        if not public_rw and not group_rw:
            raise ValueError('device must allow public rw or non-root group rw')
    user = pwd.getpwnam(data['node_user'])
    if user.pw_uid == 0:
        raise ValueError('node_user must be an unprivileged node owner')
    workspace = Path(data['workspace'])
    if not workspace.is_dir() or workspace.stat().st_uid != user.pw_uid:
        raise ValueError('workspace must be owned by node_user')
    return data


def atomic_write(path, content, mode=0o600, uid=0, gid=0):
    fd, temporary = tempfile.mkstemp(prefix='.', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as out:
            out.write(content)
            out.flush()
            os.fsync(out.fileno())
            os.fchmod(out.fileno(), mode)
            os.fchown(out.fileno(), uid, gid)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def unit_name(name):
    return f'ducktape-application-{name}'


def render_units(data, artifact, state):
    # Every interpolated unit value is an integer or derived from validated names.
    name = unit_name(data['name'])
    socket = f'''[Unit]
Description=Ducktape application socket {data['name']}

[Socket]
ListenStream=127.0.0.1:{data['port']}
Accept=no
Service={name}.service

[Install]
WantedBy=sockets.target
'''
    service = f'''[Unit]
Description=Ducktape application {data['name']}
Requires={name}.socket
After={name}.socket

[Service]
Type=notify
NotifyAccess=main
TimeoutStartSec=30
ExecStart={artifact}
Sockets={name}.socket
DynamicUser=yes
StateDirectory={name}
StateDirectoryMode=0700
DevicePolicy=closed
LoadCredential=upstream-token:{state}/upstream-token
LoadCredential=application.json:{state}/application.json
Restart=on-failure
RestartSec=1
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
CapabilityBoundingSet=
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
UMask=0077
MemoryMax={data['memory_max']}
CPUQuota={data['cpu_quota']}%
TasksMax={data['tasks_max']}
'''
    for mount in data['readonly_paths']:
        service += f"BindReadOnlyPaths={mount['source']}:{mount['destination']}\n"
    groups = set()
    for device in data['devices']:
        metadata = Path(device).stat()
        if metadata.st_mode & 0o006 != 0o006:
            groups.add(metadata.st_gid)
        service += f"BindPaths={device}\nDeviceAllow={device} rw\n"
    if groups:
        service += 'SupplementaryGroups=' + ' '.join(str(group) for group in sorted(groups)) + '\n'
    return socket, service


class Installation:
    def __init__(self, name, ducktape):
        if not NAME.fullmatch(name):
            raise ValueError('invalid application name')
        self.name = name
        self.state = STATE / name
        if not Path(ducktape).is_absolute():
            raise ValueError('ducktape must be an absolute executable path')
        self.ducktape = ducktape
        self.unit = unit_name(name)

    def installed(self):
        return checked_manifest(self.state / 'manifest.json')

    def route(self, data, verb, *extra):
        command('runuser', '-u', data['node_user'], '--', self.ducktape,
                'gateway', verb, '--workspace', data['workspace'],
                '--account', str(data['account']), '--label', data['label'], *extra)

    def stop(self):
        data = self.installed()
        # Withdraw admission before releasing the listener; retire the credential
        # even when shutdown fails, so a later activation must use a fresh token.
        self.route(data, 'unbind')
        try:
            command('systemctl', 'disable', '--now', self.unit + '.socket')
            command('systemctl', 'stop', self.unit + '.service')
        finally:
            (self.state / 'upstream-token').unlink(missing_ok=True)

    def ensure_exclusive_binding(self, data):
        for manifest in STATE.glob('*/manifest.json'):
            if manifest.parent == self.state:
                continue
            other = json.loads(manifest.read_bytes())
            same_route = (other['workspace'], other['account'], other['label']) == (
                data['workspace'], data['account'], data['label'])
            same_port = other['port'] == data['port']
            if same_route or same_port:
                raise ValueError('another installed application owns this route or port')

    def verify_artifact(self, data):
        with open(data['binary'], 'rb') as artifact:
            if hashlib.file_digest(artifact, 'sha256').hexdigest() != data['sha256']:
                raise ValueError('installed artifact SHA-256 mismatch')

    def install(self, data):
        self.ensure_exclusive_binding(data)
        # Copy first, then verify the exact immutable bytes that will execute.
        self.state.mkdir(parents=True, exist_ok=True, mode=0o755)
        fd, temporary = tempfile.mkstemp(prefix='.artifact-', dir=self.state)
        digest = hashlib.sha256()
        try:
            with os.fdopen(fd, 'wb') as out, open(data['binary'], 'rb') as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b''):
                    digest.update(chunk)
                    out.write(chunk)
                out.flush()
                os.fsync(out.fileno())
            if digest.hexdigest() != data['sha256']:
                raise ValueError('artifact SHA-256 mismatch')
            if (self.state / 'manifest.json').exists():
                self.stop()
            artifact = ARTIFACTS / self.name / data['sha256'] / 'service'
            artifact.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
            os.chmod(temporary, 0o555)
            # Both locations are disk-backed; copy across filesystem boundaries.
            shutil.copyfile(temporary, artifact)
            artifact.chmod(0o555)
            with open(artifact, 'rb') as installed:
                os.fsync(installed.fileno())
                if hashlib.file_digest(installed, 'sha256').hexdigest() != data['sha256']:
                    raise ValueError('installed artifact SHA-256 mismatch')
            stored = dict(data, binary=str(artifact))
            atomic_write(self.state / 'manifest.json', json.dumps(stored).encode())
            atomic_write(self.state / 'application.json', json.dumps(data['config']).encode())
            socket, service = render_units(stored, artifact, self.state)
            atomic_write(UNITS / (self.unit + '.socket'), socket.encode(), 0o644)
            atomic_write(UNITS / (self.unit + '.service'), service.encode(), 0o644)
            command('systemctl', 'daemon-reload')
        finally:
            Path(temporary).unlink(missing_ok=True)

    def activate(self):
        data = self.installed()
        self.verify_artifact(data)
        self.ensure_exclusive_binding(data)
        # Stop an existing activation before rotating its handoff credential.
        self.stop()
        user = pwd.getpwnam(data['node_user'])
        atomic_write(self.state / 'upstream-token', secrets.token_hex(32).encode(),
                     uid=user.pw_uid, gid=user.pw_gid)
        try:
            command('systemctl', 'enable', '--now', self.unit + '.socket')
            command('systemctl', 'start', self.unit + '.service')
            self.route(data, 'bind', '--port', str(data['port']),
                       '--credential-file', str(self.state / 'upstream-token'))
        except Exception:
            self.stop()
            raise

    def restart(self):
        data = self.installed()
        self.verify_artifact(data)
        # Keep the socket bound while the service process is replaced.
        command('systemctl', 'is-active', '--quiet', self.unit + '.socket')
        self.route(data, 'unbind')
        try:
            command('systemctl', 'restart', self.unit + '.service')
            self.route(data, 'bind', '--port', str(data['port']),
                       '--credential-file', str(self.state / 'upstream-token'))
        except Exception:
            self.stop()
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ducktape', default='/usr/local/bin/ducktape')
    verbs = parser.add_subparsers(dest='verb', required=True)
    verbs.add_parser('install').add_argument('manifest', type=Path)
    for verb in ['activate', 'stop', 'restart']:
        verbs.add_parser(verb).add_argument('name')
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error('requires root to install systemd units and isolate service credentials')
    STATE.mkdir(mode=0o755, parents=True, exist_ok=True)
    with open(STATE / '.install.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if args.verb == 'install':
            data = checked_manifest(args.manifest)
            Installation(data['name'], args.ducktape).install(data)
        else:
            getattr(Installation(args.name, args.ducktape), args.verb)()


if __name__ == '__main__':
    main()
