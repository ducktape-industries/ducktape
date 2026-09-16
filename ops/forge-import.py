#!/usr/bin/env python3
"""Publish a local Git tip as the node through generic query/blob/submit APIs."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import urllib.request


def query(url, repo, branch):
    data = json.dumps({'target': 'forge', 'query': {'list_refs': {'repo': repo}}}).encode()
    request = urllib.request.Request(url + '/v1/query', data, {'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        refs = json.load(response)['refs']
    return next((ref['head'] for ref in refs if ref['name'] == branch), None)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['head', 'push'])
    parser.add_argument('--node-url', required=True)
    parser.add_argument('--token-file', type=Path)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--branch', required=True)
    parser.add_argument('--tip', default='HEAD')
    args = parser.parse_args()
    url = args.node_url.rstrip('/')
    previous = query(url, args.repo, args.branch)
    if args.action == 'head':
        print(previous or '')
        return
    if args.token_file is None:
        parser.error('push requires --token-file')
    tip = subprocess.check_output(['git', 'rev-parse', '--verify', args.tip + '^{commit}'], text=True).strip()
    if previous == tip:
        print(tip)
        return
    revisions = tip + '\n'
    if previous:
        subprocess.run(['git', 'merge-base', '--is-ancestor', previous, tip], check=True)
        revisions += '^' + previous + '\n'
    # Read at each operation so a node restart cannot leave stale credentials.
    def post(path, data, kind, length=None):
        token = args.token_file.read_text().strip()
        headers = {'Content-Type': kind, 'x-ducktape-admin-token': token}
        # With a length, urllib streams the file object instead of reading it:
        # a pack is as big as the history it carries, and neither this script
        # nor the node it uploads to ever holds one.
        if length is not None:
            headers['Content-Length'] = str(length)
        request = urllib.request.Request(url + path, data, headers)
        with urllib.request.urlopen(request, timeout=3600) as response:
            return json.load(response)
    with tempfile.TemporaryFile() as pack:
        subprocess.run(['git', 'pack-objects', '--stdout', '--revs'], input=revisions.encode(), stdout=pack, check=True)
        size = pack.tell()
        pack.seek(0)
        digest = post('/v1/files/blob', pack, 'application/octet-stream', size)['digest']
    message = {'push_refs': {'repo': args.repo, 'updates': [{
        'ref_name': args.branch, 'prev_oid': list(bytes.fromhex(previous)) if previous else None,
        'new_oid': list(bytes.fromhex(tip))}], 'pack_digest': list(bytes.fromhex(digest)), 'cert': None}}
    post('/v1/submit', json.dumps({'target': 'forge', 'payload': message, 'required_blob': digest}).encode(), 'application/json')
    print(tip)


if __name__ == '__main__':
    main()
