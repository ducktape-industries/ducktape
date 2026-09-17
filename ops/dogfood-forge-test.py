#!/usr/bin/env python3
"""Drive dogfood-forge.sh against a node that keeps a real forge store on disk.

The script imports ducktape's own source into a LOCAL node's forge. It resolves
that node's workspace out of the ducktape home by the port its `node.toml`
serves, submits as that workspace's operator, and reads history back out of the
store on disk (`storage_dir/forge-repo/<repo>`) rather than over the network.
Each check below covers one of those, and nothing here contacts a real node,
GitHub, or the network.

The stand-in node is the smallest thing that can be told apart from a broken
one: it answers the three routes `ops/forge-import.py` calls, demands the
workspace's own admin token, and — the part that matters — actually indexes the
pack it is handed into the bare repository under the workspace's storage
directory and moves the ref. So "the object landed" is asserted against a real
git object database, not against a recorded response.
"""
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading

REPO_ROOT = Path(__file__).resolve().parent.parent
FORGE_REPO = 'ducktape'
NODE_KEY = 'ab' * 32


def git(*args, cwd, env=None):
    # Every child gets a deadline and a closed stdin. This runs inside
    # `make test`, and a git subcommand that decides to read a message or a
    # credential from the inherited terminal would wedge the whole gate.
    return subprocess.run(
        ['git', *args], cwd=cwd, env=env, check=True, timeout=60,
        stdin=subprocess.DEVNULL, capture_output=True, text=True,
    ).stdout.strip()


class Node(http.server.BaseHTTPRequestHandler):
    """The three routes forge-import.py speaks, over one bare repository."""

    def reply(self, payload, status=200):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == '/v1/status':
            self.reply({'public_key': NODE_KEY})
            return
        self.send_error(404)

    def do_POST(self):
        body = self.rfile.read(int(self.headers['Content-Length']))
        store = self.server.forge
        if self.path == '/v1/query':
            self.server.queries += 1
            head = self.server.head
            if self.server.lies_after and self.server.queries > self.server.lies_after:
                head = 'dd' * 20
            refs = [{'name': 'dev', 'head': head}] if head else []
            self.reply({'refs': refs})
            return
        # Every mutating route presents the workspace's operator credential;
        # a wrong one must not look like a transport failure.
        if self.headers.get('x-ducktape-admin-token') != self.server.token:
            self.send_error(401)
            return
        if self.path == '/v1/files/blob':
            # The node writes what it is handed into its own store. `--fix-thin`
            # because a pack built with `^previous` omits what the store holds.
            subprocess.run(['git', 'index-pack', '--stdin', '--fix-thin'],
                           cwd=store, input=body, check=True, capture_output=True)
            self.reply({'digest': hashlib.sha256(body).hexdigest()})
            return
        if self.path == '/v1/submit':
            submitted = json.loads(body)
            update = submitted['payload']['push_refs']['updates'][0]
            self.server.head = bytes(update['new_oid']).hex()
            git('update-ref', 'refs/heads/' + update['ref_name'],
                self.server.head, cwd=store)
            self.reply({'accepted': True})
            return
        self.send_error(404)

    def log_message(self, *_):
        pass


def start_node(forge, token):
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Node)
    server.forge = forge
    server.token = token
    server.head = None
    server.queries = 0
    # > 0 makes the node report a head it never took, from that query onward:
    # the shape the final verification exists to catch.
    server.lies_after = 0
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def workspace(home, port, *, token='operator-token'):
    """One workspace under the ducktape home, serving `port`."""
    ws = home / 'demo'
    (ws / 'storage' / 'forge-repo').mkdir(parents=True)
    (ws / 'network.toml').write_text('')
    (ws / 'node.toml').write_text(
        f'http_listen = "127.0.0.1:{port}"\nstorage_dir = "storage"\n')
    if token is not None:
        (ws / 'admin.token').write_text(token)
    return ws


def source_repo(root):
    """A checkout of the script, with its own history to import."""
    repo = root / 'source'
    (repo / 'ops').mkdir(parents=True)
    for tool in ('dogfood-forge.sh', 'forge-import.py'):
        shutil.copyfile(REPO_ROOT / 'ops' / tool, repo / 'ops' / tool)
    git('init', '-q', '-b', 'dev', '.', cwd=repo)
    git('config', 'user.email', 'fixture@example.test', cwd=repo)
    git('config', 'user.name', 'Fixture', cwd=repo)
    git('add', 'ops', cwd=repo)
    git('commit', '-qm', 'test: the script under test', cwd=repo)
    return repo


def run(repo, home, base_url, ref='HEAD'):
    env = {
        **os.environ,
        'DUCKTAPE_HOME': str(home),
        'DUCKTAPE_DEV_FORGE_URL': base_url,
        'SRC_REF': ref,
        'FORGE_REPO': FORGE_REPO,
        'GIT_AUTHOR_NAME': 'Fixture', 'GIT_AUTHOR_EMAIL': 'fixture@example.test',
        'GIT_COMMITTER_NAME': 'Fixture', 'GIT_COMMITTER_EMAIL': 'fixture@example.test',
    }
    for key in list(env):
        if key.startswith('GIT_CONFIG_'):
            del env[key]
    # forge-import.py gives its uploads an hour, which is right for a real pack
    # and wrong for a gate: a stand-in node that raises mid-request would hold
    # `make test` for that hour. 120s is far more than any fixture needs.
    result = subprocess.run(['bash', 'ops/dogfood-forge.sh'], cwd=repo, env=env,
                            timeout=120, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True)
    return result.returncode, result.stdout + result.stderr


def refuses_without_credential(root):
    """The workspace resolves; it just holds no operator credential."""
    home = root / 'home'
    home.mkdir()
    workspace(home, 40001, token=None)
    code, output = run(source_repo(root), home, 'http://127.0.0.1:40001')
    assert code != 0, output
    assert 'no readable admin.token' in output, output


def refuses_a_port_no_workspace_serves(root):
    """A port this box does not serve names no workspace, which is not the
    same failure as a workspace missing its token — the script says which."""
    home = root / 'home'
    home.mkdir()
    workspace(home, 40001)
    code, output = run(source_repo(root), home, 'http://127.0.0.1:40002')
    assert code != 0, output
    assert 'serves port 40002' in output, output
    assert 'admin.token' not in output, output


def imports_into_the_workspace_forge_store(root):
    """The import lands in the store on disk, and the next run reads its
    history back out of that store rather than off the network."""
    home = root / 'home'
    home.mkdir()
    repo = source_repo(root)
    server = start_node(None, 'operator-token')
    port = server.server_address[1]
    ws = workspace(home, port)
    store = ws / 'storage' / 'forge-repo' / FORGE_REPO
    store.mkdir(parents=True)
    git('init', '-q', '--bare', str(store), cwd=root)
    server.forge = store

    base_url = f'http://127.0.0.1:{port}'
    first = git('rev-parse', 'HEAD', cwd=repo)
    code, output = run(repo, home, base_url)
    assert code == 0, output
    assert 'creating Forge dev' in output, output
    # the object is IN the store, not merely acknowledged
    git('cat-file', '-e', first + '^{commit}', cwd=store)
    assert git('rev-parse', 'refs/heads/dev', cwd=store) == first
    assert f'verified Forge dev at {first}' in output, output

    # A second run fast-forwards, which reaches the store on disk for the
    # committed head: `git fetch <storage_dir>/forge-repo/<repo>`.
    git('commit', '-q', '--allow-empty', '-m', 'test: more history', cwd=repo)
    second = git('rev-parse', 'HEAD', cwd=repo)
    code, output = run(repo, home, base_url)
    assert code == 0, output
    assert 'fast-forwarding Forge dev' in output, output
    git('cat-file', '-e', second + '^{commit}', cwd=store)
    assert git('rev-parse', 'refs/heads/dev', cwd=store) == second

    # Nothing to do is not a failure, and must not push again.
    code, output = run(repo, home, base_url)
    assert code == 0, output
    assert 'already matches' in output, output
    server.shutdown()


def reads_forge_history_out_of_the_store_on_disk(root):
    """The fetch at the top of the divergence check is the only way the source
    learns a commit the forge has and it does not, and it reads it from the
    workspace's own store — no product HTTP route serves git objects.

    This needs a forge head the source has NEVER seen. With one it already
    holds, `git fetch <remote> <oid>:<ref>` never contacts the remote at all
    (it exits 0 against a dead port), so a run where the forge is behind proves
    nothing about where history comes from.
    """
    home = root / 'home'
    home.mkdir()
    repo = source_repo(root)
    server = start_node(None, 'operator-token')
    port = server.server_address[1]
    ws = workspace(home, port)
    store = ws / 'storage' / 'forge-repo' / FORGE_REPO
    store.mkdir(parents=True)
    git('init', '-q', '--bare', str(store), cwd=root)
    server.forge = store
    base_url = f'http://127.0.0.1:{port}'

    mine = git('rev-parse', 'HEAD', cwd=repo)
    code, output = run(repo, home, base_url)
    assert code == 0, output

    # a commit that exists ONLY in the forge store, built in the bare repo
    # -m, not stdin: `git commit-tree` with no message reads one from the
    # inherited stdin and blocks there forever.
    ahead = git('commit-tree', mine + '^{tree}', '-p', mine,
                '-m', 'a commit only the node has', cwd=store,
                env={**os.environ, 'GIT_AUTHOR_NAME': 'Node',
                     'GIT_AUTHOR_EMAIL': 'node@nodes.duck',
                     'GIT_COMMITTER_NAME': 'Node',
                     'GIT_COMMITTER_EMAIL': 'node@nodes.duck'})
    git('update-ref', 'refs/heads/dev', ahead, cwd=store)
    server.head = ahead
    assert subprocess.run(['git', 'cat-file', '-e', ahead], cwd=repo,
                          capture_output=True).returncode != 0, \
        'the source must not already hold the forge-only commit'

    code, output = run(repo, home, base_url)
    assert code == 0, output
    assert 'already contains GitHub dev' in output, output
    # it could only have judged that by reading `ahead` out of the store
    git('cat-file', '-e', ahead, cwd=repo)
    server.shutdown()


def refuses_a_head_the_node_did_not_take(root):
    """A successful push is not evidence. If the node reports a different head
    afterwards, the run fails rather than reporting a sync that did not land."""
    home = root / 'home'
    home.mkdir()
    repo = source_repo(root)
    server = start_node(None, 'operator-token')
    port = server.server_address[1]
    ws = workspace(home, port)
    store = ws / 'storage' / 'forge-repo' / FORGE_REPO
    store.mkdir(parents=True)
    git('init', '-q', '--bare', str(store), cwd=root)
    server.forge = store
    # Three reads happen, in this order: the script's opening `forge_head`,
    # forge-import's own `query` inside the push, and the script's verifying
    # `forge_head`. Only the last may lie — a lie to the second one aborts the
    # push itself, which is a different failure and not the one under test.
    server.lies_after = 2

    code, output = run(repo, home, f'http://127.0.0.1:{port}')
    assert code != 0, output
    assert 'verification failed' in output, output
    server.shutdown()


for check in (refuses_without_credential,
              refuses_a_port_no_workspace_serves,
              imports_into_the_workspace_forge_store,
              reads_forge_history_out_of_the_store_on_disk,
              refuses_a_head_the_node_did_not_take):
    with tempfile.TemporaryDirectory(prefix='dogfood-forge-') as tmp:
        check(Path(tmp))

print('dogfood-forge: credential refusal, unserved port, a real import into the '
      'workspace forge store, history read back out of that store, and a head '
      'the node never took all passed')
