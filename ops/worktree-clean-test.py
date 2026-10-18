"""Exercise cleanup against a real temporary repository and borrowed target."""
import os
import pathlib
import subprocess
import sys
import tempfile

SCRIPT = pathlib.Path(__file__).with_name("worktree-clean.sh").resolve()


def run(*args, cwd=None, env=None):
    return subprocess.run(
        args, cwd=cwd, env=env, check=True, capture_output=True, text=True
    )


with tempfile.TemporaryDirectory() as temp:
    root = pathlib.Path(temp)
    repo = root / "repo"
    origin = root / "origin"
    worktree = repo / ".worktree/finished"
    run("git", "init", "--bare", str(origin))
    run("git", "init", "-b", "dev", str(repo))
    run("git", "config", "user.email", "fixture@example.test", cwd=repo)
    run("git", "config", "user.name", "Fixture", cwd=repo)
    (repo / ".gitignore").write_text(".worktree/\ntarget/\n")
    run("git", "add", ".gitignore", cwd=repo)
    run("git", "commit", "-m", "test: fixture", cwd=repo)
    run("git", "remote", "add", "origin", str(origin), cwd=repo)
    run("git", "push", "origin", "dev", cwd=repo)
    run("git", "worktree", "add", "-b", "fix/finished", str(worktree), "HEAD", cwd=repo)
    (worktree / "finished.txt").write_text("finished\n")
    run("git", "add", "finished.txt", cwd=worktree)
    run("git", "commit", "-m", "fix: finished", cwd=worktree)
    (repo / "moved.txt").write_text("dev moved\n")
    run("git", "add", "moved.txt", cwd=repo)
    run("git", "commit", "-m", "test: dev moved", cwd=repo)
    run("git", "merge", "--squash", "fix/finished", cwd=repo)
    run("git", "commit", "-m", "fix: finished (change 1)", cwd=repo)
    run("git", "push", "origin", "dev", cwd=repo)
    unmerged = repo / ".worktree/unmerged"
    run("git", "worktree", "add", "-b", "fix/unmerged", str(unmerged), "HEAD", cwd=repo)
    (unmerged / "unmerged.txt").write_text("unmerged\n")
    run("git", "add", "unmerged.txt", cwd=unmerged)
    run("git", "commit", "-m", "fix: unmerged", cwd=unmerged)
    fresh = repo / ".worktree/fresh"
    run("git", "worktree", "add", "-b", "fix/fresh", str(fresh), "HEAD", cwd=repo)
    target = worktree / "target"
    target.mkdir()
    cached = target / "live"
    cached.write_text("cached build")

    # Isolate process enumeration, not file inspection: unrelated nondumpable
    # processes on the test machine must not decide this fixture's result.
    # The scanner still reads the real child's cwd, descriptors and environment.
    shim = root / "process_namespace"
    shim.mkdir()
    (shim / "sitecustomize.py").write_text(
        "import os\nfrom pathlib import Path\n"
        "original = Path.iterdir\n"
        "def processes(path):\n"
        "    if path == Path('/proc'):\n"
        "        return iter([Path('/proc') / os.environ['FIXTURE_PID']])\n"
        "    return original(path)\n"
        "Path.iterdir = processes\n"
    )

    # Neither process has its cwd in the worktree being cleaned.
    for mode in ("descriptor", "environment"):
        setup = "f=open(sys.argv[1]);" if mode == "descriptor" else ""
        env = os.environ.copy()
        if mode == "environment":
            env["CARGO_TARGET_DIR"] = str(target)
        child = subprocess.Popen(
            [sys.executable, "-c", "import sys;" + setup
             + 'print("ready",flush=True); sys.stdin.readline()', str(cached)],
            cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
        )
        try:
            assert child.stdout.readline().strip() == "ready"
            scan_env = {**os.environ, "PYTHONPATH": str(shim), "FIXTURE_PID": str(child.pid)}
            result = run("bash", str(SCRIPT), "--yes", cwd=repo, env=scan_env)
            assert worktree.exists(), (
                f"cleanup deleted a borrowed target ({mode}): " + result.stdout
            )
            assert "live process" in result.stdout, result.stdout
        finally:
            child.communicate("\n", timeout=5)

    # An inspection failure must not turn into a zero-PID count.
    fake_bin = root / "bin"
    fake_bin.mkdir()
    scanner = fake_bin / "python3"
    scanner.write_text("#!/bin/sh\nexit 7\n")
    scanner.chmod(0o755)
    result = subprocess.run(
        ["bash", str(SCRIPT), "--yes"], cwd=repo, check=True,
        env={**os.environ, "PATH": str(fake_bin) + os.pathsep + os.environ["PATH"]},
        capture_output=True, text=True,
    )
    assert worktree.exists(), "inspection failure must retain the worktree"
    assert "could not be inspected" in result.stdout, result.stdout

    result = run("bash", str(SCRIPT), "--yes", cwd=repo, env=scan_env)
    assert not worktree.exists(), "released cache should be removable: " + result.stdout
    assert unmerged.exists(), "a commit dev lacks must keep its worktree: " + result.stdout
    assert "no commit on dev with this patch" in result.stdout, result.stdout
    assert fresh.exists(), "a worktree with no commit of its own must stay: " + result.stdout
    assert "no commit of its own" in result.stdout, result.stdout
    print("PASS: borrowed target retained by descriptor/environment, then the squash-merged worktree removed; unmerged and fresh ones kept")
