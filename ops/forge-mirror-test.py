#!/usr/bin/env python3
"""Drive forge-mirror.sh against a stand-in node and a local "GitHub".

The node is dogfood-forge-test.py's: it answers the three routes
`ops/forge-import.py` calls and indexes each pack into a real bare repository.
The source is a local bare repository standing where GitHub would. Nothing
here reaches a real node or the network.
"""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile

REPO_ROOT = Path(__file__).resolve().parent.parent
_spec = importlib.util.spec_from_file_location(
    'dogfood_forge_test', REPO_ROOT / 'ops' / 'dogfood-forge-test.py')
stand_in = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(stand_in)
git = stand_in.git


def commit(work, text):
    (work / 'lib.rs').write_text(text)
    git('add', 'lib.rs', cwd=work)
    git('commit', '-qm', text, cwd=work)
    return git('rev-parse', 'HEAD', cwd=work)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        github = root / 'github'
        git('init', '-q', '--bare', '-b', 'dev', str(github / 'sdk'), cwd=root)
        work = root / 'work'
        git('clone', '-q', str(github / 'sdk'), str(work), cwd=root)
        git('config', 'user.email', 'fixture@example.test', cwd=work)
        git('config', 'user.name', 'Fixture', cwd=work)
        git('checkout', '-q', '-b', 'dev', cwd=work)
        store = root / 'store'
        git('init', '-q', '--bare', str(store), cwd=root)
        server = stand_in.start_node(store, 'operator-token')
        token = root / 'admin.token'
        token.write_text('operator-token')
        env = {key: value for key, value in os.environ.items()
               if not key.startswith('GIT_CONFIG_')}
        env.update({
            'FORGE_MIRROR_NODE': f'http://127.0.0.1:{server.server_address[1]}',
            'FORGE_MIRROR_TOKEN_FILE': str(token),
            'FORGE_MIRROR_SOURCE': str(github),
            'FORGE_MIRROR_REPOS': 'sdk',
            'FORGE_MIRROR_BRANCHES': 'dev',
            'FORGE_MIRROR_STATE': str(root / 'state'),
        })

        def mirror():
            result = subprocess.run(
                ['bash', str(REPO_ROOT / 'ops' / 'forge-mirror.sh')], env=env,
                timeout=120, stdin=subprocess.DEVNULL, capture_output=True, text=True)
            print(result.stdout + result.stderr, end='')
            return result.returncode, result.stdout + result.stderr

        first = commit(work, 'one')
        git('push', '-q', 'origin', 'dev', cwd=work)
        code, output = mirror()
        assert code == 0 and f'empty -> {first}' in output, output
        assert server.head == first
        git('cat-file', '-e', first + '^{commit}', cwd=store)

        # twice is idempotent: nothing is pushed.
        code, output = mirror()
        assert code == 0 and f'Forge is at {first} already' in output, output
        assert '->' not in output, output

        second = commit(work, 'two')
        git('push', '-q', 'origin', 'dev', cwd=work)
        code, output = mirror()
        assert code == 0 and f'{first} -> {second}' in output, output
        assert server.head == second

        # a forced rewind on the source is refused, and Forge stays put.
        git('reset', '-q', '--hard', first, cwd=work)
        commit(work, 'rewritten')
        git('push', '-q', '--force', 'origin', 'dev', cwd=work)
        code, output = mirror()
        assert code != 0 and 'the source rewound' in output, output
        assert server.head == second

        # Forge at a commit this mirror never pushed: the other side moved.
        server.head = 'ee' * 20
        code, output = mirror()
        assert code != 0 and 'Forge moved outside this mirror' in output, output
        assert server.head == 'ee' * 20
    print('forge-mirror-test: ok')


if __name__ == '__main__':
    main()
