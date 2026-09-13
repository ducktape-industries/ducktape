#!/usr/bin/env python3
"""Run the mirror script against real Git repos with a size-rejecting transport."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

SCRIPT = Path(__file__).with_name('dogfood-forge.sh').resolve()
GIT = shutil.which('git')

with tempfile.TemporaryDirectory(prefix='dogfood-forge-') as tmp:
    root = Path(tmp)
    repo = root / 'source'
    remote = root / 'server/forge/ducktape'
    repo.mkdir()
    remote.mkdir(parents=True)
    env = dict(os.environ, GIT_AUTHOR_NAME='Test', GIT_AUTHOR_EMAIL='test@example.com',
               GIT_COMMITTER_NAME='Test', GIT_COMMITTER_EMAIL='test@example.com',
               DUCKTAPE_HOME=str(root / 'home'), DUCKTAPE_DEV_FORGE_URL=f'file://{root}/server',
               SRC_REF='HEAD', FORGE_REMOTE='mirror', FORGE_REPO='ducktape',
               REAL_GIT=GIT, TEST_REMOTE=str(remote), TEST_LIMIT='2')
    # Do not inherit an operator credential or caller-specific Git configuration.
    for key in list(env):
        if key.startswith('GIT_CONFIG_'):
            del env[key]

    def git(*args, cwd=repo):
        return subprocess.check_output([GIT, *args], cwd=cwd, env=env, text=True).strip()

    git('init', '-q')
    git('init', '--bare', '-q', str(remote))
    (repo / 'ops').mkdir()
    shutil.copyfile(SCRIPT, repo / 'ops/dogfood-forge.sh')
    bins = root / 'bin'
    bins.mkdir()
    (bins / 'curl').write_text('#!/bin/sh\nexit 0\n')
    (bins / 'git').write_text('''#!/usr/bin/env python3
import os, subprocess, sys
args = sys.argv[1:]
git = os.environ['REAL_GIT']
if args[0] == 'push':
    tip, ref = args[-1].split(':', 1)
    old = subprocess.run([git, '--git-dir=' + os.environ['TEST_REMOTE'], 'rev-parse', '--verify', ref], capture_output=True, text=True)
    revs = [tip] + (['^' + old.stdout.strip()] if old.returncode == 0 else [])
    count = int(subprocess.check_output([git, 'rev-list', '--count', *revs]))
    if count > int(os.environ['TEST_LIMIT']):
        print('error: RPC failed; HTTP ' + os.environ.get('TEST_HTTP', '413'), file=sys.stderr)
        sys.exit(1)
os.execv(git, [git, *args])
''')
    for binary in bins.iterdir():
        binary.chmod(0o755)
    env['PATH'] = f'{bins}:{env["PATH"]}'

    def commit():
        git('commit', '--allow-empty', '-qm', 'history')

    def mirror(ok=True):
        result = subprocess.run(['bash', 'ops/dogfood-forge.sh'], cwd=repo, env=env,
                                capture_output=True, text=True)
        assert (result.returncode == 0) == ok, result.stdout + result.stderr
        return result.stdout + result.stderr

    def tip():
        return git('--git-dir=' + str(remote), 'rev-parse', 'refs/heads/dev')

    for _ in range(9):
        commit()
    assert 'importing ancestor' in mirror()
    assert tip() == git('rev-parse', 'HEAD')
    assert 'already matches' in mirror()
    for _ in range(5):
        commit()
    assert 'importing ancestor' in mirror()
    assert tip() == git('rev-parse', 'HEAD')
    accepted = tip()
    for _ in range(3):
        commit()
    env['TEST_HTTP'] = '500'
    assert 'importing ancestor' not in mirror(ok=False)
    assert tip() == accepted
    del env['TEST_HTTP']
    env['TEST_LIMIT'] = '0'
    assert 'cannot split its history further' in mirror(ok=False)
    assert tip() == accepted
    env['TEST_LIMIT'] = '2'
    mirror()
    assert tip() == git('rev-parse', 'HEAD')
    # The remote tip can enter the source through a merge's second parent.
    # No candidate split may rewind it onto the unrelated first-parent branch.
    accepted = tip()
    git('checkout', '-q', '--orphan', 'other')
    for _ in range(3):
        commit()
    git('merge', '--allow-unrelated-histories', '-qm', 'join', accepted)
    assert 'cannot split its history further' in mirror(ok=False)
    assert tip() == accepted
    env['TEST_LIMIT'] = '10'
    mirror()
    assert tip() == git('rev-parse', 'HEAD')
    assert git('--git-dir=' + str(remote), 'for-each-ref', '--format=%(refname)', 'refs/heads') == 'refs/heads/dev'

print('dogfood-forge: initial import, resume, fast-forward, HTTP failures and indivisible commit passed')
