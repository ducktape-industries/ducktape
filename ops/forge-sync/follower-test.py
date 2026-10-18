"""Exercise the forge-sync follower against local git repositories standing in for forge and GitHub, a local websocket and HTTP node, and a recording ducktape; run by `make test`."""
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
spec = importlib.util.spec_from_file_location("follower", HERE / "follower.py")
follower = importlib.util.module_from_spec(spec)
spec.loader.exec_module(follower)
follower.say = lambda event, **fields: None

CORE_DEV_KEY = "722f636f72650000726566732f68656164732f6465760000"
CORE_DEV_VALUE = "14000000f782668e3842f577a623e4a70294f214f8c769ec"
POSTED = "040400000073796e6313000000666f7267652d73796e632d636f72652d64657601000000000100000019000000666f7267652077696e73206f6e20612f62206465763a20782e0000000000"
GIT_ENV = {
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@t",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@t",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}


def git(*args, cwd=None):
    ran = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, env=dict(os.environ, **GIT_ENV))
    if ran.returncode != 0:
        raise AssertionError(f"git {' '.join(args)}: {ran.stderr}")
    return ran.stdout.strip()


def ref_value(oid):
    return follower.borsh_bytes(bytes.fromhex(oid))


class Encoding(unittest.TestCase):
    def test_a_forge_ref_key_is_the_one_forge_writes(self):
        pair = follower.Pair("core", "ducktape-industries/ducktape", "dev")
        self.assertEqual(follower.ref_key(pair).hex(), CORE_DEV_KEY)

    def test_a_nul_in_a_key_part_is_escaped(self):
        self.assertEqual(follower.key_part(b"a\x00b"), b"a\x00\xffb\x00\x00")

    def test_the_channel_head_key_names_the_channel(self):
        self.assertEqual(follower.channel_head_key("sync"), b"head/sync\x00\x00")
        self.assertEqual(follower.message_key("m-1"), b"message-id/m-1\x00\x00")

    def test_a_ref_value_is_its_oid(self):
        self.assertEqual(follower.oid_of(bytes.fromhex(CORE_DEV_VALUE)), "f782668e3842f577a623e4a70294f214f8c769ec")
        self.assertIsNone(follower.oid_of(None))

    def test_a_post_is_chat_post_message_with_one_plain_paragraph(self):
        self.assertEqual(follower.post_message("sync", "forge-sync-core-dev", "forge wins on a/b dev: x.").hex(), POSTED)

    def test_a_change_frame_decodes_its_writes(self):
        frame = follower.u64(81452) + bytes(32) + follower.borsh_vec(
            [follower.borsh_bytes(b"k1") + b"\x01" + follower.borsh_bytes(b"v1"), follower.borsh_bytes(b"k2") + b"\x00"]
        )
        change = follower.decode_change(frame)
        self.assertEqual(change.height, 81452)
        self.assertEqual(change.writes, [(b"k1", b"v1"), (b"k2", None)])

    def test_relation_names_each_case(self):
        ancestry = {("b", "a"), ("c", "a")}
        contains = lambda descendant, ancestor: (descendant, ancestor) in ancestry
        cases = [
            (None, "a", "forge_absent"),
            ("a", None, "github_absent"),
            ("a", "a", "equal"),
            ("b", "a", "behind"),
            ("a", "b", "ahead"),
            ("b", "c", "diverged"),
        ]
        for forge, github, expected in cases:
            self.assertEqual(follower.relation(forge, github, contains), expected, (forge, github))


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
        change = follower.u64(7) + bytes(32) + follower.borsh_vec([follower.borsh_bytes(b"r/x") + b"\x00", follower.borsh_bytes(b"k" * 200) + b"\x01" + follower.borsh_bytes(b"v")])
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
        changes = list(follower.Changes(f"http://127.0.0.1:{port}", "forge"))
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
        with self.assertRaises(follower.Stopped):
            follower.Changes(f"http://127.0.0.1:{port}", "nothing")
        server.join()
        listener.close()

    def test_a_follow_the_peer_resets_ends_with_what_it_carried(self):
        change = follower.Change(9, [(b"r/x", b"v")])

        def reset():
            yield change
            raise ConnectionResetError(104, "Connection reset by peer")

        inbox = follower.queue.Queue()
        follower.pump("forge", reset(), inbox)
        self.assertEqual([inbox.get(), inbox.get()], [follower.Wrote("forge", change), follower.Ended("forge")])
        self.assertTrue(inbox.empty())


class NodeGet(unittest.TestCase):
    def test_get_posts_a_confirmed_get_and_reads_the_option(self):
        bodies = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                bodies.append((self.path, self.rfile.read(int(self.headers["Content-Length"]))))
                reply = b"\x01" + follower.borsh_bytes(b"value") if len(bodies) == 1 else b"\x00"
                self.send_response(200)
                self.send_header("Content-Length", str(len(reply)))
                self.end_headers()
                self.wfile.write(reply)

            def log_message(self, *args):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        node = follower.Node(f"http://127.0.0.1:{server.server_address[1]}/")
        self.assertEqual(node.get("forge", b"key"), b"value")
        self.assertIsNone(node.get("chat", b"other"))
        server.shutdown()
        server.server_close()
        self.assertEqual(bodies[0], ("/v1/get", b"\x00" + follower.borsh_text("forge") + follower.borsh_bytes(b"key")))


class Poster(unittest.TestCase):
    def poster(self, temp, said, code=0):
        script = pathlib.Path(temp) / "ducktape"
        script.write_text(f"#!/bin/sh\necho \"$@\" > {temp}/argv\ncat \"$5\" > {temp}/op\necho '{said}'\nexit {code}\n")
        script.chmod(0o755)
        keys = pathlib.Path(temp) / "keys"
        keys.mkdir()
        (keys / "key").write_text("sealed")
        (keys / "password").write_text("pw\n")
        return follower.Poster(str(script), "http://node", keys, temp)

    def test_an_applied_post_is_applied_and_sends_the_op(self):
        with tempfile.TemporaryDirectory() as temp:
            self.assertEqual(self.poster(temp, "preconfirmed applied ").post("sync", "m", "line"), "applied")
            argv = (pathlib.Path(temp) / "argv").read_text().split()
            self.assertEqual(argv[:4] + argv[5:], ["--node", "http://node", "submit", "chat", "--preconfirm"])
            self.assertEqual((pathlib.Path(temp) / "op").read_bytes(), follower.post_message("sync", "m", "line"))

    def test_a_post_already_made_is_exists(self):
        with tempfile.TemporaryDirectory() as temp:
            self.assertEqual(self.poster(temp, "preconfirmed rejected already_exists: message m exists").post("sync", "m", "line"), "exists")

    def test_any_other_outcome_stops(self):
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaises(follower.Stopped):
                self.poster(temp, "preconfirmed rejected unauthorized: no").post("sync", "m", "line")
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaises(follower.Stopped):
                self.poster(temp, "", code=1).post("sync", "m", "line")


class FakeNode:
    def __init__(self):
        self.values = {}
        self.followed = []

    def get(self, program, key):
        return self.values.get((program, key))

    def follow(self, program):
        self.followed.append(program)
        return iter([])


class Recorder:
    def __init__(self):
        self.posts = []

    def post(self, channel, message_id, line):
        self.posts.append((channel, message_id, line))
        return "applied"


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
        self.config = follower.Config(
            node="http://node",
            chain="test",
            channel="sync",
            state=root / "state",
            pairs=(follower.Pair("core", "ducktape-industries/ducktape", "dev"),),
            forge_url=f"file://{root}/forge/{{forge}}.git",
            github_url=f"file://{root}/github/{{github}}.git",
        )
        self.pair = self.config.pairs[0]
        self.node = FakeNode()
        self.poster = Recorder()
        self.follower = follower.Follower(self.config, self.node, self.poster)

    def tearDown(self):
        self.temp.cleanup()

    def commit(self, name, parent=None):
        if parent is not None:
            git("checkout", "--quiet", "--detach", parent, cwd=self.work)
        (self.work / name).write_text(name)
        git("add", name, cwd=self.work)
        git("commit", "--quiet", "-m", name, cwd=self.work)
        return git("rev-parse", "HEAD", cwd=self.work)

    def put(self, bare, oid, force=False):
        git("push", "--quiet", *(["--force"] if force else []), str(bare), f"{oid}:refs/heads/dev", cwd=self.work)

    def head(self, bare):
        return git("--git-dir", str(bare), "rev-parse", "refs/heads/dev")

    def on_forge(self, oid):
        self.put(self.forge, oid)
        self.node.values[(follower.FORGE, follower.ref_key(self.pair))] = ref_value(oid)

    def test_github_behind_is_fast_forwarded(self):
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.assertEqual(self.follower.sync(self.pair, ahead, "test"), "pushed")
        self.assertEqual(self.head(self.github), ahead)
        self.assertEqual(self.poster.posts, [])

    def test_github_equal_is_left(self):
        self.assertEqual(self.follower.sync(self.pair, self.base, "test"), "equal")

    def test_github_ahead_is_left_for_forge_sync_yml(self):
        ahead = self.commit("github-only", self.base)
        self.put(self.github, ahead)
        self.assertEqual(self.follower.sync(self.pair, self.base, "test"), "ahead")
        self.assertEqual(self.head(self.github), ahead)

    def test_a_ref_forge_lacks_is_left(self):
        self.assertEqual(self.follower.sync(self.pair, None, "test"), "forge_absent")
        self.assertEqual(self.head(self.github), self.base)

    def test_a_divergence_resets_github_to_forge_and_reports_the_dropped_commits(self):
        github_one = self.commit("github-1", self.base)
        github_two = self.commit("github-2")
        self.put(self.github, github_two)
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        self.assertEqual(self.follower.sync(self.pair, forge, "test"), "reset")
        self.assertEqual(self.head(self.github), forge)
        [(channel, message_id, line)] = self.poster.posts
        self.assertEqual(channel, "sync")
        self.assertTrue(message_id.startswith(f"forge-sync-core-dev-{github_two[:12]}-{forge[:12]}-"))
        self.assertIn(f"was {github_two} and is now {forge}", line)
        self.assertIn(f"Dropped commits: {github_two}, {github_one}.", line)
        self.assertEqual(self.follower.journal.held(), [])

    def test_a_push_that_loses_a_race_runs_the_job_again(self):
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        raced = self.commit("github-raced", self.base)
        mirror = self.follower.mirror(self.pair)
        push = mirror.push
        moves = [raced]

        def racing(branch, oid, lease=None):
            if moves:
                self.put(self.github, moves.pop())
            return push(branch, oid, lease)

        mirror.push = racing
        outcomes = []
        follower.say = lambda event, **fields: outcomes.append(fields["outcome"]) if event == "synced" else None
        try:
            self.assertEqual(self.follower.sync(self.pair, forge, "test"), "reset")
        finally:
            follower.say = lambda event, **fields: None
        self.assertEqual(outcomes, ["raced", "reset"])
        self.assertEqual(self.head(self.github), forge)
        self.assertIn(f"Dropped commits: {raced}.", self.poster.posts[0][2])

    def refuse(self):
        hook = self.github / "hooks" / "pre-receive"
        hook.write_text("#!/bin/sh\necho protected >&2\nexit 1\n")
        hook.chmod(0o755)
        return hook

    def test_a_refused_push_is_reported_once_and_tried_again_at_the_next_forge_push(self):
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        hook = self.refuse()
        self.assertEqual(self.follower.sync(self.pair, forge, "test"), "failed")
        self.assertEqual(self.head(self.github), self.base)
        self.assertEqual(self.follower.pending, {self.pair})
        [(channel, message_id, line)] = self.poster.posts
        self.assertEqual(message_id, f"forge-sync-failed-core-dev-{forge[:12]}")
        self.assertIn("protected", line)
        hook.unlink()
        other = follower.ref_key(follower.Pair("core", "x", "feature"))
        self.follower.wake([self.change(follower.FORGE, 9, [(other, ref_value(self.base))])])
        self.assertEqual(self.head(self.github), self.base)
        self.follower.wake([self.change(follower.CHAT, 10, [(follower.channel_head_key("sync"), b"x")])])
        self.assertEqual(self.head(self.github), forge)
        self.assertEqual(self.follower.pending, set())

    def test_a_pending_ref_is_tried_again_when_another_followed_ref_moves(self):
        second = follower.Pair("core", "ducktape-industries/ducktape", "main")
        self.config = dataclasses.replace(self.config, pairs=(self.pair, second))
        self.follower = follower.Follower(self.config, self.node, self.poster)
        for bare in (self.forge, self.github):
            git("push", "--quiet", str(bare), f"{self.base}:refs/heads/main", cwd=self.work)
        forge = self.commit("forge-1", self.base)
        self.on_forge(forge)
        hook = self.refuse()
        self.follower.sync(self.pair, forge, "test")
        hook.unlink()
        main = self.commit("forge-main", self.base)
        git("push", "--quiet", str(self.forge), f"{main}:refs/heads/main", cwd=self.work)
        self.follower.wake([self.change(follower.FORGE, 11, [(follower.ref_key(second), ref_value(main))])])
        self.assertEqual(self.head(self.github), forge)
        self.assertEqual(git("--git-dir", str(self.github), "rev-parse", "refs/heads/main"), main)

    def test_a_held_report_of_a_reset_that_happened_is_posted(self):
        github = self.commit("github-1", self.base)
        forge = self.commit("forge-1", self.base)
        self.put(self.github, forge)
        mirror = self.follower.mirror(self.pair)
        mirror.must("fetch", "--quiet", str(self.work), f"+{github}:refs/old")
        report = follower.Report("forge-sync-core-dev-x", self.pair, github, forge, [github])
        self.follower.journal.write(report)
        self.follower.settle_reports()
        self.assertEqual([post[1] for post in self.poster.posts], ["forge-sync-core-dev-x"])
        self.assertEqual(self.follower.journal.held(), [])

    def test_a_held_report_of_a_reset_that_did_not_happen_is_dropped(self):
        github = self.commit("github-1", self.base)
        forge = self.commit("forge-1", self.base)
        self.put(self.github, github)
        report = follower.Report("forge-sync-core-dev-y", self.pair, github, forge, [github])
        self.follower.journal.write(report)
        self.follower.settle_reports()
        self.assertEqual(self.poster.posts, [])
        self.assertEqual(self.follower.journal.held(), [])

    def test_a_held_report_already_posted_is_dropped(self):
        report = follower.Report("forge-sync-core-dev-z", self.pair, self.base, self.base, [])
        self.follower.journal.write(report)
        self.node.values[(follower.CHAT, follower.message_key("forge-sync-core-dev-z"))] = b"row"
        self.follower.settle_reports()
        self.assertEqual(self.poster.posts, [])
        self.assertEqual(self.follower.journal.held(), [])

    def change(self, program, height, writes):
        return follower.Wrote(program, follower.Change(height, writes))

    def test_a_forge_write_to_a_followed_ref_syncs_that_ref_to_the_written_head(self):
        ahead = self.commit("forge-only", self.base)
        self.put(self.forge, ahead)
        other = follower.ref_key(follower.Pair("core", "x", "feature"))
        self.follower.wake([self.change(follower.FORGE, 9, [(other, ref_value(self.base)), (follower.ref_key(self.pair), ref_value(ahead))])])
        self.assertEqual(self.head(self.github), ahead)

    def test_a_post_in_the_channel_syncs_every_ref_at_its_confirmed_head(self):
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.follower.wake([self.change(follower.CHAT, 9, [(b"head/general\x00\x00", b"x")])])
        self.assertEqual(self.head(self.github), self.base)
        self.follower.wake([self.change(follower.CHAT, 10, [(follower.channel_head_key("sync"), b"x")])])
        self.assertEqual(self.head(self.github), ahead)

    def test_an_ended_forge_follow_follows_again_and_syncs_what_moved_meanwhile(self):
        self.follower.sync(self.pair, self.base, "start")
        self.follower.wake([follower.Ended(follower.FORGE)])
        self.assertEqual(self.node.followed, ["forge"])
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.follower.wake([follower.Ended(follower.FORGE)])
        self.assertEqual(self.head(self.github), ahead)

    def test_an_ended_chat_follow_follows_again_and_syncs_all_when_the_channel_moved_meanwhile(self):
        ahead = self.commit("forge-only", self.base)
        self.on_forge(ahead)
        self.follower.seen[self.pair] = ahead
        self.follower.wake([follower.Ended(follower.CHAT)])
        self.assertEqual(self.head(self.github), self.base)
        self.node.values[(follower.CHAT, follower.channel_head_key("sync"))] = b"seq 2"
        self.follower.wake([follower.Ended(follower.CHAT)])
        self.assertEqual(self.node.followed, ["chat", "chat"])
        self.assertEqual(self.head(self.github), ahead)


class ConfigFromEnv(unittest.TestCase):
    def test_pairs_are_every_repo_on_every_branch(self):
        config = follower.Config.from_env(
            {
                "FORGE_SYNC_NODE": "http://node",
                "FORGE_SYNC_CHAIN": "duckpond-725bcad1",
                "FORGE_SYNC_CHANNEL": "sync",
                "FORGE_SYNC_STATE": "/state",
                "FORGE_SYNC_REPOS": "core=ducktape-industries/ducktape app=ducktape-industries/app",
                "FORGE_SYNC_BRANCHES": "dev main",
            }
        )
        self.assertEqual(
            [(pair.forge, pair.github, pair.branch) for pair in config.pairs],
            [
                ("core", "ducktape-industries/ducktape", "dev"),
                ("core", "ducktape-industries/ducktape", "main"),
                ("app", "ducktape-industries/app", "dev"),
                ("app", "ducktape-industries/app", "main"),
            ],
        )
        self.assertEqual(config.forge_url.format(forge="core"), "duck://duckpond-725bcad1/forge/core")
        self.assertEqual(config.github_url.format(github="ducktape-industries/app"), "git@github.com:ducktape-industries/app.git")


if __name__ == "__main__":
    unittest.main()
