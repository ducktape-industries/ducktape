#!/usr/bin/env python3
"""Exercise forge-org-mirror.sh without GitHub or a node."""

from pathlib import Path
import os
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "ops" / "forge-org-mirror.sh"
ORG = "fixture-org"
NETWORK = "fixture#01234567"
PUBKEY = "11" * 32


def run(*args, cwd=None, env=None, check=True, stdin=None):
    result = subprocess.run(
        list(args), cwd=cwd, env=env, input=stdin, text=True,
        capture_output=True,
    )
    if check and result.returncode:
        raise AssertionError(f"{args!r}\nstdout={result.stdout}\nstderr={result.stderr}")
    return result


def git(*args, cwd=None, check=True):
    return run("git", *args, cwd=cwd, check=check)


def commit(repo, branch, message):
    work = repo.parent / f"{repo.stem}-{message.replace(' ', '-')}-work"
    git("clone", "-q", "--branch", branch, str(repo), str(work))
    git("config", "user.name", "Fixture", cwd=work)
    git("config", "user.email", "fixture@example.test", cwd=work)
    (work / "README").write_text(message + "\n", encoding="utf-8")
    git("add", "README", cwd=work)
    git("commit", "-qm", message, cwd=work)
    git("push", "-q", "origin", branch, cwd=work)
    return work, git("rev-parse", "HEAD", cwd=work).stdout.strip()


def initial_commit(repo, branch, message):
    work = repo.parent / f"{repo.stem}-{message.replace(' ', '-')}-work"
    git("init", "-q", "-b", branch, str(work))
    git("config", "user.name", "Fixture", cwd=work)
    git("config", "user.email", "fixture@example.test", cwd=work)
    (work / "README").write_text(message + "\n", encoding="utf-8")
    git("add", "README", cwd=work)
    git("commit", "-qm", message, cwd=work)
    git("remote", "add", "origin", str(repo), cwd=work)
    git("push", "-q", "origin", branch, cwd=work)
    return work, git("rev-parse", "HEAD", cwd=work).stdout.strip()


def make_source(root, name, branch):
    repo = root / ORG / f"{name}.git"
    repo.parent.mkdir(parents=True, exist_ok=True)
    git("init", "--bare", "-q", str(repo))
    work, head = initial_commit(repo, branch, f"{name} initial")
    (work / "tagged").write_text("tag\n", encoding="utf-8")
    git("add", "tagged", cwd=work)
    git("commit", "-qm", f"{name} tagged", cwd=work)
    tagged = git("rev-parse", "HEAD", cwd=work).stdout.strip()
    git("tag", "v1", cwd=work)
    git("push", "-q", "origin", branch, "v1", cwd=work)
    # This branch proves the mirror does not copy every branch.
    git("checkout", "-qb", "feature/not-mirrored", cwd=work)
    (work / "feature").write_text("feature\n", encoding="utf-8")
    git("add", "feature", cwd=work)
    git("commit", "-qm", "feature", cwd=work)
    git("push", "-q", "origin", "feature/not-mirrored", cwd=work)
    return repo, work, head, tagged


def write_tools(root):
    tools = root / "tools"
    tools.mkdir()
    (tools / "gh").write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$*\" > \"$GH_ARGS_FILE\"\n"
        "printf '%s\\n' '[[{\"name\":\"alpha\",\"default_branch\":\"main\",\"private\":true}],[{\"name\":\"beta\",\"default_branch\":\"dev\",\"private\":true}]]'\n",
        encoding="utf-8",
    )
    (tools / "ducktape").write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$*\" >> \"$DUCKTAPE_CALLS\"\n"
        "case \" $* \" in\n"
        "  *' user key status '*) printf 'encrypted %s\\n' \"$DUCKTAPE_PUBKEY\" ;;\n"
        "  *' account show '*) printf 'number=7 name=fixture-owner\\nkey=ed25519 %s\\n' \"$DUCKTAPE_PUBKEY\" ;;\n"
        "  *' forge setup '*) printf 'remote workspace fixture#01234567: node http://127.0.0.1:1\\n'; printf 'duck://fixture-01234567/forge/<owner>/<repo> goes through fixture for owner fixture-owner\\n' ;;\n"
        "  *' forge publish '*) printf 'published route\\n' ;;\n"
        "  *' account set-handle '*) printf 'handle set\\n' ;;\n"
        "  *) printf 'unexpected fake ducktape command\\n' >&2; exit 91 ;;\n"
        "esac\n",
        encoding="utf-8",
    )
    (tools / "git").write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        "args=()\n"
        "push=0\n"
        "repo=''\n"
        "for arg in \"$@\"; do\n"
        "  case \"$arg\" in\n"
        "    push) push=1 ;;\n"
        "    --signed|--porcelain) continue ;;\n"
        "    duck://*) repo=\"${arg##*/}\"; arg=\"$FAKE_FORGE_ROOT/$repo.git\" ;;\n"
        "  esac\n"
        "  args+=(\"$arg\")\n"
        "done\n"
        "if [ \"$push\" -eq 1 ]; then\n"
        "  if [ \"${FAKE_PUSH_FAIL:-}\" = \"$repo\" ]; then exit 73; fi\n"
        "  unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GIT_CONFIG_KEY_1 GIT_CONFIG_VALUE_1 GIT_CONFIG_KEY_2 GIT_CONFIG_VALUE_2\n"
        "fi\n"
        "exec /usr/bin/git \"${args[@]}\"\n",
        encoding="utf-8",
    )
    for tool in tools.iterdir():
        tool.chmod(0o755)
    return tools


def invoke(root, tools, forge, source, work, *extra, check=True, stdin="mirror-password\n"):
    env = os.environ.copy()
    env.update({
        "PATH": f"{tools}:{env['PATH']}",
        "GH_ARGS_FILE": str(root / "gh.args"),
        "DUCKTAPE_CALLS": str(root / "ducktape.calls"),
        "DUCKTAPE_PUBKEY": PUBKEY,
        "FAKE_FORGE_ROOT": str(forge),
    })
    args = [str(SCRIPT), "--org", ORG, "--source-base", str(source), "--work-dir", str(work)]
    args.extend(extra)
    return run(*args, env=env, check=check, stdin=stdin)


def execute_args(key, signing):
    return (
        "--execute", "--node", "http://127.0.0.1:1", "--network", NETWORK,
        "--owner-account", "7", "--owner-handle", "fixture-owner", "--key", str(key),
        "--git-signing-key", str(signing),
    )


def refs(repo):
    return set(git("for-each-ref", "--format=%(refname)", cwd=repo).stdout.splitlines())


def main():
    with tempfile.TemporaryDirectory(prefix="forge-org-mirror-test-") as tmp:
        root = Path(tmp)
        source = root / "source"
        forge = root / "forge"
        forge.mkdir()
        alpha, alpha_work, alpha_start, alpha_tag = make_source(source, "alpha", "main")
        beta, beta_work, beta_start, beta_tag = make_source(source, "beta", "dev")
        git("init", "--bare", "-q", str(forge / "alpha.git"))
        git("init", "--bare", "-q", str(forge / "beta.git"))
        tools = write_tools(root)
        key = root / "fixture-owner.key"
        signing = root / "git-signing-key"
        key.write_text("encrypted fixture key\n", encoding="utf-8")
        signing.write_text("fixture ssh key\n", encoding="utf-8")
        work = root / "work"

        plan = invoke(root, tools, forge, source, work)
        assert plan.returncode == 0, plan
        assert "refs/heads/main" in plan.stdout and "refs/heads/dev" in plan.stdout
        assert "refs/tags/v1" in plan.stdout
        assert "feature/not-mirrored" not in plan.stdout
        gh_args = (root / "gh.args").read_text(encoding="utf-8")
        assert "--paginate" in gh_args and "type=all" in gh_args
        assert "mirror-password" not in plan.stdout + plan.stderr
        assert not (root / "ducktape.calls").exists(), "plan contacted the node helper"

        first = invoke(root, tools, forge, source, work, *execute_args(key, signing))
        assert first.returncode == 0, first
        assert refs(forge / "alpha.git") == {"refs/heads/main", "refs/tags/v1"}
        assert refs(forge / "beta.git") == {"refs/heads/dev", "refs/tags/v1"}
        second = invoke(root, tools, forge, source, work, *execute_args(key, signing))
        assert second.returncode == 0 and "unchanged" in second.stdout, second
        assert "push\t" not in second.stdout

        # A Forge branch that is not an ancestor is refused and left intact.
        alt = git("commit-tree", f"{alpha_start}^{{tree}}", "-p", alpha_start,
                   cwd=forge / "alpha.git", check=True).stdout.strip()
        git("update-ref", "refs/heads/main", alt, cwd=forge / "alpha.git")
        diverged = invoke(root, tools, forge, source, work, *execute_args(key, signing), check=False)
        assert diverged.returncode != 0 and "diverging Forge branch" in diverged.stderr, diverged
        assert git("rev-parse", "refs/heads/main", cwd=forge / "alpha.git").stdout.strip() == alt
        git("update-ref", "refs/heads/main", alpha_tag, cwd=forge / "alpha.git")

        # A differing existing tag is immutable and is refused.
        tag_alt = git("commit-tree", f"{beta_start}^{{tree}}", "-p", beta_start,
                      cwd=forge / "beta.git").stdout.strip()
        git("update-ref", "refs/tags/v1", tag_alt, cwd=forge / "beta.git")
        tag_refused = invoke(root, tools, forge, source, work, *execute_args(key, signing), check=False)
        assert tag_refused.returncode != 0 and "existing tag" in tag_refused.stderr, tag_refused
        assert git("rev-parse", "refs/tags/v1", cwd=forge / "beta.git").stdout.strip() == tag_alt
        git("update-ref", "refs/tags/v1", beta_tag, cwd=forge / "beta.git")

        # A later push failure is still nonzero after an earlier repo landed.
        commit(alpha, "main", "alpha second")
        commit(beta, "dev", "beta second")
        env = os.environ.copy()
        env["FAKE_PUSH_FAIL"] = "beta"
        env.update({"PATH": f"{tools}:{env['PATH']}", "GH_ARGS_FILE": str(root / "gh.args"),
                    "DUCKTAPE_CALLS": str(root / "ducktape.calls"), "DUCKTAPE_PUBKEY": PUBKEY,
                    "FAKE_FORGE_ROOT": str(forge)})
        partial = run(str(SCRIPT), "--org", ORG, "--source-base", str(source), "--work-dir", str(work),
                      *execute_args(key, signing), env=env, check=False, stdin="mirror-password\n")
        assert partial.returncode != 0, partial
        assert "beta: signed Forge push failed" in partial.stderr, partial
    print("forge-org-mirror-test: ok")


if __name__ == "__main__":
    main()
