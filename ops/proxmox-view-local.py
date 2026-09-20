#!/usr/bin/env python3
"""Run a dedicated, loopback-only three-node canary until interrupted.

This foreground supervisor owns only children it starts. Every run copies its
release into a new scratch directory; restarting never reuses network state.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import time
import tomllib
import urllib.request
import uuid


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--modules', type=Path, required=True)
    parser.add_argument('--revision', required=True)
    parser.add_argument('--ui-revision', required=True)
    parser.add_argument('--seconds', type=int, default=7200)
    args = parser.parse_args()
    if args.seconds < 1 or args.seconds > 43200:
        parser.error('--seconds must be between 1 and 43200')
    for revision in (args.revision, args.ui_revision):
        if not re.fullmatch(r'[0-9a-f]{40}', revision):
            parser.error('revisions must be exact 40-character hashes')
    version = subprocess.check_output([str(args.binary.resolve()), '--version'], text=True).strip()
    if not version.endswith('+' + args.revision[:9]):
        parser.error('binary version does not match --revision')
    owners = ('governance', 'files', 'pages', 'chat', 'forge')
    for owner in owners:
        if not (args.modules / (owner + '.view.wasm')).is_file():
            parser.error('all five owner views must be staged')
    if args.modules.is_symlink() or not args.modules.is_dir():
        parser.error('modules must be a real directory')
    if any(path.is_symlink() for path in args.modules.rglob('*')):
        parser.error('release modules cannot contain symlinks')

    work = Path(__file__).resolve().parent.parent
    owner = 'ducktape-local-view-' + uuid.uuid4().hex
    run = work / 'target' / owner
    run.mkdir(parents=True)
    release = run / 'release'
    release.mkdir()
    binary = release / 'ducktape'
    shutil.copy2(args.binary, binary)
    shutil.copytree(args.modules, release / 'modules')
    record = dict(owner=owner, directory=str(run), revision=args.revision,
                  ui_revision=args.ui_revision, binary_version=version,
                  state='prepared', reset=True, kind='local-processes',
                  files={str(path.relative_to(release)): digest(path)
                         for path in sorted(release.rglob('*')) if path.is_file()})
    marker = run / 'owner.json'

    def save():
        marker.write_text(json.dumps(record, indent=2) + '\n')

    save()
    spec = importlib.util.spec_from_file_location('lane', work / 'ops/proxmox-view-lane.py')
    lane = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(lane)
    reservations = []

    def reserve(kind=socket.SOCK_STREAM):
        connection = socket.socket(socket.AF_INET, kind)
        connection.bind(('127.0.0.1', 0))
        reservations.append(connection)
        return connection.getsockname()[1]

    ports = {name: [reserve(kind) for _ in range(3)] for name, kind in
             [('p2p', socket.SOCK_STREAM), ('rpc', socket.SOCK_STREAM),
              ('http', socket.SOCK_STREAM), ('wg', socket.SOCK_DGRAM),
              ('invite', socket.SOCK_DGRAM)]}
    configs = []
    for index in range(3):
        root = run / f'node{index}'
        (root / 'network').mkdir(parents=True)
        lane.ROOT = str(root)
        values = tomllib.loads(lane.node_config(record, ['127.0.0.1'] * 3, index))
        values.update(modules=str(release / 'modules'),
                      listen=f'127.0.0.1:{ports["p2p"][index]}',
                      peer_addrs=[f'127.0.0.1:{port}' for port in ports['p2p']],
                      rpc_listen=f'127.0.0.1:{ports["rpc"][index]}',
                      http_listen=f'127.0.0.1:{ports["http"][index]}',
                      wireguard_listen=f'127.0.0.1:{ports["wg"][index]}',
                      invite_listen=f'127.0.0.1:{ports["invite"][index]}')
        config = root / 'network/node.toml'
        config.write_text(''.join(f'{key} = {json.dumps(value)}\n' for key, value in values.items()))
        configs.append(config)
    record.update(ports=ports, configs=[str(path) for path in configs],
                  http=[f'http://127.0.0.1:{port}' for port in ports['http']],
                  supervisor_pid=os.getpid())
    save()
    for connection in reservations:
        connection.close()
    children, logs = [], []
    observer = None

    def stop(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        for index, config in enumerate(configs):
            log = (run / f'node{index}.log').open('wb')
            logs.append(log)
            env = dict(os.environ, DUCKTAPE_HOME=str(config.parent / 'home'),
                       DUCKTAPE_MODULES_DIR=str(release / 'modules'), RUST_LOG='info',
                       TOKIO_WORKER_THREADS='2')
            children.append(subprocess.Popen([str(binary), 'node', 'run', '--config', str(config)],
                                             stdout=log, stderr=subprocess.STDOUT, env=env))
        record.update(state='started_unverified', pids=[child.pid for child in children])
        save()
        print(json.dumps(record), flush=True)
        deadline = time.monotonic() + 480
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        while True:
            if any(child.poll() is not None for child in children):
                raise RuntimeError('a task node exited; inspect scratch logs')
            if time.monotonic() >= deadline:
                raise TimeoutError('three-node startup deadline expired')
            try:
                statuses = []
                for base in record['http']:
                    request = urllib.request.Request(base + '/v1/query',
                        data=b'{"target":"modules","query":"module_status"}',
                        headers={'Content-Type': 'application/json'})
                    with opener.open(request, timeout=2) as response:
                        statuses.append(json.load(response)['module_status']['modules'])
                if statuses[0] and statuses[0] == statuses[1] == statuses[2]:
                    break
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(1)
        observer = subprocess.Popen(['node', str(work / 'ops/proxmox-view-observe.mjs')] +
                                    [base.replace('http:', 'ws:') + '/v1/ws' for base in record['http']],
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        output, error = observer.communicate(timeout=190)
        if observer.returncode:
            raise RuntimeError(error)
        record.update(state='ready', common_root=json.loads(output), initial_modules=statuses[0])
        save()
        print(json.dumps({'state': 'ready', 'record': str(marker), 'http': record['http'],
                          'proof': record['common_root']}), flush=True)
        deadline = time.monotonic() + args.seconds
        while time.monotonic() < deadline:
            if any(child.poll() is not None for child in children):
                raise RuntimeError('a task node exited; inspect scratch logs')
            time.sleep(1)
    except KeyboardInterrupt:
        pass
    finally:
        owned = ([observer] if observer else []) + children
        for child in owned:
            if child.poll() is None:
                child.terminate()
        for child in owned:
            try:
                child.wait(timeout=30)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        for log in logs:
            log.close()
        record.update(state='stopped', exit_codes=[child.returncode for child in children])
        save()
        print(json.dumps({'state': 'stopped', 'record': str(marker)}), flush=True)


if __name__ == '__main__':
    main()
