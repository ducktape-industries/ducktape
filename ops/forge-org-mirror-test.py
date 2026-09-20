#!/usr/bin/env python3
"""Exercise forge-org-mirror.sh without GitHub or a node.

The node's `/v1/status` is a real HTTP fixture, the wallet/account verbs are a
fake `ducktape`, and Forge is a set of bare repositories. Two kinds of proof
live here and must not be confused:

- REF SEMANTICS (scope, idempotence, refusals, landing) run against those bare
  repositories through a `git` shim that rewrites `duck://` to a path;
- the SIGNED GATE is a real `git push --signed`: the fixture repositories carry
  `receive.certNonceSeed`, so git generates a real push certificate, and a
  `pre-receive` hook captures it for `ssh-keygen -Y check-novalidate`. That
  proves the run signs with the key it was given. It does NOT exercise forge's
  own certificate check (`crates/modules/apps/forge/src/pushcert.rs`), which
  needs a node.
"""

from pathlib import Path
import base64
import json
import os
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


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
    # Annotated: its ref names the tag OBJECT, not the commit, and the mirror
    # must carry that id through unchanged.
    git("tag", "-a", "v1", "-m", f"{name} v1", cwd=work)
    git("push", "-q", "origin", branch, "v1", cwd=work)
    tag_oid = git("rev-parse", "refs/tags/v1", cwd=work).stdout.strip()
    assert tag_oid != tagged, "the fixture tag must be an annotated tag object"
    # This branch proves the mirror does not copy every branch.
    git("checkout", "-qb", "feature/not-mirrored", cwd=work)
    (work / "feature").write_text("feature\n", encoding="utf-8")
    git("add", "feature", cwd=work)
    git("commit", "-qm", "feature", cwd=work)
    git("push", "-q", "origin", "feature/not-mirrored", cwd=work)
    return repo, work, head, tag_oid


def make_forge(forge, name, certs):
    """a Forge fixture repository that offers a push-cert nonce, like a node."""
    repo = forge / f"{name}.git"
    git("init", "--bare", "-q", str(repo))
    git("config", "receive.certNonceSeed", "forge-org-mirror-test", cwd=repo)
    hook = repo / "hooks" / "pre-receive"
    hook.write_text(
        "#!/bin/sh\n"
        f'if [ -n "$GIT_PUSH_CERT" ]; then git cat-file blob "$GIT_PUSH_CERT" > {certs}/{name}.cert; fi\n'
        "cat >/dev/null\n",
        encoding="utf-8",
    )
    hook.chmod(0o755)
    return repo


def ssh_pubkey_hex(pub_path):
    fields = pub_path.read_text(encoding="utf-8").split()
    blob = base64.b64decode(fields[1], validate=True)
    size = int.from_bytes(blob[4 + 11:4 + 11 + 4], "big")
    assert size == 32, blob
    return blob[4 + 11 + 4:].hex()


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
        "  *' account show '*) printf 'number=%s name=fixture-owner\\n%s\\n' \"$DUCKTAPE_ACCOUNT\" \"$DUCKTAPE_KEYS\" ;;\n"
        "  *' forge setup '*) printf 'remote workspace fixture#01234567: node http://127.0.0.1:1\\n'; printf 'duck://fixture-01234567/forge/<owner>/<repo> goes through fixture for owner %s\\n' \"$DUCKTAPE_OWNERS\" ;;\n"
        "  *' forge publish '*) printf 'published route\\n' ;;\n"
        "  *' account set-handle '*) printf 'handle set\\n' ;;\n"
        "  *) printf 'unexpected fake ducktape command\\n' >&2; exit 91 ;;\n"
        "esac\n",
        encoding="utf-8",
    )
    # Rewrites duck:// to the fixture Forge path and records every push's
    # argv; --signed and the signing configuration are passed through to the
    # real git, so a push here really is signed.
    (tools / "git").write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        "args=()\n"
        "push=0\n"
        "repo=''\n"
        "for arg in \"$@\"; do\n"
        "  case \"$arg\" in\n"
        "    push) push=1 ;;\n"
        "    duck://*) repo=\"${arg##*/}\"; arg=\"$FAKE_FORGE_ROOT/$repo.git\" ;;\n"
        "  esac\n"
        "  args+=(\"$arg\")\n"
        "done\n"
        "if [ \"$push\" -eq 1 ]; then\n"
        "  printf '%s\\n' \"$*\" >> \"$FAKE_PUSH_ARGS\"\n"
        "  if [ \"${FAKE_PUSH_FAIL:-}\" = \"$repo\" ]; then exit 73; fi\n"
        # A push that reports success and writes nothing: the run must still
        # fail, on its own re-read of Forge.
        "  if [ \"${FAKE_PUSH_SILENT:-}\" = \"$repo\" ]; then exit 0; fi\n"
        "fi\n"
        "exec /usr/bin/git \"${args[@]}\"\n",
        encoding="utf-8",
    )
    for tool in tools.iterdir():
        tool.chmod(0o755)
    return tools


def serve_status(chain):
    """the node fixture: `/v1/status` with whatever chain id `chain` holds."""
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path != "/v1/status":
                self.send_error(404)
                return
            body = json.dumps({"chain_id": chain[0]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_port}"


class Fixture:
    """one mirror invocation's environment, with the knobs cases vary."""

    def __init__(self, root, tools, forge, source, work, certs, node, keys):
        self.root, self.tools, self.forge = root, tools, forge
        self.source, self.work, self.certs = source, work, certs
        self.node, self.keys = node, keys
        self.account = "7"
        self.owners = "fixture-owner"

    def calls(self):
        path = self.root / "ducktape.calls"
        return path.read_text(encoding="utf-8") if path.exists() else ""

    def forget_calls(self):
        (self.root / "ducktape.calls").unlink(missing_ok=True)

    def env(self, **extra):
        env = os.environ.copy()
        env.update({
            "PATH": f"{self.tools}:{env['PATH']}",
            "GH_ARGS_FILE": str(self.root / "gh.args"),
            "DUCKTAPE_CALLS": str(self.root / "ducktape.calls"),
            "DUCKTAPE_PUBKEY": PUBKEY,
            "DUCKTAPE_ACCOUNT": self.account,
            "DUCKTAPE_KEYS": self.keys,
            "DUCKTAPE_OWNERS": self.owners,
            "FAKE_FORGE_ROOT": str(self.forge),
            "FAKE_PUSH_ARGS": str(self.root / "push.args"),
        })
        env.update(extra)
        return env

    def invoke(self, *extra, check=True, **env):
        args = [str(SCRIPT), "--org", ORG, "--source-base", str(self.source),
                "--work-dir", str(self.work), *extra]
        return run(*args, env=self.env(**env), check=check, stdin="mirror-password\n")

    def execute(self, *extra, check=True, network=NETWORK, **env):
        return self.invoke(
            "--execute", "--node", self.node, "--network", network,
            "--owner-account", "7", "--owner-handle", "fixture-owner",
            "--key", str(self.root / "fixture-owner.key"),
            "--git-signing-key", str(self.root / "git-signing-key"),
            *extra, check=check, **env,
        )


def refs(repo):
    return set(git("for-each-ref", "--format=%(refname)", cwd=repo).stdout.splitlines())


def ref_oid(repo, ref):
    return git("rev-parse", ref, cwd=repo).stdout.strip()


def assert_no_writes(fixture, result, reason):
    assert result.returncode != 0, result
    assert reason in result.stderr, result
    calls = fixture.calls()
    assert "forge publish" not in calls and "account set-handle" not in calls, calls
    assert not (fixture.root / "push.args").exists(), "a ref was pushed after a refusal"


def check_signed_push(fixture, repo, signing_pub):
    """the real push certificate the run produced, verified as a signature."""
    cert = (fixture.certs / f"{repo}.cert").read_text(encoding="utf-8")
    assert "-----BEGIN SSH SIGNATURE-----" in cert, cert
    payload, signature = cert.split("-----BEGIN SSH SIGNATURE-----", 1)
    (fixture.root / "cert.payload").write_text(payload, encoding="utf-8")
    (fixture.root / "cert.sig").write_text(
        "-----BEGIN SSH SIGNATURE-----" + signature, encoding="utf-8")
    verified = run(
        "ssh-keygen", "-Y", "check-novalidate", "-n", "git",
        "-s", str(fixture.root / "cert.sig"),
        stdin=payload, check=True,
    )
    fingerprint = run("ssh-keygen", "-lf", str(signing_pub)).stdout.split()[1]
    assert fingerprint in verified.stdout, (verified.stdout, fingerprint)
    assert "refs/heads/" in payload, payload


def main():
    with tempfile.TemporaryDirectory(prefix="forge-org-mirror-test-") as tmp:
        root = Path(tmp)
        source = root / "source"
        forge = root / "forge"
        certs = root / "certs"
        forge.mkdir()
        certs.mkdir()
        alpha, _alpha_work, alpha_start, alpha_tag = make_source(source, "alpha", "main")
        beta, _beta_work, beta_start, beta_tag = make_source(source, "beta", "dev")
        make_forge(forge, "alpha", certs)
        make_forge(forge, "beta", certs)
        tools = write_tools(root)
        key = root / "fixture-owner.key"
        key.write_text("encrypted fixture key\n", encoding="utf-8")
        signing = root / "git-signing-key"
        run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "fixture",
            "-f", str(signing))
        signing_pub = root / "git-signing-key.pub"
        signing_hex = ssh_pubkey_hex(signing_pub)
        keys = f"key=ed25519 {PUBKEY} wallet\nkey=ed25519 {signing_hex} git"
        chain = [NETWORK]
        server, node = serve_status(chain)
        fixture = Fixture(root, tools, forge, source, root / "work", certs, node, keys)

        plan = fixture.invoke()
        assert plan.returncode == 0, plan
        assert "refs/heads/main" in plan.stdout and "refs/heads/dev" in plan.stdout
        assert f"refs/tags/v1\t{alpha_tag}" in plan.stdout, plan.stdout
        assert "feature/not-mirrored" not in plan.stdout
        gh_args = (root / "gh.args").read_text(encoding="utf-8")
        assert "--paginate" in gh_args and "type=all" in gh_args
        assert "mirror-password" not in plan.stdout + plan.stderr
        assert not (root / "ducktape.calls").exists(), "plan contacted the node helper"

        # A network identity that is not EXACTLY the requested one — including
        # one that merely contains it — refuses before any account read, owner
        # write, or ref push.
        for served in (f"x{NETWORK}", f"{NETWORK}89", "other#01234567"):
            chain[0] = served
            fixture.forget_calls()
            refused = fixture.execute(check=False)
            assert_no_writes(fixture, refused, f"serves network {served}, not {NETWORK}")
            assert fixture.calls() == "", fixture.calls()
        chain[0] = NETWORK

        # An account that is not the requested one, a wallet that is not its
        # member, and a signing key that is not its member each refuse before
        # any write. The signing key is the one the push certificate carries,
        # so an unenrolled one can never authorize a Forge ref update.
        fixture.forget_calls()
        fixture.account = "9"
        assert_no_writes(fixture, fixture.execute(check=False), "answered for account 9")
        fixture.account = "7"

        fixture.forget_calls()
        fixture.keys = f"key=ed25519 {signing_hex} git"
        assert_no_writes(fixture, fixture.execute(check=False),
                         "wallet key is not a member key")

        fixture.forget_calls()
        fixture.keys = f"key=ed25519 {PUBKEY} wallet"
        assert_no_writes(fixture, fixture.execute(check=False),
                         "is not a member key of account 7")
        fixture.keys = keys

        # An owner handle the Git door does not serve refuses too — a handle
        # that is a substring of a served one is not the owner.
        fixture.forget_calls()
        fixture.owners = "fixture-owner-two, other"
        assert_no_writes(fixture, fixture.execute(check=False), "owner has no Git door")
        fixture.owners = "fixture-owner"

        fixture.forget_calls()
        first = fixture.execute()
        assert first.returncode == 0, first
        assert refs(forge / "alpha.git") == {"refs/heads/main", "refs/tags/v1"}
        assert refs(forge / "beta.git") == {"refs/heads/dev", "refs/tags/v1"}
        assert ref_oid(forge / "alpha.git", "refs/tags/v1") == alpha_tag
        assert ref_oid(forge / "beta.git", "refs/tags/v1") == beta_tag
        assert f"landed\talpha\trefs/tags/v1\t{alpha_tag}" in first.stdout, first.stdout
        push_args = (root / "push.args").read_text(encoding="utf-8")
        assert "--signed" in push_args, push_args
        check_signed_push(fixture, "alpha", signing_pub)
        (root / "push.args").unlink()

        second = fixture.execute()
        assert second.returncode == 0 and "unchanged" in second.stdout, second
        assert "push\t" not in second.stdout
        assert not (root / "push.args").exists(), "an unchanged mirror pushed"

        # A Forge branch that is not an ancestor is refused and left intact.
        alt = git("commit-tree", f"{alpha_start}^{{tree}}", "-p", alpha_start,
                  cwd=forge / "alpha.git", check=True).stdout.strip()
        git("update-ref", "refs/heads/main", alt, cwd=forge / "alpha.git")
        diverged = fixture.execute(check=False)
        assert diverged.returncode != 0 and "diverging Forge branch" in diverged.stderr, diverged
        assert ref_oid(forge / "alpha.git", "refs/heads/main") == alt
        git("update-ref", "refs/heads/main", alpha_start, cwd=forge / "alpha.git")

        # A differing existing tag is immutable and is refused.
        tag_alt = git("commit-tree", f"{beta_start}^{{tree}}", "-p", beta_start,
                      cwd=forge / "beta.git").stdout.strip()
        git("update-ref", "refs/tags/v1", tag_alt, cwd=forge / "beta.git")
        tag_refused = fixture.execute(check=False)
        assert tag_refused.returncode != 0 and "existing tag" in tag_refused.stderr, tag_refused
        assert ref_oid(forge / "beta.git", "refs/tags/v1") == tag_alt
        git("update-ref", "refs/tags/v1", beta_tag, cwd=forge / "beta.git")

        # A push that exits zero and writes nothing fails the run.
        commit(alpha, "main", "alpha second")
        commit(beta, "dev", "beta second")
        silent = fixture.execute(check=False, FAKE_PUSH_SILENT="alpha")
        assert silent.returncode != 0, silent
        assert "alpha: refs/heads/main is" in silent.stderr, silent.stderr

        # A later push failure is still nonzero after an earlier repo landed.
        partial = fixture.execute(check=False, FAKE_PUSH_FAIL="beta")
        assert partial.returncode != 0, partial
        assert "beta: signed Forge push failed" in partial.stderr, partial
        assert "landed\talpha\trefs/heads/main" in partial.stdout, partial.stdout
        server.shutdown()
    print("forge-org-mirror-test: ok")


if __name__ == "__main__":
    main()
