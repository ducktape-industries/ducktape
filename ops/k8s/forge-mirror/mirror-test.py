"""Exercise forge-mirror against local git repositories standing in for forge and GitHub, a local websocket and HTTP node, and a local GitHub API; run by `make test`."""
import dataclasses
import http.server
import importlib.util
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import threading
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("mirror", HERE / "mirror.py")
mirror = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mirror)
mirror.say = lambda event, **fields: None

CORE_DEV_KEY = "722f636f72650000726566732f68656164732f6465760000"
CORE_DEV_VALUE = "14000000f782668e3842f577a623e4a70294f214f8c769ec"
GIT_ENV = {
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@t",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@t",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}
HAS_CRYPTOGRAPHY = importlib.util.find_spec("cryptography") is not None


def git(*args, cwd=None):
    ran = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, env=dict(os.environ, **GIT_ENV))
    if ran.returncode != 0:
        raise AssertionError(f"git {' '.join(args)}: {ran.stderr}")
    return ran.stdout.strip()


def ref_value(oid):
    return mirror.borsh_bytes(bytes.fromhex(oid))


class Said:
    def __enter__(self):
        self.events = []
        mirror.say = lambda event, **fields: self.events.append((event, fields))
        return self

    def __exit__(self, *exc):
        mirror.say = lambda event, **fields: None

    def named(self, name):
        return [fields for event, fields in self.events if event == name]


class Encoding(unittest.TestCase):
    def test_a_forge_ref_key_is_the_one_forge_writes(self):
        pair = mirror.Pair("core", "ducktape-industries/ducktape", "dev")
        self.assertEqual(mirror.ref_key(pair).hex(), CORE_DEV_KEY)

    def test_a_nul_in_a_key_part_is_escaped(self):
        self.assertEqual(mirror.key_part(b"a\x00b"), b"a\x00\xffb\x00\x00")

    def test_a_ref_value_is_its_oid(self):
        self.assertEqual(mirror.oid_of(bytes.fromhex(CORE_DEV_VALUE)), "f782668e3842f577a623e4a70294f214f8c769ec")
        self.assertIsNone(mirror.oid_of(None))

    def test_a_change_frame_decodes_its_writes(self):
        frame = mirror.u64(81452) + bytes(32) + mirror.borsh_vec(
            [mirror.borsh_bytes(b"k1") + b"\x01" + mirror.borsh_bytes(b"v1"), mirror.borsh_bytes(b"k2") + b"\x00"]
        )
        change = mirror.decode_change(frame)
        self.assertEqual(change.height, 81452)
        self.assertEqual(change.writes, [(b"k1", b"v1"), (b"k2", None)])

    def test_a_failure_logs_its_first_attempt_then_every_tenth(self):
        logged = [attempts for attempts in range(1, 31) if mirror.worth_a_line(attempts)]
        self.assertEqual(logged, [1, 10, 20, 30])


def frame(opcode, payload, final=True):
    head = bytes([(0x80 if final else 0) | opcode])
    if len(payload) < 126:
        return head + bytes([len(payload)]) + payload
    return head + bytes([126]) + len(payload).to_bytes(2, "big") + payload


def read_client_frame(connection):
    head = connection.recv(2)
    mask = connection.recv(4)
    payload = connection.recv(head[1] & 0x7F)
    return head[0] & 0x0F, bytes(byte ^ mask[at % 4] for at, byte in enumerate(payload))


class Websocket(unittest.TestCase):
    def test_changes_answers_pings_joins_fragments_and_ends_on_close(self):
        listener = socket.create_server(("127.0.0.1", 0))
        port = listener.getsockname()[1]
        change = mirror.u64(7) + bytes(32) + mirror.borsh_vec([mirror.borsh_bytes(b"r/x") + b"\x00", mirror.borsh_bytes(b"k" * 200) + b"\x01" + mirror.borsh_bytes(b"v")])
        heard = {}

        def serve():
            connection, _ = listener.accept()
            request = b""
            while not request.endswith(b"\r\n\r\n"):
                request += connection.recv(1024)
            heard["request"] = request.decode()
            connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            connection.sendall(frame(0x9, b"hi"))
            heard["pong"] = read_client_frame(connection)
            connection.sendall(frame(0x2, change[:10], final=False) + frame(0x0, change[10:]))
            connection.sendall(frame(0x8, b""))
            connection.close()

        server = threading.Thread(target=serve)
        server.start()
        changes = list(mirror.Changes(f"http://127.0.0.1:{port}", "forge"))
        server.join()
        listener.close()
        self.assertIn("GET /v1/changes/forge HTTP/1.1", heard["request"])
        self.assertEqual(heard["pong"], (0xA, b"hi"))
        self.assertEqual([(c.height, c.writes) for c in changes], [(7, [(b"r/x", None), (b"k" * 200, b"v")])])

    def test_a_refused_upgrade_stops(self):
        listener = socket.create_server(("127.0.0.1", 0))
        port = listener.getsockname()[1]

        def serve():
            connection, _ = listener.accept()
            connection.recv(1024)
            connection.sendall(b"HTTP/1.1 404 Not Found\r\n\r\n")
            connection.close()

        server = threading.Thread(target=serve)
        server.start()
        with self.assertRaises(mirror.Stopped):
            mirror.Changes(f"http://127.0.0.1:{port}", "nothing")
        server.join()
        listener.close()

    def test_a_follow_the_peer_resets_ends_with_what_it_carried(self):
        change = mirror.Change(9, [(b"r/x", b"v")])

        def reset():
            yield change
            raise ConnectionResetError(104, "Connection reset by peer")

        inbox = mirror.queue.Queue()
        mirror.pump("forge", reset(), inbox)
        self.assertEqual([inbox.get(), inbox.get()], [mirror.Wrote("forge", change), mirror.Ended("forge")])
        self.assertTrue(inbox.empty())


def serve(handler):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}"


class NodeGet(unittest.TestCase):
    def test_get_posts_a_confirmed_get_and_reads_the_option(self):
        bodies = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                bodies.append((self.path, self.rfile.read(int(self.headers["Content-Length"]))))
                reply = b"\x01" + mirror.borsh_bytes(b"value") if len(bodies) == 1 else b"\x00"
                self.send_response(200)
                self.send_header("Content-Length", str(len(reply)))
                self.end_headers()
                self.wfile.write(reply)

            def log_message(self, *args):
                pass

        server, url = serve(Handler)
        node = mirror.Node(url + "/")
        self.assertEqual(node.get("forge", b"key"), b"value")
        self.assertIsNone(node.get("forge", b"other"))
        server.shutdown()
        server.server_close()
        self.assertEqual(bodies[0], ("/v1/get", b"\x00" + mirror.borsh_text("forge") + mirror.borsh_bytes(b"key")))


def app_key():
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import rsa

    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    return key, key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption())


def unpadded(part):
    return mirror.base64.urlsafe_b64decode(part + "=" * (-len(part) % 4))


@unittest.skipUnless(HAS_CRYPTOGRAPHY, "the App's JWT needs python's cryptography")
class App(unittest.TestCase):
    def test_the_jwt_is_rs256_signed_by_the_app_key_and_names_the_app(self):
        from cryptography.hazmat.primitives import hashes
        from cryptography.hazmat.primitives.asymmetric import padding

        key, pem = app_key()
        token = mirror.App("1234", "99", pem, ["ducktape"], clock=lambda: 1_000_000).jwt()
        header, claims, signature = token.split(".")
        self.assertEqual(json.loads(unpadded(header)), {"alg": "RS256", "typ": "JWT"})
        self.assertEqual(json.loads(unpadded(claims)), {"iat": 999_940, "exp": 1_000_540, "iss": 1234})
        key.public_key().verify(unpadded(signature), f"{header}.{claims}".encode(), padding.PKCS1v15(), hashes.SHA256())

    def test_git_auth_sends_the_token_as_basic_auth_to_github_only(self):
        auth = mirror.git_auth("tok")
        self.assertEqual(auth["GIT_CONFIG_KEY_0"], "http.https://github.com/.extraheader")
        self.assertEqual(auth["GIT_CONFIG_VALUE_0"], "AUTHORIZATION: basic " + mirror.base64.b64encode(b"x-access-token:tok").decode())

    def test_a_token_is_minted_for_the_installation_and_its_repositories_and_kept_until_github_expires_it(self):
        minted = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                asked = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                minted.append((self.path, self.headers["Authorization"].split()[0], asked))
                body = json.dumps({"token": f"tok-{len(minted)}", "expires_at": "2026-10-19T00:00:00Z"}).encode()
                self.send_response(201)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server, url = serve(Handler)
        now = [mirror.datetime.fromisoformat("2026-10-18T23:00:00Z").timestamp()]
        app = mirror.App("1234", "99", app_key()[1], ["ducktape", "programs", "app"], api=url, clock=lambda: now[0])
        first = app.auth()
        self.assertEqual(app.auth(), first)
        now[0] = mirror.datetime.fromisoformat("2026-10-19T00:00:00Z").timestamp()
        self.assertEqual(app.auth(), mirror.git_auth("tok-2"))
        server.shutdown()
        server.server_close()
        self.assertEqual(first, mirror.git_auth("tok-1"))
        asked = {"repositories": ["ducktape", "programs", "app"]}
        self.assertEqual(minted, [("/app/installations/99/access_tokens", "Bearer", asked)] * 2)

    def test_a_refused_mint_stops_the_job_with_a_reason(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *args):
                pass

        server, url = serve(Handler)
        app = mirror.App("1234", "99", app_key()[1], ["ducktape"], api=url)
        with self.assertRaises(mirror.Stopped) as stopped:
            app.auth()
        server.shutdown()
        server.server_close()
        self.assertEqual(stopped.exception.reason, "token_refused")

    def test_the_app_reads_its_three_secret_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / "app_id").write_text("1234\n")
            (root / "installation_id").write_text("99\n")
            (root / "private_key").write_bytes(app_key()[1])
            app = mirror.App.from_dir(root, ("ducktape", "app"), "https://api")
        self.assertEqual((app.app_id, app.installation, app.repositories, app.api), ("1234", "99", ["ducktape", "app"], "https://api"))


class FakeNode:
    def __init__(self):
        self.values = {}
        self.followed = []
        self.down = False

    def get(self, program, key):
        if self.down:
            raise ConnectionRefusedError(111, "Connection refused")
        return self.values.get((program, key))

    def follow(self, program):
        self.followed.append(program)
        return iter([])


class FakeApp:
    def auth(self):
        return {}


class Sync(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.temp.name)
        self.work = root / "work"
        git("init", "--quiet", "-b", "dev", str(self.work))
        self.base = self.commit("base")
        self.forge = root / "forge" / "core.git"
        self.github = root / "github" / "ducktape-industries" / "ducktape.git"
        for bare in (self.forge, self.github):
            git("init", "--quiet", "--bare", str(bare))
            git("push", "--quiet", str(bare), f"{self.base}:refs/heads/dev", cwd=self.work)
        self.config = mirror.Config(
            node="http://node",
            chain="test",
            clock="scheduler",
            state=root / "state",
            app=root / "app",
            pairs=(mirror.Pair("core", "ducktape-industries/ducktape", "dev"),),
            repositories=("ducktape",),
            forge_url=f"file://{root}/forge/{{forge}}.git",
            github_url=f"file://{root}/github/{{github}}.git",
            api="http://api",
        )
        self.pair = self.config.pairs[0]
        self.node = FakeNode()
        self.on_forge(self.base)
        self.mirror = mirror.Follower(self.config, self.node, FakeApp())

    def tearDown(self):
        self.temp.cleanup()

    def commit(self, name, parent=None):
        if parent is not None:
            git("checkout", "--quiet", "--detach", parent, cwd=self.work)
        (self.work / name).write_text(name)
        git("add", name, cwd=self.work)
        git("commit", "--quiet", "-m", name, cwd=self.work)
        return git("rev-parse", "HEAD", cwd=self.work)

    def put(self, bare, oid, branch="dev"):
        git("push", "--quiet", "--force", str(bare), f"{oid}:refs/heads/{branch}", cwd=self.work)

    def head(self, bare, branch="dev"):
        return git("--git-dir", str(bare), "rev-parse", f"refs/heads/{branch}")

    def on_forge(self, oid, pair=None):
        pair = pair or self.config.pairs[0]
        self.put(self.forge, oid, pair.branch)
        self.node.values[(mirror.FORGE, mirror.ref_key(pair))] = ref_value(oid)

    def change(self, program, height, writes):
        return mirror.Wrote(program, mirror.Change(height, writes))

    def tick(self, height):
        return self.change("scheduler", height, [(b"s/tick", b"x")])

    def refuse(self):
        hook = self.github / "hooks" / "pre-receive"
        hook.write_text("#!/bin/sh\necho protected >&2\nexit 1\n")
        hook.chmod(0o755)
        return hook

    def test_github_behind_is_fast_forwarded(self):
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.assertEqual(self.mirror.sync(self.pair, "test"), "pushed")
        self.assertEqual(self.head(self.github), ahead)

    def test_github_equal_is_left(self):
        self.assertEqual(self.mirror.sync(self.pair, "test"), "equal")

    def test_a_ref_forge_lacks_is_left(self):
        self.assertEqual(self.mirror.sync(self.pair, "test", None), "forge_absent")
        self.assertEqual(self.head(self.github), self.base)

    def test_a_branch_github_lacks_is_created(self):
        git("--git-dir", str(self.github), "update-ref", "-d", "refs/heads/dev")
        self.assertEqual(self.mirror.sync(self.pair, "test"), "pushed")
        self.assertEqual(self.head(self.github), self.base)

    def test_github_ahead_is_reset_to_forge_and_the_dropped_commits_logged(self):
        ahead = self.commit("github-only", self.base)
        self.put(self.github, ahead)
        with Said() as said:
            self.assertEqual(self.mirror.sync(self.pair, "test"), "reset")
        self.assertEqual(self.head(self.github), self.base)
        self.assertEqual(said.named("reset"), [{"forge": "core", "github": "ducktape-industries/ducktape", "branch": "dev", "old": ahead, "new": self.base, "dropped": [ahead]}])

    def test_a_divergence_resets_github_to_forge(self):
        github_one = self.commit("github-1", self.base)
        github_two = self.commit("github-2")
        self.put(self.github, github_two)
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        with Said() as said:
            self.assertEqual(self.mirror.sync(self.pair, "test"), "reset")
        self.assertEqual(self.head(self.github), forge)
        self.assertEqual(said.named("reset")[0]["dropped"], [github_two, github_one])

    def test_a_push_that_loses_a_race_runs_the_job_again_against_the_new_head(self):
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        raced = self.commit("github-raced", self.base)
        clone = self.mirror.mirror(self.pair)
        push = clone.push
        moves = [raced]

        def racing(branch, oid, lease, auth):
            if moves:
                self.put(self.github, moves.pop())
            return push(branch, oid, lease, auth)

        clone.push = racing
        with Said() as said:
            self.assertEqual(self.mirror.sync(self.pair, "test"), "reset")
        self.assertEqual([fields["outcome"] for fields in said.named("synced")], ["raced", "reset"])
        self.assertEqual(said.named("reset")[0]["dropped"], [raced])
        self.assertEqual(self.head(self.github), forge)

    def test_a_refused_push_is_tried_again_on_each_later_block_and_logged_on_attempt_one_and_every_tenth(self):
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        hook = self.refuse()
        with Said() as said:
            self.assertEqual(self.mirror.sync(self.pair, "test"), "failed")
            for height in range(2, 13):
                self.mirror.wake([self.tick(height)])
            self.assertEqual(self.mirror.attempts, {self.pair: 12})
            self.assertEqual(self.head(self.github), self.base)
            hook.unlink()
            self.mirror.wake([self.tick(13)])
        failures = said.named("push_failed")
        self.assertEqual([fields["attempts"] for fields in failures], [1, 10])
        self.assertEqual({fields["reason"] for fields in failures}, {"push_refused"})
        self.assertIn("protected", failures[0]["detail"])
        self.assertEqual(said.named("push_recovered"), [{"forge": "core", "github": "ducktape-industries/ducktape", "branch": "dev", "attempts": 12}])
        self.assertEqual(self.mirror.attempts, {})
        self.assertEqual(self.head(self.github), forge)

    def test_a_node_it_cannot_reach_is_a_failed_attempt_tried_again(self):
        self.node.down = True
        self.assertEqual(self.mirror.sync(self.pair, "test"), "failed")
        self.assertEqual(self.mirror.attempts, {self.pair: 1})
        self.node.down = False
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.mirror.wake([self.tick(5)])
        self.assertEqual(self.head(self.github), ahead)
        self.assertEqual(self.mirror.attempts, {})

    def test_a_block_with_nothing_pending_pushes_nothing(self):
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.mirror.wake([self.tick(5)])
        self.assertEqual(self.head(self.github), self.base)

    def test_a_forge_write_to_a_followed_ref_pushes_that_ref_at_the_written_head(self):
        ahead = self.commit("forge-only", self.base)
        self.put(self.forge, ahead)
        other = mirror.ref_key(mirror.Pair("core", "x", "feature"))
        self.mirror.wake([self.change(mirror.FORGE, 9, [(other, ref_value(self.base))])])
        self.assertEqual(self.head(self.github), self.base)
        self.mirror.wake([self.change(mirror.FORGE, 10, [(other, ref_value(self.base)), (mirror.ref_key(self.pair), ref_value(ahead))])])
        self.assertEqual(self.head(self.github), ahead)

    def test_a_pending_ref_is_tried_again_when_another_followed_ref_moves(self):
        second = mirror.Pair("core", "ducktape-industries/ducktape", "main")
        self.config = dataclasses.replace(self.config, pairs=(self.pair, second))
        self.mirror = mirror.Follower(self.config, self.node, FakeApp())
        for bare in (self.forge, self.github):
            self.put(bare, self.base, "main")
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        hook = self.refuse()
        self.mirror.sync(self.pair, "test")
        hook.unlink()
        main = self.commit("forge-main", self.base)
        self.put(self.forge, main, "main")
        self.mirror.wake([self.change(mirror.FORGE, 11, [(mirror.ref_key(second), ref_value(main))])])
        self.assertEqual(self.head(self.github), forge)
        self.assertEqual(self.head(self.github, "main"), main)

    def test_an_ended_forge_follow_follows_again_and_pushes_what_moved_meanwhile(self):
        self.mirror.sync(self.pair, "start")
        self.mirror.wake([mirror.Ended(mirror.FORGE)])
        self.assertEqual(self.node.followed, ["forge"])
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.mirror.wake([mirror.Ended(mirror.FORGE)])
        self.assertEqual(self.node.followed, ["forge", "forge"])
        self.assertEqual(self.head(self.github), ahead)

    def test_an_ended_clock_follow_follows_again_and_pushes_nothing_by_itself(self):
        self.mirror.sync(self.pair, "start")
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.mirror.wake([mirror.Ended("scheduler")])
        self.assertEqual(self.node.followed, ["scheduler"])
        self.assertEqual(self.head(self.github), self.base)


class ConfigFromEnv(unittest.TestCase):
    def test_pairs_are_every_repo_on_every_branch_and_the_token_asks_for_those_repositories(self):
        config = mirror.Config.from_env(
            {
                "FORGE_MIRROR_NODE": "https://duckpond.ducktape.industries",
                "FORGE_MIRROR_CHAIN": "duckpond-725bcad1",
                "FORGE_MIRROR_CLOCK": "scheduler",
                "FORGE_MIRROR_STATE": "/state",
                "FORGE_MIRROR_APP": "/keys/github-app",
                "FORGE_MIRROR_REPOS": "core=ducktape-industries/ducktape programs=ducktape-industries/programs",
                "FORGE_MIRROR_BRANCHES": "dev main",
            }
        )
        self.assertEqual(
            [(pair.forge, pair.github, pair.branch) for pair in config.pairs],
            [
                ("core", "ducktape-industries/ducktape", "dev"),
                ("core", "ducktape-industries/ducktape", "main"),
                ("programs", "ducktape-industries/programs", "dev"),
                ("programs", "ducktape-industries/programs", "main"),
            ],
        )
        self.assertEqual(config.repositories, ("ducktape", "programs"))
        self.assertEqual(config.forge_url.format(forge="core"), "duck://duckpond-725bcad1/forge/core")
        self.assertEqual(config.github_url.format(github="ducktape-industries/app"), "https://github.com/ducktape-industries/app.git")
        self.assertEqual(config.api, "https://api.github.com")
        self.assertEqual(config.app, pathlib.Path("/keys/github-app"))


if __name__ == "__main__":
    unittest.main()
