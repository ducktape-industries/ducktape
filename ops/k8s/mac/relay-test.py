"""Exercise the mac relay over real HTTP, on both its ports, against a recording ducktape CLI; run by `make test`."""
import http.client
import importlib.util
import socket
import struct
import subprocess
import json
import pathlib
import tempfile
import threading
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("relay", HERE / "relay.py")
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)
relay.say = lambda event, **fields: None

ASKED = {"ducktape-chain": "duckpond", "ducktape-program": "harness", "ducktape-request": "41"}
TOKEN = "s3cret"
COMMIT = "c0ffee" + "0" * 34
HEAD = "sha256:" + "ab" * 32
SNAPSHOT = "cd" * 32
TO = "/shared/jobs/J-089/runs"
SCRIPT = {"steps": [{"launch": {}}, {"screenshot": "home"}]}
BUILD = {"repo": "app", "commit": COMMIT, "build": "ios-sim", "to": TO}


def ran(out=b"", code=0, err=""):
    return relay.Ran(code, out, err)


def contents(data, snapshot=SNAPSHOT):
    return relay.u8(5) + relay.u8(0) + bytes.fromhex(snapshot) + relay.u64(len(data)) + relay.u64(0) + relay.borsh_bytes(data) + relay.u8(1)


def read_path(request):
    reader = relay.Reader(request)
    reader.u8()
    path = reader.bytes().decode()
    snapshot = reader.blob() if reader.u8() == 1 else None
    return path, snapshot


class Signer:
    def __init__(self):
        self.keyed = True
        self.files = {}
        self.puts = []
        self.submits = []
        self.queries = []
        self.answers = []

    def held(self):
        return self.keyed

    def put(self, paths, kind):
        self.puts.append((kind, [path.read_bytes() for path in paths]))
        blobs = "".join(f"sha256:{index:064x}\n" for index in range(len(paths)))
        return ran(f"{blobs}bundle 7 put at block 9\n".encode())

    def submit(self, program, payload):
        self.submits.append((program, payload))
        return self.answers.pop(0) if self.answers else ran(b"preconfirmed applied 0a0b")

    def query(self, program, request):
        self.queries.append((program, request))
        if request[0] == 0:
            return ran(relay.u8(0) + relay.u8(1) + relay.blob_id(HEAD))
        path, snapshot = read_path(request)
        if path not in self.files:
            return ran(code=1, err=f"not_found: {path}")
        return ran(contents(self.files[path]))


class Relay(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.temp.name)
        (root / "token").write_text(TOKEN + "\n")
        self.signer = Signer()
        self.store = relay.Store(root / "state")
        self.forge = relay.Forge(root / "state" / "repos", f"file://{root}/forge", "https://node.example")
        self.service = self.serve(relay.Service(self.signer, self.store, root / "token", self.forge, clock=lambda: "2026-10-19T12:00:00Z"))

    def serve(self, service):
        service.spawn = lambda job: job()
        self.servers = [relay.listening("127.0.0.1:0", answer) for answer in (service.agents, service.macs)]
        for server in self.servers:
            threading.Thread(target=server.serve_forever, daemon=True).start()
        return service

    def tearDown(self):
        for server in self.servers:
            server.shutdown()
            server.server_close()
        self.temp.cleanup()

    def call(self, side, method, target, body=None, headers=None):
        connection = http.client.HTTPConnection(*self.servers[side].server_address)
        payload = body if isinstance(body, bytes) or body is None else json.dumps(body).encode()
        connection.request(method, target, body=payload, headers=headers or {})
        answer = connection.getresponse()
        data = answer.read()
        connection.close()
        kind = answer.getheader("content-type")
        return answer.status, json.loads(data) if kind == "application/json" else data

    def agent(self, method, target, body=None, request=41):
        headers = {**ASKED, "ducktape-request": str(request)} if method == "POST" else {}
        return self.call(0, method, target, body, headers)

    def mac(self, method, target, body=None, token=TOKEN):
        return self.call(1, method, target, body, {"authorization": f"Bearer {token}"})

    def work(self, runs=()):
        connection = http.client.HTTPConnection(*self.servers[1].server_address)
        hello = json.dumps({"facts": {"xcode": "26.0"}, "runs": list(runs)}).encode()
        connection.request("POST", "/v1/mac/work", body=hello, headers={"authorization": f"Bearer {TOKEN}"})
        answer = connection.getresponse()
        self.assertEqual(answer.status, 200)
        return connection, answer

    def event(self, answer):
        lines = []
        while (line := answer.readline().decode()) != "\n":
            self.assertNotEqual(line, "", "the stream ended")
            lines.append(line.rstrip("\n"))
        kind = lines[0].removeprefix("event: ")
        return kind, json.loads(lines[1].removeprefix("data: "))

    def until(self, holds):
        with self.service.changed:
            self.service.changed.wait_for(holds)

    def test_a_run_is_queued_while_the_mac_is_away_and_a_repeat_answers_it_again(self):
        status, body = self.agent("POST", "/v1/runs", {**BUILD, "script": SCRIPT, "requester": 21})
        self.assertEqual(status, 201)
        run = body["run"]
        self.assertEqual((run["id"], run["state"], run["steps"], run["requester"]), ("duckpond-41", "queued", 2, 21))
        self.assertFalse(body["mac"]["connected"])
        self.assertIn("asleep or unreachable since 2026-10-19T12:00:00Z", body["mac"]["sentence"])
        again = self.agent("POST", "/v1/runs", {**BUILD, "build": "ios-archive"})
        self.assertEqual((again[0], again[1]["run"]["build"]), (200, "ios-sim"))
        self.assertEqual([run["id"] for run in self.agent("GET", "/v1/runs")[1]["runs"]], ["duckpond-41"])

    def test_a_malformed_run_is_refused_in_words(self):
        cases = [
            ({**BUILD, "from": "duckpond-1"}, "not both"),
            ({"repo": "app", "to": TO}, "it lacks commit, build"),
            ({**BUILD, "commit": "c0ffee"}, "whole commit id"),
            ({**BUILD, "to": "runs"}, "absolute path"),
            ({k: v for k, v in BUILD.items() if k != "to"}, "names its to"),
            ({**BUILD, "vars": {"A B": "1"}}, "not a make variable"),
            ({**BUILD, "device": {"fresh": "yes"}}, "fresh is a JSON boolean"),
            ({**BUILD, "requester": True}, "requester is a JSON number"),
            ({**BUILD, "script": {"steps": [{"tap": {}}]}}, "step 1 is tap, not a step"),
            ({**BUILD, "script": {"steps": [{"screenshot": "../x"}]}}, "names its output"),
            ({**BUILD, "script": {"steps": [{"system": {"tap": "Photo"}}]}}, "names its deadline_ms"),
            ({**BUILD, "script": {"steps": [{"changed": {"name": "sheet"}}]}}, "names its deadline_ms"),
            ({**BUILD, "script": {"steps": [{"changed": {"name": "../x", "deadline_ms": 1}}]}}, "names its output"),
            ({**BUILD, "script": {"steps": [], "files": {"path": "/s.json"}}}, "one of them"),
            ({"from": "duckpond-9", "to": TO}, "there is no run duckpond-9"),
        ]
        for body, words in cases:
            status, answer = self.agent("POST", "/v1/runs", body)
            self.assertIn(status, (400, 404), body)
            self.assertIn(words, answer["error"])
        self.assertEqual(self.agent("GET", "/v1/runs")[1]["runs"], [])

    def test_a_relay_without_its_key_takes_no_run(self):
        self.signer.keyed = False
        status, body = self.agent("POST", "/v1/runs", BUILD)
        self.assertEqual((status, body["error"]), (503, "this service holds no key to read a run's inputs and write its outputs with"))

    def test_a_script_and_its_media_are_read_from_files_when_the_run_is_asked(self):
        self.signer.files["/shared/s.json"] = json.dumps({"steps": [{"media": {"files": {"path": "/shared/cat.png"}}}, {"launch": {}}]}).encode()
        self.signer.files["/shared/cat.png"] = b"\x89PNG cat"
        status, body = self.agent("POST", "/v1/runs", {**BUILD, "script": {"files": {"path": "/shared/s.json", "snapshot": SNAPSHOT}}})
        self.assertEqual((status, body["run"]["steps"]), (201, 2))
        self.assertEqual(read_path(self.signer.queries[0][1]), ("/shared/s.json", "sha256:" + SNAPSHOT))
        connection, answer = self.work()
        kind, handed = self.event(answer)
        self.assertEqual(kind, "run")
        self.assertEqual(handed["script"], [{"media": {"input": 0, "name": "cat.png"}}, {"launch": {}}])
        self.assertEqual(handed["inputs"], ["cat.png"])
        self.assertEqual(self.mac("GET", "/v1/mac/runs/duckpond-41/inputs/0"), (200, b"\x89PNG cat"))
        self.assertEqual(self.mac("GET", "/v1/mac/runs/duckpond-41/inputs/1")[0], 404)
        connection.close()

    def test_a_script_files_cannot_read_is_refused_with_files_words(self):
        status, body = self.agent("POST", "/v1/runs", {**BUILD, "script": {"files": {"path": "/nope.json"}}})
        self.assertEqual((status, body["error"]), (422, "files did not read /nope.json: not_found: /nope.json"))

    def test_the_mac_pulls_runs_reports_them_and_its_outputs_land_in_one_commit(self):
        self.agent("POST", "/v1/runs", {**BUILD, "script": SCRIPT, "requester": 21, "notify": {"channel": "j-089"}})
        connection, answer = self.work()
        self.assertEqual(self.event(answer)[0], "run")
        self.assertTrue(self.agent("GET", "/v1/mac")[1]["mac"]["connected"])
        self.assertEqual(self.agent("GET", "/v1/mac")[1]["mac"]["facts"], {"xcode": "26.0"})
        self.agent("POST", "/v1/runs", {**BUILD, "to": TO}, request=42)
        kind, handed = self.event(answer)
        self.assertEqual((kind, handed["run"]["id"], handed["script"]), ("run", "duckpond-42", None))
        status, body = self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "testing", "step": "step 2/2: screenshot home"})
        self.assertEqual((status, body["run"]["state"], body["run"]["step"]), (200, "testing", "step 2/2: screenshot home"))
        self.assertEqual(self.mac("PUT", "/v1/mac/runs/duckpond-41/outputs/02-home.png", b"png" * 10)[0], 200)
        self.assertEqual(self.mac("PUT", "/v1/mac/runs/duckpond-41/outputs/crashes/Ducktape-1.ips", b"ips")[0], 200)
        self.assertEqual(self.mac("PUT", "/v1/mac/runs/duckpond-41/outputs/..%2Fx", b"x")[0], 400)
        status, body = self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "succeeded"})
        run = body["run"]
        base = f"{TO}/duckpond-41"
        self.assertEqual(run["state"], "succeeded")
        self.assertEqual(run["outputs"], [f"{base}/02-home.png", f"{base}/crashes/Ducktape-1.ips", f"{base}/run.json"])
        kind, pieces = self.signer.puts[0]
        self.assertEqual((kind, pieces[:2]), ("chunk", [b"png" * 10, b"ips"]))
        self.assertEqual(json.loads(pieces[2])["state"], "succeeded")
        commit, post = self.signer.submits
        self.assertEqual(commit[0], "files")
        for path in run["outputs"]:
            self.assertIn(path.encode(), commit[1])
        self.assertEqual(post[0], "chat")
        self.assertIn(b"mac run duckpond-41 succeeded: 3 outputs in " + base.encode(), post[1])
        self.assertIn(b"<@21>", post[1])
        status, waited = self.agent("GET", "/v1/runs/duckpond-41/wait?every=5")
        self.assertIn(b"event: ended", waited)
        self.assertEqual(self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "failed"})[0], 409)
        connection.close()

    def test_a_commit_files_moved_under_is_made_again(self):
        self.agent("POST", "/v1/runs", BUILD)
        self.signer.answers = [ran(b"preconfirmed rejected stale: the base moved")]
        self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "failed", "reason": "make exited 2"})
        status, body = self.agent("GET", "/v1/runs/duckpond-41")
        self.assertEqual(len(self.signer.submits), 2)
        self.assertEqual((body["run"]["state"], body["run"]["reason"]), ("failed", "make exited 2"))
        self.assertEqual(body["run"]["outputs"], [f"{TO}/duckpond-41/run.json"])

    def test_outputs_files_did_not_take_are_written_again_by_the_next_call_about_the_run(self):
        self.agent("POST", "/v1/runs", {**BUILD, "notify": {"channel": "j-089"}})
        self.signer.answers = [ran(code=1, err="error sending request: connection reset by peer")]
        status, body = self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "succeeded"})
        run = body["run"]
        self.assertEqual((status, run["state"]), (200, "writing"))
        self.assertIn("not written yet (the write was not sent: error sending request", run["step"])
        self.assertEqual([program for program, _ in self.signer.submits], ["files"])
        run = self.agent("GET", "/v1/runs/duckpond-41")[1]["run"]
        self.assertEqual((run["state"], run["outputs"]), ("succeeded", [f"{TO}/duckpond-41/run.json"]))
        self.assertEqual([program for program, _ in self.signer.submits], ["files", "files", "chat"])

    def test_a_read_files_does_not_answer_is_refused_once_with_its_reason(self):
        self.signer.files = {}
        before = len(self.signer.queries)
        status, body = self.agent("POST", "/v1/runs", {**BUILD, "script": {"files": {"path": "/flaky.json"}}})
        self.assertEqual(status, 422)
        self.assertEqual(len(self.signer.queries) - before, 1)
        self.assertEqual(self.agent("GET", "/v1/runs")[1]["runs"], [])

    def test_a_forge_step_that_fails_answers_once_with_its_reason(self):
        calls = []

        def runner(argv, env=None):
            calls.append(argv)
            return ran(code=128, err="fatal: error sending request: connection reset by peer")

        self.service.forge = relay.Forge(pathlib.Path(self.temp.name) / "repos", "duck://duckpond/forge", "https://node.example", runner=runner)
        status, body = self.mac("POST", "/v1/mac/source", {"repo": "app", "commit": COMMIT})
        self.assertEqual(status, 502)
        self.assertEqual(body["error"], "app was not cloned from forge: fatal: error sending request: connection reset by peer")
        self.assertEqual(len(calls), 1)
        self.assertIn("duck.duckpond.node=https://node.example", calls[0])

    def test_a_refused_commit_leaves_the_reason_and_no_outputs(self):
        self.agent("POST", "/v1/runs", BUILD)
        self.signer.answers = [ran(b"preconfirmed rejected unauthorized: account 30 may not write /shared/jobs")]
        self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "succeeded"})
        run = self.agent("GET", "/v1/runs/duckpond-41")[1]["run"]
        self.assertEqual(run["outputs"], [])
        self.assertEqual(run["reason"], "its outputs were not written: files refused the write: unauthorized: account 30 may not write /shared/jobs")

    def test_the_mac_leaving_is_heard_at_once(self):
        connection, answer = self.work()
        self.assertTrue(self.agent("GET", "/v1/mac")[1]["mac"]["connected"])
        answer.close()
        connection.close()
        self.until(lambda: not self.service.mac["connected"])
        connection, answer = self.work()
        status, body = self.mac("POST", "/v1/mac/bye")
        self.assertEqual((status, body["mac"]["connected"]), (200, False))
        self.assertEqual(answer.read(), b"")
        connection.close()

    def test_the_work_stream_is_kept_alive_and_a_dead_socket_is_heard_as_the_mac_gone(self):
        kept, started = [], threading.Event()
        real = self.service.keep

        def keep(sock):
            real(sock)
            kept.append(sock)
            started.set()

        self.service.keep = keep
        raw = socket.create_connection(self.servers[1].server_address)
        head = f"POST /v1/mac/work HTTP/1.1\r\nhost: relay\r\nauthorization: Bearer {TOKEN}\r\ncontent-length: 2\r\n\r\n{{}}"
        raw.sendall(head.encode())
        started.wait()
        self.assertTrue(self.agent("GET", "/v1/mac")[1]["mac"]["connected"])
        options = [(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1), (socket.IPPROTO_TCP, socket.TCP_KEEPIDLE, 10)]
        options += [(socket.IPPROTO_TCP, socket.TCP_KEEPINTVL, 5), (socket.IPPROTO_TCP, socket.TCP_KEEPCNT, 3)]
        options += [(socket.IPPROTO_TCP, socket.TCP_USER_TIMEOUT, 25000)]
        for level, option, value in options:
            self.assertEqual(kept[0].getsockopt(level, option), value)
        raw.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        raw.close()
        self.until(lambda: not self.service.mac["connected"])

    def test_a_reconnecting_mac_is_told_what_it_lost_and_what_waits(self):
        self.agent("POST", "/v1/runs", {**BUILD, "notify": {"channel": "j-089"}}, request=41)
        self.agent("POST", "/v1/runs", BUILD, request=42)
        self.agent("POST", "/v1/runs", BUILD, request=43)
        self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "building"})
        self.mac("POST", "/v1/mac/runs/duckpond-43/state", {"state": "testing"})
        connection, answer = self.work(runs=["duckpond-43", "duckpond-9"])
        self.assertEqual(self.event(answer), ("forget", {"id": "duckpond-9"}))
        kind, handed = self.event(answer)
        self.assertEqual((kind, handed["run"]["id"]), ("run", "duckpond-42"))
        runs = {run["id"]: run for run in self.agent("GET", "/v1/runs")[1]["runs"]}
        self.assertEqual(runs["duckpond-41"]["state"], "interrupted")
        self.assertEqual(runs["duckpond-41"]["reason"], "the Mac no longer held this run when it connected again")
        self.assertEqual(runs["duckpond-43"]["state"], "testing")
        self.assertIn(b"mac run duckpond-41 interrupted", self.signer.submits[-1][1])
        connection.close()

    def test_deleting_a_run_tells_the_mac_to_forget_it(self):
        self.agent("POST", "/v1/runs", BUILD)
        connection, answer = self.work()
        self.event(answer)
        self.assertEqual(self.agent("DELETE", "/v1/runs/duckpond-41")[1]["deleted"], "duckpond-41")
        self.assertEqual(self.event(answer), ("forget", {"id": "duckpond-41"}))
        self.assertEqual(self.agent("GET", "/v1/runs/duckpond-41")[0], 404)
        self.assertEqual(self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "building"})[0], 404)
        self.assertFalse(self.store.run_dir("duckpond-41").exists())
        connection.close()

    def test_a_wait_beats_until_the_run_ends(self):
        self.agent("POST", "/v1/runs", BUILD)
        connection = http.client.HTTPConnection(*self.servers[0].server_address)
        connection.request("GET", "/v1/runs/duckpond-41/wait?every=1")
        answer = connection.getresponse()
        self.assertEqual(self.event(answer), ("alive", {}))
        self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "stopped", "reason": "deleted on the Mac"})
        while (event := self.event(answer))[0] == "alive":
            pass
        self.assertEqual(event[1]["line"], f"mac run duckpond-41 stopped: deleted on the Mac; 1 outputs in {TO}/duckpond-41/")
        connection.close()
        self.assertEqual(self.agent("GET", "/v1/runs/duckpond-9/wait?every=1")[0], 404)
        self.assertIn("every", self.agent("GET", "/v1/runs/duckpond-41/wait")[1]["error"])

    def test_the_macs_routes_need_its_token(self):
        self.assertEqual(self.mac("POST", "/v1/mac/bye", token="wrong")[0], 401)
        (pathlib.Path(self.temp.name) / "token").unlink()
        self.assertEqual(self.mac("POST", "/v1/mac/bye")[0], 503)
        self.assertEqual(self.agent("GET", "/v1/mac")[0], 200)

    def test_runs_outlive_a_restart_and_an_unwritten_end_is_written(self):
        self.agent("POST", "/v1/runs", BUILD)
        with self.service.changed:
            record = self.service.runs["duckpond-41"]
            record["state"], record["ending"] = "writing", {"state": "failed", "reason": "relay died"}
            self.service.saved()
        for server in self.servers:
            server.shutdown()
            server.server_close()
        again = self.serve(relay.Service(self.signer, self.store, pathlib.Path(self.temp.name) / "token", self.forge))
        again.resume()
        status, body = self.agent("GET", "/v1/runs/duckpond-41")
        self.assertEqual((body["run"]["state"], body["run"]["reason"]), ("failed", "relay died"))
        self.assertFalse(body["mac"]["connected"])

    def test_a_relay_with_no_labels_reads_no_actions_and_keeps_the_runs_shapes(self):
        self.agent("POST", "/v1/runs", BUILD)
        connection, answer = self.work()
        kind, handed = self.event(answer)
        self.assertEqual(sorted(handed), ["inputs", "run", "script"])
        self.assertEqual(sorted(handed["run"]), sorted(relay.VIEWED))
        self.mac("PUT", "/v1/mac/runs/duckpond-41/outputs/build.log", b"made")
        status, body = self.mac("POST", "/v1/mac/runs/duckpond-41/state", {"state": "succeeded"})
        self.assertEqual(sorted(body["run"]), sorted(relay.VIEWED))
        self.assertNotIn("exit", self.store.load()["duckpond-41"])
        written = [piece for kind, pieces in self.signer.puts for piece in pieces if b'"state": "succeeded"' in piece]
        self.assertEqual(sorted(json.loads(written[0])), sorted(relay.VIEWED))
        self.assertEqual([program for program, _ in self.signer.queries + self.signer.submits if program == relay.ACTIONS], [])
        connection.close()

    def test_the_mac_is_sent_a_commit_as_a_bundle_of_what_it_lacks(self):
        root = pathlib.Path(self.temp.name)
        work = root / "work"
        git = lambda *args, at=work: subprocess.run(["git", "-C", str(at), *args], check=True, capture_output=True).stdout.decode().strip()
        work.mkdir()
        git("init", "--quiet", "--initial-branch=dev")
        commits = []
        for number in (1, 2):
            (work / "file").write_text(f"{number}\n")
            git("add", "file")
            git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", f"c{number}")
            commits.append(git("rev-parse", "HEAD"))
        (root / "forge").mkdir()
        subprocess.run(["git", "clone", "--quiet", "--bare", str(work), str(root / "forge" / "app")], check=True)
        mac = root / "mac.git"
        subprocess.run(["git", "init", "--quiet", "--bare", str(mac)], check=True)
        for commit, have in ((commits[0], []), (commits[1], [commits[0], "ee" * 20])):
            status, bundle = self.mac("POST", "/v1/mac/source", {"repo": "app", "commit": commit, "have": have})
            self.assertEqual(status, 200)
            (root / "b").write_bytes(bundle)
            git("fetch", "--quiet", str(root / "b"), f"refs/mac/{commit}:refs/mac/{commit}", at=mac)
            self.assertEqual(git("rev-parse", f"refs/mac/{commit}", at=mac), commit)
        self.assertIn("c1", git("log", "--format=%s", commits[1], at=mac))
        status, body = self.mac("POST", "/v1/mac/source", {"repo": "app", "commit": "ab" * 20})
        self.assertEqual(status, 404)
        self.assertIn("forge's app has no commit", body["error"])


def text(value):
    return relay.borsh_text(value)


def check_row(check, runs_on=("mac",), steps=("make app",), state="queued"):
    job = text("macos") + relay.borsh_vec([text(label) for label in runs_on]) + text("") + relay.borsh_vec([text(step) for step in steps])
    job += relay.borsh_vec([text("MODE") + text("dist")]) + relay.borsh_vec([])
    row = relay.u64(check) + relay.u64(1) + text("app") + text(COMMIT) + text("release.toml") + relay.u32(1) + job
    row += relay.u8(relay.CHECK_STATES.index(state)) + b"\x00\x00" + relay.u64(1) + b"\x00\x00" + text("") + b"\x00"
    return row


class ActionsSigner(Signer):
    def __init__(self):
        super().__init__()
        self.queue = []
        self.states = {}
        self.taken = set()
        self.needs = {}
        self.outputs = {}
        self.blobs = {}
        self.follows = []

    def query(self, program, request):
        if program != relay.ACTIONS:
            return super().query(program, request)
        self.queries.append((program, request))
        tag, check = request[0], int.from_bytes(request[1:9], "little")
        match tag:
            case 2:
                rows = [check_row(id, **fields) for id, fields in self.queue if id not in self.taken]
                return ran(relay.u8(2) + relay.u64(1) + relay.borsh_vec(rows) + b"\x00")
            case 0:
                state = self.states.get(check, "running")
                return ran(relay.u8(0) + b"\x01" + check_row(check, state=state))
            case 6:
                needs = [text(job) + relay.borsh_option(met, relay.u64) for job, met in self.needs.get(check, [])]
                return ran(relay.u8(6) + relay.borsh_vec(needs))
            case 7:
                files = self.outputs.get(check)
                if files is None:
                    return ran(relay.u8(7) + b"\x00")
                listed = [text(path) + relay.blob_id(blob) + relay.u64(size) for path, blob, size in files]
                return ran(relay.u8(7) + b"\x01" + relay.u64(1) + relay.borsh_vec(listed))
        raise AssertionError(f"an actions query this fake does not answer: {request.hex()}")

    def submit(self, program, payload):
        if program == relay.ACTIONS and payload[0] == 3:
            check = int.from_bytes(payload[1:9], "little")
            self.submits.append((program, payload))
            if check in self.taken:
                return ran(b"preconfirmed rejected wrong_state: check is running, not queued")
            self.taken.add(check)
            return ran(b"preconfirmed applied 0a")
        return super().submit(program, payload)

    def blob(self, blob):
        return ran(b"blob 3\0" + self.blobs[blob])

    def follow(self, program):
        return Followed(self.follows.pop(0) if self.follows else [])


class Followed:
    def __init__(self, lines):
        self.lines = lines
        self.why = "ducktape: IO error: the node is not reachable"

    def __iter__(self):
        return iter(self.lines)


class Stopped(Exception):
    pass


class Checks(unittest.TestCase):
    serve = Relay.serve
    tearDown = Relay.tearDown
    call = Relay.call
    mac = Relay.mac
    agent = Relay.agent
    work = Relay.work
    event = Relay.event

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.temp.name)
        (root / "token").write_text(TOKEN + "\n")
        self.signer = ActionsSigner()
        self.store = relay.Store(root / "state")
        forge = relay.Forge(root / "state" / "repos", f"file://{root}/forge", "https://node.example")
        clock = lambda: "2026-10-19T12:00:00Z"
        self.service = self.serve(relay.Service(self.signer, self.store, root / "token", forge, clock=clock, labels=("mac", "macos-aarch64")))
        self.service.follow = lambda: None

    def actions(self, tag):
        return [payload for program, payload in self.signer.submits if program == relay.ACTIONS and payload[0] == tag]

    def test_an_idle_mac_claims_the_oldest_check_it_covers_and_its_end_is_reported_with_its_outputs(self):
        self.signer.queue = [(5, {"runs_on": ("linux",)}), (3, {}), (4, {})]
        self.service.pull()
        self.assertEqual(self.signer.submits, [], "nothing is claimed while the Mac is away")
        connection, answer = self.work()
        kind, handed = self.event(answer)
        self.assertEqual((kind, handed["run"]["id"], handed["run"]["check"]), ("run", "check-3", 3))
        self.assertEqual(handed["job"], {"steps": ["make app"], "env": {"MODE": "dist"}})
        self.assertEqual(self.mac("PUT", "/v1/mac/runs/check-3/outputs/log", b"$ make app\n")[0], 200)
        self.assertEqual(self.mac("PUT", "/v1/mac/runs/check-3/outputs/dist/app.tar.gz", b"tar")[0], 200)
        self.mac("POST", "/v1/mac/runs/check-3/state", {"state": "succeeded"})
        log = f"/shared/actions/app/{COMMIT}/release/macos-1/log"
        files = [payload for program, payload in self.signer.submits if program == "files"]
        self.assertEqual(len(files), 1)
        self.assertIn(log.encode(), files[0])
        self.assertIn(("output", [b"tar"]), self.signer.puts)
        [outputs] = self.actions(7)
        self.assertEqual(outputs, relay.actions_outputs(3, [("dist/app.tar.gz", f"sha256:{0:064x}", 3)]))
        [report] = self.actions(4)
        self.assertEqual(report, relay.actions_report(3, "passed", "every step passed", log))
        kind, handed = self.event(answer)
        self.assertEqual((kind, handed["run"]["id"]), ("run", "check-4"), "the next check once the Mac is idle")
        self.assertEqual([int.from_bytes(claim[1:9], "little") for claim in self.actions(3)], [3, 4])
        connection.close()

    def test_a_step_that_fails_fails_the_check_and_a_broken_run_errs_it(self):
        self.signer.queue = [(3, {}), (4, {})]
        connection, answer = self.work()
        self.event(answer)
        self.mac("POST", "/v1/mac/runs/check-3/state", {"state": "failed", "reason": "step 1 (make app) exited 2: no rule", "exit": 2})
        self.assertEqual(self.actions(4)[0], relay.actions_report(3, "failed", "step 1 (make app) exited 2: no rule", None))
        self.event(answer)
        self.mac("POST", "/v1/mac/runs/check-4/state", {"state": "failed", "reason": "the Mac's side failed: OSError()"})
        self.assertEqual(self.actions(4)[1], relay.actions_report(4, "errored", "the Mac's side failed: OSError()", None))
        connection.close()

    def test_what_a_check_needs_is_handed_to_the_mac_and_a_path_out_of_its_folder_errs_it(self):
        blob = "sha256:" + "ab" * 32
        self.signer.queue = [(3, {}), (4, {})]
        self.signer.needs = {3: [("build", 9)], 4: [("build", 10)]}
        self.signer.outputs = {9: [("dist/app.tar.gz", blob, 3)], 10: [("../../etc/passwd", blob, 3)]}
        self.signer.blobs = {blob: b"tar"}
        connection, answer = self.work()
        kind, handed = self.event(answer)
        self.assertEqual(handed["inputs"], ["build/dist/app.tar.gz"])
        status, data = self.mac("GET", "/v1/mac/runs/check-3/inputs/0")
        self.assertEqual((status, data), (200, b"tar"))
        self.mac("POST", "/v1/mac/runs/check-3/state", {"state": "succeeded"})
        errored = relay.actions_report(4, "errored", "check output '../../etc/passwd' of `build` would land outside its needs folder", None)
        self.assertIn(errored, self.actions(4))
        self.assertEqual(self.service.runs["check-4"]["state"], "failed", "a check that errs before it starts never reaches the Mac")
        connection.close()

    def test_a_check_that_ended_elsewhere_is_forgotten_on_the_mac_and_deleting_one_errs_it(self):
        self.signer.queue = [(3, {}), (4, {})]
        connection, answer = self.work()
        self.event(answer)
        self.signer.states[3] = "superseded"
        self.service.settle_checks()
        self.assertEqual(self.event(answer), ("forget", {"id": "check-3"}))
        self.assertNotIn("check-3", self.service.runs)
        self.assertEqual(self.actions(4), [], "a superseded check is not reported")
        self.service.pull()
        self.assertEqual(self.event(answer)[1]["run"]["id"], "check-4")
        self.mac("PUT", "/v1/mac/runs/check-4/outputs/log", b"$ make app\n")
        self.signer.answers = [ran(code=1, err="the node was not reached")]
        self.mac("POST", "/v1/mac/runs/check-4/state", {"state": "succeeded"})
        self.assertEqual(self.service.runs["check-4"]["state"], "writing", "its log did not send")
        self.agent("DELETE", "/v1/runs/check-4")
        self.assertEqual(self.event(answer), ("forget", {"id": "check-4"}))
        self.assertEqual(self.actions(4), [relay.actions_report(4, "errored", "its run was deleted on the mac relay", None)])
        self.assertNotIn("check-4", self.service.runs)
        connection.close()

    def test_deleting_the_run_that_held_the_mac_takes_the_next_check(self):
        self.signer.queue = [(3, {})]
        self.agent("POST", "/v1/runs", BUILD)
        connection, answer = self.work()
        self.assertEqual(self.event(answer)[1]["run"]["id"], "duckpond-41")
        self.assertEqual(self.actions(3), [], "the Mac holds an agent's run")
        self.agent("DELETE", "/v1/runs/duckpond-41")
        self.assertEqual(self.event(answer), ("forget", {"id": "duckpond-41"}))
        self.assertEqual(self.event(answer)[1]["run"]["id"], "check-3")
        connection.close()

    def test_a_report_that_was_not_sent_is_sent_again_and_holds_the_mac_until_it_is(self):
        blob = "sha256:" + "ab" * 32
        self.signer.queue = [(3, {}), (4, {})]
        self.signer.needs = {3: [("build", 10)]}
        self.signer.outputs = {10: [("../x", blob, 1)]}
        self.signer.answers = [ran(code=1, err="the node was not reached")]
        connection, answer = self.work()
        self.assertEqual(self.service.runs["check-3"]["state"], "writing")
        self.assertEqual(len(self.actions(4)), 1)
        self.assertEqual([int.from_bytes(claim[1:9], "little") for claim in self.actions(3)], [3], "a Mac with a report owed is not idle")
        self.service.pull()
        errored = relay.actions_report(3, "errored", "check output '../x' of `build` would land outside its needs folder", None)
        self.assertEqual(self.actions(4), [errored, errored])
        self.assertEqual(self.service.runs["check-3"]["state"], "failed")
        self.assertEqual(self.event(answer)[1]["run"]["id"], "check-4")
        connection.close()

    def test_the_follower_reads_actions_once_a_height_and_follows_again_after_its_pause(self):
        pulls, paused = [], []
        self.service.pull = lambda: pulls.append(1)
        self.service.settle_checks = lambda: None

        def pause(seconds):
            paused.append(seconds)
            raise Stopped()

        self.service.pause = pause
        self.signer.follows = [[b"5\tk1\tv\n", b"5\tk2\tv\n", b"6\tk1\tv\n"]]
        with self.assertRaises(Stopped):
            self.service.followed()
        self.assertEqual((len(pulls), paused), (2, [relay.FOLLOW_PAUSE]))
        self.assertFalse(self.service.following)

if __name__ == "__main__":
    unittest.main()
