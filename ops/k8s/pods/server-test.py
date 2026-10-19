"""Exercise the pods service over real HTTP against a recording kubectl, ducktape and kubelet, and its input fetcher against a recording runner; run by `make test`."""
import contextlib
import http.client
import socket
import importlib.util
import io
import json
import os
import pathlib
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import unittest
import unittest.mock

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("server", HERE / "server.py")
server = importlib.util.module_from_spec(spec)
spec.loader.exec_module(server)
server.say = lambda event, **fields: None

ASKED = {"ducktape-chain": "duckpond", "ducktape-program": "harness", "ducktape-request": "41"}
NOT_FOUND = 'Error from server (NotFound): pods "duckpond-9" not found'
TERMINATING = 'Error from server (Forbidden): error when creating "STDIN": pods "duckpond-41" is forbidden: unable to create new content in namespace agents because it is being terminated'
BASELINE = 'Error from server (Forbidden): error when creating "STDIN": pods "web-" is forbidden: violates PodSecurity "baseline:latest": non-default capabilities (container "web" must not include "NET_ADMIN" in securityContext.capabilities.add)'
SNAPSHOT = "sha256:" + "ab" * 32
CHUNKS = ["sha256:" + "01" * 32, "sha256:" + "02" * 32, "sha256:" + "03" * 32]
NOTIFY = {"ducktape.industries/notify-channel": "builds"}
ENDED = {"terminated": {"exitCode": 0, "reason": "Completed", "startedAt": "s", "finishedAt": "f"}}
RUNNING = {"running": {"startedAt": "s"}}
POSTED = bytes.fromhex(
    "04060000006275696c647316000000706f64732d6475636b706f6e642d34312d656e646564010000000002000000050000003c4031"
    "373e01000000030011000000000000002f00000020706f64206475636b706f6e642d343120656e6465643a206578697420636f6465"
    "20302028436f6d706c65746564290000000000"
)
KINDS = b"namespaces  ns  v1  false  Namespace\nclusterroles    rbac.authorization.k8s.io/v1  false  ClusterRole\n"
FETCHING = server.Fetching(image="localhost:5000/pods@sha256:00", node="https://node.example", forge="duck://duckpond/forge")


DROPPED = "error: unable to upgrade connection: read tcp: connection reset by peer"
MERGED = "0c46f11b649fcae450bf2f774eb766d5b33e107e"
LOST = "fatal: no node of duckpond answered: error sending request for url (https://node.example/v1/query)"
OPEN = bytes([3, 1]) + b"the request's row"
GONE = bytes([3, 0])
COMMIT = "c0ffee" + "0" * 34
HAVE = "be" * 20
UNSTARTED = 'error: unable to upgrade connection: container not found ("run")'
APISERVER = "Error from server (ServiceUnavailable): apiserver not ready"
WEBSOCKET = "error: error reading from error stream: websocket: close 1006 (abnormal closure): unexpected EOF"
UNPLACED = "error: unable to upgrade connection: pod duckpond-41 does not have a host assigned"
FETCHING_STATE = {"waiting": {"reason": "PodInitializing"}}
FAILED = {"terminated": {"exitCode": 128, "reason": "Error", "message": "clone failed"}}


def asked(request):
    return {**ASKED, "ducktape-request": str(request)}


def room(owner=99, members_only=False, archived=False):
    row = server.borsh_text("c") + server.borsh_text("c") + server.u64(1) + server.u8(1 if members_only else 0)
    row += server.u8(0) + server.u64(owner) + server.u8(1 if archived else 0)
    return server.u8(1) + server.u8(1) + row + server.u64(0) + server.borsh_vec([])


def members(accounts, after=None):
    rows = [server.u8(0) + server.u64(account) + server.u64(1) + server.u64(1) for account in accounts]
    return server.u8(7) + server.u64(1) + server.borsh_vec(rows) + server.borsh_option(after, server.borsh_bytes)


def taken(holder, field):
    value = getattr(holder, field)
    if isinstance(value, list):
        return value.pop(0) if len(value) > 1 else value[0]
    return value


def pack_of(objects):
    return b"PACK" + (2).to_bytes(4, "big") + objects.to_bytes(4, "big") + b"objects" * objects


POOLED = b"pooled " + b"ab" * 32 + b"\n"


def ran(out=b"", code=0, err=""):
    return server.Ran(code, out if isinstance(out, bytes) else json.dumps(out).encode(), err)


def claimed(name, labels):
    claim = {"accessModes": ["ReadWriteOnce"], "storageClassName": "local-path", "resources": {"requests": {"storage": "1"}}}
    return {"name": name, "ephemeral": {"volumeClaimTemplate": {"metadata": {"labels": labels}, "spec": claim}}}


def pod_json(name="duckpond-41", state=None, phase="Pending", notes=None, reason=None, requester="17", init=None):
    status = {"phase": phase}
    if reason is not None:
        status["reason"] = reason
    if state is not None:
        status["containerStatuses"] = [{"name": "out", "state": {"running": {}}}, {"name": "run", "state": state}]
    if init is not None:
        status["initContainerStatuses"] = [{"name": "fetch", "state": init}]
    labels = {"ducktape.industries/chain": "duckpond", "ducktape.industries/program": "harness", "ducktape.industries/request": "41"}
    if requester is not None:
        labels["ducktape.industries/requester"] = requester
    return {
        "metadata": {"name": name, "creationTimestamp": "2026-10-18T12:00:00Z", "labels": labels, "annotations": notes or {}},
        "spec": {
            "containers": [{"name": "run", "image": "toolchain", "command": ["make"], "args": ["wasm"]}, {"name": "out", "image": "pods"}],
        },
        "status": status,
    }


class Kube:
    namespace = "agents"

    def __init__(self):
        self.calls = []
        self.answers = {}
        self.watches = []
        self.gate = None
        self.follows = []
        self.logs_of = {}

    def answer(self, verb, *answers):
        self.answers[verb] = list(answers)

    def ran(self, verb, *args):
        self.calls.append((verb, *args))
        queued = self.answers.get(verb, [])
        if len(queued) > 1:
            return queued.pop(0)
        return queued[0] if queued else ran({})

    def verbs(self):
        return [call[0] for call in self.calls]

    def create(self, pod):
        return self.ran("create", pod)

    def get(self, name):
        return self.ran("get", name)

    def list(self):
        return self.ran("list")

    def logs(self, name, options):
        return self.ran("logs", name, options)

    def delete(self, name):
        return self.ran("delete", name)

    def exec(self, name, container, command):
        if self.gate is not None:
            self.gate.wait()
        return self.ran("exec", name, container, command)

    def annotate(self, name, key, value):
        return self.ran("annotate", name, key, value)

    def apply(self, objects, dry):
        return self.ran("apply", objects, dry)

    def trial(self, pod):
        return self.ran("trial", pod)

    def objects(self, kind, name):
        return self.ran("objects", kind, name)

    def claims(self):
        return self.ran("claims")

    def quotas(self):
        return self.ran("quotas")

    def remove(self, kind, name):
        return self.ran("remove", kind, name)

    def cluster_kinds(self):
        return self.ran("cluster_kinds")

    def watch_pod(self, name):
        self.calls.append(("watch_pod", name))
        return self.watches.pop(0)

    def follow(self, name):
        self.follows.append(name)
        return Log(self.logs_of.get(name, b""))


class Log:
    def __init__(self, data):
        self.stdout = io.BytesIO(data)

    def wait(self):
        return 0


class Watch:
    def __init__(self, code=0):
        read, self.end = os.pipe()
        self.stdout = os.fdopen(read, "rb")
        self.code = code
        self.killed = False

    def send(self, kind, pod):
        os.write(self.end, json.dumps({"type": kind, "object": pod}).encode())

    def close(self):
        os.close(self.end)
        self.end = None

    def wait(self):
        return self.code

    def kill(self):
        self.killed = True
        if self.end is not None:
            self.close()


class Signer:
    def __init__(self, keyed=True):
        self.keyed = keyed
        self.puts = []
        self.submits = []
        self.queries = []
        self.asked = []
        self.rooms = {}
        self.put_ran = ran(b"sha256:ab12cd34ef56ab78cd90\nbundle 7 put at block 900\nbundle 7 held since block 901\n")
        self.submitted = ran(POOLED + b"landed 960 applied 00ff\n")
        self.head = ran(bytes([0, 1, 0]) + bytes.fromhex("ab" * 32) + b"rest of the row")
        self.request = ran(OPEN)

    def held(self):
        return self.keyed

    def put(self, paths, kind):
        self.puts.append(([pathlib.Path(path).read_bytes() for path in paths], kind))
        return taken(self, "put_ran")

    def submit(self, program, payload):
        self.submits.append((program, payload))
        return taken(self, "submitted")

    def query(self, program, request):
        if program == "chat":
            self.asked.append(request)
            answer = self.rooms.get(request, room())
            return answer if isinstance(answer, server.Ran) else ran(answer)
        self.queries.append((program, request))
        return taken(self, "request" if program == "egress" else "head")


class Stats:
    def __init__(self):
        self.hosts = []
        self.summaries = {}

    def summary(self, host):
        self.hosts.append(host)
        return self.summaries.get(host, {"pods": []})


class Api:
    def __init__(self):
        self.calls = []
        self.answers = []

    def call(self, method, path, query, body, kind):
        self.calls.append((method, path, query, body, kind))
        return self.answers.pop(0) if self.answers else server.Answered(200, "application/json", b"{}")


class Served(unittest.TestCase):
    def serve(self, signer=None):
        self.kube = Kube()
        self.kube.answer("cluster_kinds", ran(KINDS))
        self.signer = signer or Signer()
        self.stats = Stats()
        self.api = Api()
        service = self.service = server.Service(self.kube, self.signer, FETCHING, self.stats, self.api)
        self.paused = []
        service.pause = self.paused.append
        quiet = type("Quiet", (server.handler(service),), {"log_message": lambda *_: None})
        self.http = server.ThreadingHTTPServer(("127.0.0.1", 0), quiet)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        self.addCleanup(self.http.server_close)
        self.addCleanup(self.http.shutdown)

    def setUp(self):
        self.serve()

    def call(self, method, path, body=None, headers=ASKED):
        connection = http.client.HTTPConnection(*self.http.server_address)
        payload = body if body is None or isinstance(body, bytes) else json.dumps(body).encode()
        connection.request(method, path, body=payload, headers=headers)
        response = connection.getresponse()
        data = response.read()
        connection.close()
        kind = response.getheader("content-type")
        return response.status, json.loads(data) if kind == "application/json" else data

    def created(self):
        return [call[1] for call in self.kube.calls if call[0] == "create"][0]

    def refused(self, method, path, body, status, words):
        code, answer = self.call(method, path, body)
        self.assertEqual(code, status, answer)
        for word in words:
            self.assertIn(word, answer["error"])
        return answer


class Streamed(Served):
    def stream(self, path, method="GET", body=None):
        connection = http.client.HTTPConnection(*self.http.server_address)
        self.addCleanup(connection.close)
        connection.request(method, path, body=None if body is None else json.dumps(body).encode(), headers=ASKED)
        return connection.getresponse()

    def event(self, response):
        lines = []
        while (line := response.readline()) not in (b"\n", b""):
            lines.append(line.decode().rstrip("\n"))
        fields = dict(line.split(": ", 1) for line in lines)
        return fields["event"], json.loads(fields["data"])

    def past_alive(self, response):
        while (heard := self.event(response))[0] == "alive":
            pass
        return heard


class Wait(Streamed):
    def test_a_wait_on_an_ended_pod_answers_ended_at_once(self):
        self.kube.answer("get", ran(pod_json(state=ENDED, phase="Succeeded")))
        response = self.stream("/v1/pods/duckpond-41/wait?every=30")
        self.assertEqual((response.status, response.getheader("content-type")), (200, "text/event-stream"))
        kind, data = self.event(response)
        self.assertEqual((kind, data["exit_code"], data["line"]), ("ended", 0, "pod duckpond-41 ended: exit code 0 (Completed)"))
        self.assertEqual(data["pod"]["name"], "duckpond-41")
        self.assertEqual(response.read(), b"")
        self.assertNotIn("watch_pod", self.kube.verbs())

    def test_a_wait_names_every_in_whole_seconds_from_one(self):
        for query, words in (("", "names every"), ("?every=0", "from 1"), ("?every=1.5", "whole number"), ("?every=5&tail=3", "no tail")):
            self.refused("GET", f"/v1/pods/duckpond-41/wait{query}", None, 400, [words])
        self.assertNotIn("get", self.kube.verbs())

    def test_a_wait_on_no_pod_is_refused_before_any_stream(self):
        self.kube.answer("get", ran(code=1, err=NOT_FOUND))
        self.refused("GET", "/v1/pods/duckpond-9/wait?every=5", None, 404, ["duckpond-9"])

    def test_a_running_pod_is_alive_each_every_until_its_run_container_ends(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        watch = Watch()
        self.kube.watches.append(watch)
        watch.send("ADDED", pod_json(state=RUNNING, phase="Running"))
        response = self.stream("/v1/pods/duckpond-41/wait?every=1")
        self.assertEqual(self.event(response), ("alive", {}))
        watch.send("MODIFIED", pod_json(state=ENDED, phase="Succeeded"))
        kind, data = self.past_alive(response)
        self.assertEqual((kind, data["exit_code"]), ("ended", 0))
        self.assertEqual(response.read(), b"")
        self.assertTrue(watch.killed)

    def test_a_deleted_pod_ends_the_wait_with_a_line_saying_so(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        watch = Watch()
        self.kube.watches.append(watch)
        watch.send("DELETED", pod_json(state=RUNNING, phase="Running"))
        kind, data = self.event(self.stream("/v1/pods/duckpond-41/wait?every=30"))
        self.assertEqual((kind, data["line"], data["exit_code"]), ("ended", "pod duckpond-41 was deleted", None))

    def test_a_watch_that_ends_reads_the_pod_again_and_watches_on(self):
        running = ran(pod_json(state=RUNNING, phase="Running"))
        self.kube.answer("get", running, running, ran(pod_json(state=ENDED, phase="Succeeded")))
        first, second = Watch(), Watch()
        first.close()
        second.close()
        self.kube.watches += [first, second]
        kind, data = self.event(self.stream("/v1/pods/duckpond-41/wait?every=30"))
        self.assertEqual((kind, data["exit_code"]), ("ended", 0))
        self.assertEqual(self.kube.verbs().count("watch_pod"), 2)

    def test_a_pod_gone_between_watches_ends_the_wait(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")), ran(code=1, err=NOT_FOUND))
        watch = Watch()
        watch.close()
        self.kube.watches.append(watch)
        kind, data = self.event(self.stream("/v1/pods/duckpond-9/wait?every=30"))
        self.assertEqual((kind, data["line"], data["pod"]), ("ended", "pod duckpond-9 is gone", None))


class Framing(Streamed):
    def answering(self, events):
        self.service.answer = lambda *_: (200, server.Stream(events))

    def test_a_stream_is_chunked_and_ends_with_its_last_chunk(self):
        self.answering(event for event in [server.sse("ended", {"exit_code": 0})])
        with socket.create_connection(self.http.server_address) as raw:
            raw.sendall(b"GET /v1/pods/duckpond-41/wait?every=5 HTTP/1.1\r\nhost: pods\r\n\r\n")
            heard = b""
            while data := raw.recv(65536):
                heard += data
        head, _, body = heard.partition(b"\r\n\r\n")
        self.assertIn(b"transfer-encoding: chunked", head.lower())
        self.assertTrue(body.endswith(server.LAST_CHUNK), body)
        self.assertIn(b"event: ended", body)

    def test_a_stream_the_service_stops_before_its_end_reads_as_broken_not_as_an_end(self):
        class Stopped(BaseException):
            pass

        def events():
            yield server.sse("alive", {})
            raise Stopped()

        hook = threading.excepthook
        threading.excepthook = lambda _: None
        self.addCleanup(setattr, threading, "excepthook", hook)
        self.answering(events())
        response = self.stream("/v1/pods/duckpond-41/wait?every=5")
        self.assertEqual(self.event(response), ("alive", {}))
        with self.assertRaises(http.client.IncompleteRead):
            response.read()

    def test_a_stream_that_fails_inside_ends_with_a_refused_event_saying_why(self):
        def events():
            yield server.sse("alive", {})
            raise RuntimeError("the watch broke")

        self.answering(events())
        response = self.stream("/v1/pods/duckpond-41/wait?every=5")
        self.assertEqual(self.event(response), ("alive", {}))
        kind, data = self.event(response)
        self.assertEqual((kind, data["status"]), ("refused", 500))
        self.assertIn("the watch broke", data["error"])
        self.assertEqual(response.read(), b"")


class StreamedExec(Streamed):
    def test_an_exec_with_every_is_alive_each_every_until_its_command_ends(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.answer("exec", ran(b"built\n"))
        self.kube.gate = threading.Event()
        response = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 1})
        self.assertEqual((response.status, response.getheader("content-type")), (200, "text/event-stream"))
        self.assertEqual(self.event(response), ("alive", {}))
        self.kube.gate.set()
        self.assertEqual(self.past_alive(response), ("ended", {"exit_code": 0, "stdout": "built\n", "stderr": ""}))
        self.assertEqual(response.read(), b"")
        self.assertEqual(self.kube.calls[-1], ("exec", "duckpond-41", None, ["make"]))

    def test_an_exec_without_every_still_answers_whole(self):
        self.kube.answer("exec", ran(b"built\n"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})
        self.assertEqual((status, answer), (200, {"exit_code": 0, "stdout": "built\n", "stderr": ""}))
        self.assertNotIn("get", self.kube.verbs())

    def test_a_streamed_exec_on_no_pod_is_refused_before_any_stream(self):
        self.kube.answer("get", ran(code=1, err=NOT_FOUND))
        self.refused("POST", "/v1/pods/duckpond-9/exec", {"command": ["make"], "every": 5}, 404, ["duckpond-9"])
        self.assertNotIn("exec", self.kube.verbs())

    def test_a_streamed_exec_names_every_in_whole_seconds_from_one(self):
        for every, words in ((0, "from 1"), ("5", "JSON int"), (True, "JSON int")):
            self.refused("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"], "every": every}, 400, [words])
        self.assertNotIn("exec", self.kube.verbs())

    def test_a_streamed_exec_the_cluster_refuses_ends_with_the_refusal(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.answer("exec", ran(code=1, err='Error from server (BadRequest): container "nope" is not valid for pod "duckpond-41"'))
        body = {"command": ["make"], "container": "nope", "every": 30}
        kind, data = self.event(self.stream("/v1/pods/duckpond-41/exec", "POST", body))
        self.assertEqual((kind, data["status"]), ("refused", 400))
        self.assertIn("is not valid", data["error"])


class Create(Served):
    def test_a_pod_runs_the_agents_image_confined_at_agents_priority_with_an_out_container(self):
        self.kube.answer("create", ran(pod_json()))
        spec = {"image": "toolchain", "command": ["make"], "args": ["wasm"], "env": {"A": "1"}, "workdir": "/w", "requester": 17}
        status, answer = self.call("POST", "/v1/pods", spec)
        self.assertEqual(status, 201)
        self.assertEqual(answer["pod"]["name"], "duckpond-41")
        pod = self.created()
        self.assertEqual(pod["metadata"]["name"], "duckpond-41")
        self.assertEqual(pod["metadata"]["namespace"], "agents")
        self.assertEqual(
            pod["metadata"]["labels"],
            {
                "app.kubernetes.io/managed-by": "pods",
                "ducktape.industries/chain": "duckpond",
                "ducktape.industries/program": "harness",
                "ducktape.industries/request": "41",
                "ducktape.industries/requester": "17",
            },
        )
        body = pod["spec"]
        self.assertEqual(body["priorityClassName"], "agents")
        self.assertFalse(body["automountServiceAccountToken"])
        self.assertFalse(body["enableServiceLinks"])
        self.assertFalse(body["hostUsers"])
        self.assertTrue(body["shareProcessNamespace"])
        self.assertEqual(body["restartPolicy"], "Never")
        self.assertNotIn("resources", body)
        self.assertNotIn("initContainers", body)
        run, out = body["containers"]
        self.assertEqual((run["name"], run["image"], run["command"], run["args"], run["workingDir"]), ("run", "toolchain", ["make"], ["wasm"], "/w"))
        self.assertEqual(run["env"], [{"name": "A", "value": "1"}])
        self.assertNotIn("resources", run)
        self.assertFalse(run["securityContext"]["allowPrivilegeEscalation"])
        self.assertEqual((out["name"], out["image"], out["command"]), ("out", FETCHING.image, ["sleep", "infinity"]))
        self.assertEqual(run["volumeMounts"], [{"name": "out", "mountPath": "/out"}])
        self.assertEqual(out["volumeMounts"], [{"name": "out", "mountPath": "/out"}])
        self.assertEqual(body["volumes"], [claimed("out", pod["metadata"]["labels"])])

    def test_resources_are_the_pods_own_limits_and_requests_as_given(self):
        self.kube.answer("create", ran(pod_json()))
        resources = {"limits": {"cpu": "4", "memory": "8Gi"}, "requests": {"cpu": "2"}}
        self.call("POST", "/v1/pods", {"image": "toolchain", "resources": resources})
        self.assertEqual(self.created()["spec"]["resources"], resources)

    def test_resources_name_only_limits_and_requests_of_cpu_and_memory(self):
        self.refused("POST", "/v1/pods", {"image": "t", "resources": {"limits": {"gpu": "1"}}}, 400, ["no field gpu"])
        self.refused("POST", "/v1/pods", {"image": "t", "resources": {"cpu": "1"}}, 400, ["no field cpu"])
        self.refused("POST", "/v1/pods", {"image": "t", "resources": {"limits": {"cpu": 1}}}, 400, ["cpu is a JSON str"])
        self.assertNotIn("create", self.kube.verbs())

    def test_kubernetes_refusing_a_pod_passes_its_reason_through(self):
        self.kube.answer("create", ran(code=1, err=TERMINATING))
        answer = self.refused("POST", "/v1/pods", {"image": "toolchain"}, 403, [])
        self.assertIn("namespace agents because it is being terminated", answer["error"])

    def test_each_input_is_fetched_before_run_and_mounted_at_its_path(self):
        self.kube.answer("create", ran(pod_json()))
        inputs = [
            {"path": "/work/programs", "forge": {"repo": "programs", "commit": "83b18766"}},
            {"path": "/work/spec.md", "files": {"path": "/docs/spec.md", "snapshot": SNAPSHOT}},
            {"path": "/work/fixture.wasm", "blob": CHUNKS[0]},
        ]
        self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs})
        body = self.created()["spec"]
        (fetch,) = body["initContainers"]
        self.assertEqual((fetch["name"], fetch["image"]), ("fetch", FETCHING.image))
        self.assertEqual(fetch["command"], ["python3", "/usr/local/lib/pods/server.py", "fetch"])
        env = {entry["name"]: entry["value"] for entry in fetch["env"]}
        self.assertEqual(json.loads(env["PODS_INPUTS"]), inputs)
        self.assertEqual(env["PODS_NODE"], "https://node.example")
        self.assertEqual(env["PODS_FORGE"], "duck://duckpond/forge")
        self.assertEqual(
            [(mount["name"], mount["mountPath"]) for mount in fetch["volumeMounts"]],
            [("in-0", "/in/0"), ("in-1", "/in/1"), ("in-2", "/in/2")],
        )
        run = body["containers"][0]
        self.assertEqual(
            run["volumeMounts"][1:],
            [
                {"name": "in-0", "mountPath": "/work/programs", "subPath": "data"},
                {"name": "in-1", "mountPath": "/work/spec.md", "subPath": "data"},
                {"name": "in-2", "mountPath": "/work/fixture.wasm", "subPath": "data"},
            ],
        )
        self.assertEqual(body["containers"][1]["volumeMounts"], run["volumeMounts"])
        labels = self.created()["metadata"]["labels"]
        self.assertEqual(body["volumes"], [claimed(name, labels) for name in ("out", "in-0", "in-1", "in-2")])

    def test_a_scratch_input_is_an_empty_directory_in_memory_that_nothing_fetches(self):
        self.kube.answer("create", ran(pod_json()))
        inputs = [
            {"path": "/work/programs", "forge": {"repo": "programs", "commit": "83b18766"}},
            {"path": "/var/tmp/target", "scratch": {"medium": "Memory"}},
        ]
        self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs})
        body = self.created()["spec"]
        (fetch,) = body["initContainers"]
        self.assertEqual([mount["name"] for mount in fetch["volumeMounts"]], ["in-0"])
        run = body["containers"][0]
        self.assertEqual(run["volumeMounts"][2], {"name": "in-1", "mountPath": "/var/tmp/target"})
        self.assertEqual(body["containers"][1]["volumeMounts"], run["volumeMounts"])
        self.assertEqual(body["volumes"][2], {"name": "in-1", "emptyDir": {"medium": "Memory"}})
        labels = self.created()["metadata"]["labels"]
        self.assertEqual(body["volumes"][:2], [claimed("out", labels), claimed("in-0", labels)])

    def test_a_scratch_input_lives_in_memory_only(self):
        for scratch, words in (({}, "names its medium"), ({"medium": "Disk"}, "medium is Memory"), ({"medium": "Memory", "bytes": "1Gi"}, "no field bytes")):
            self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/s", "scratch": scratch}]}, 400, [words])
        self.assertNotIn("create", self.kube.verbs())

    def test_a_scratch_size_is_its_empty_directorys_size_limit(self):
        self.kube.answer("create", ran(pod_json()))
        sizes = ["48Gi", "512M", "1e9", "1.5Gi", "+64Mi", ".5Ti"]
        inputs = [{"path": f"/scratch/{index}", "scratch": {"medium": "Memory", "size": size}} for index, size in enumerate(sizes)]
        status, _ = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs})
        self.assertEqual(status, 201)
        volumes = self.created()["spec"]["volumes"][1:]
        self.assertEqual(volumes, [{"name": f"in-{index}", "emptyDir": {"medium": "Memory", "sizeLimit": size}} for index, size in enumerate(sizes)])

    def test_a_scratch_without_a_size_has_no_size_limit(self):
        self.kube.answer("create", ran(pod_json()))
        self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": [{"path": "/var/tmp/target", "scratch": {"medium": "Memory"}}]})
        self.assertNotIn("sizeLimit", self.created()["spec"]["volumes"][1]["emptyDir"])

    def test_a_scratch_size_is_a_kubernetes_quantity_above_zero(self):
        malformed = ("", "48GB", "48gi", "Gi", "1.2.3Gi", "48 Gi", "1e", "1e3Gi", "0x10", "forty")
        for size in malformed:
            self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/s", "scratch": {"medium": "Memory", "size": size}}]}, 400, ["is not a Kubernetes quantity"])
        for size in ("0", "0Gi", "0.0", "-1Gi", "-0.5"):
            self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/s", "scratch": {"medium": "Memory", "size": size}}]}, 400, ["is not above 0"])
        self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/s", "scratch": {"medium": "Memory", "size": 48}}]}, 400, ["size is a JSON str"])
        self.assertNotIn("create", self.kube.verbs())

    def test_a_scratch_larger_than_the_memory_request_is_made_with_a_warning(self):
        self.kube.answer("create", ran(pod_json()))
        inputs = [
            {"path": "/work", "scratch": {"medium": "Memory", "size": "80Gi"}},
            {"path": "/small", "scratch": {"medium": "Memory", "size": "8Gi"}},
            {"path": "/just-over", "scratch": {"medium": "Memory", "size": "73729Mi"}},
            {"path": "/unsized", "scratch": {"medium": "Memory"}},
        ]
        spec = {"image": "toolchain", "inputs": inputs, "resources": {"requests": {"memory": "72Gi"}}}
        status, answer = self.call("POST", "/v1/pods", spec)
        self.assertEqual(status, 201)
        self.assertEqual(self.kube.verbs(), ["create"])
        self.assertEqual(answer["pod"]["name"], "duckpond-41")
        self.assertEqual(
            answer["warnings"],
            [
                "scratch /work is 80Gi on a 72Gi memory request; memory scratch counts as pod memory, so the pod can be evicted.",
                "scratch /just-over is 73729Mi on a 72Gi memory request; memory scratch counts as pod memory, so the pod can be evicted.",
                "scratch /unsized has no size, so it can grow past the 72Gi memory request; memory scratch counts as pod memory, so the pod can be evicted.",
            ],
        )

    def test_a_scratch_no_larger_than_the_memory_request_answers_no_warnings(self):
        self.kube.answer("create", ran(pod_json()))
        sizes = ["72Gi", "73728Mi", "77309411328", "64Gi", "1e9"]
        inputs = [{"path": f"/scratch/{index}", "scratch": {"medium": "Memory", "size": size}} for index, size in enumerate(sizes)]
        status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, "resources": {"requests": {"memory": "72Gi"}}})
        self.assertEqual(status, 201)
        self.assertEqual(list(answer), ["pod"])

    def test_a_scratch_in_a_pod_without_a_memory_request_answers_no_warnings(self):
        self.kube.answer("create", ran(pod_json()))
        inputs = [{"path": "/work", "scratch": {"medium": "Memory", "size": "80Gi"}}, {"path": "/unsized", "scratch": {"medium": "Memory"}}]
        for resources in ({}, {"resources": {"limits": {"memory": "8Gi"}, "requests": {"cpu": "2"}}}):
            status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, **resources})
            self.assertEqual(status, 201)
            self.assertEqual(list(answer), ["pod"])

    def test_a_disk_scratch_is_refused_before_any_warning(self):
        inputs = [{"path": "/work", "scratch": {"medium": "Disk", "size": "80Gi"}}]
        answer = self.refused("POST", "/v1/pods", {"image": "t", "inputs": inputs, "resources": {"requests": {"memory": "72Gi"}}}, 400, ["medium is Memory"])
        self.assertNotIn("warnings", answer)
        self.assertNotIn("create", self.kube.verbs())

    def test_an_input_names_one_source_an_absolute_path_and_well_formed_ids(self):
        two = {"path": "/a", "blob": CHUNKS[0], "files": {"path": "/b"}}
        self.refused("POST", "/v1/pods", {"image": "t", "inputs": [two]}, 400, ["input 0 names one source"])
        self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "a", "blob": CHUNKS[0]}]}, 400, ["absolute path"])
        self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/a", "blob": "sha256:zz"}]}, 400, ["not a blob id"])
        self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/a", "forge": {"repo": "programs"}}]}, 400, ["commit"])
        self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/a", "files": {"path": "/b", "snapshot": "x"}}]}, 400, ["not a blob id"])
        self.assertNotIn("create", self.kube.verbs())

    def test_the_same_request_again_answers_the_pod_it_made(self):
        self.kube.answer("create", ran(code=1, err='Error from server (AlreadyExists): pods "duckpond-41" already exists'))
        self.kube.answer("get", ran(pod_json(state={"running": {"startedAt": "s"}}, phase="Running")))
        status, answer = self.call("POST", "/v1/pods", {"image": "toolchain"})
        self.assertEqual(status, 200)
        self.assertEqual(answer["pod"]["phase"], "Running")
        self.assertEqual(list(answer), ["pod"])

    def test_the_same_request_again_warns_as_the_first_did(self):
        self.kube.answer("create", ran(code=1, err='Error from server (AlreadyExists): pods "duckpond-41" already exists'))
        self.kube.answer("get", ran(pod_json(state={"running": {"startedAt": "s"}}, phase="Running")))
        inputs = [{"path": "/work", "scratch": {"medium": "Memory", "size": "80Gi"}}]
        status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, "resources": {"requests": {"memory": "72Gi"}}})
        self.assertEqual(status, 200)
        self.assertEqual(answer["pod"]["phase"], "Running")
        self.assertEqual(answer["warnings"], ["scratch /work is 80Gi on a 72Gi memory request; memory scratch counts as pod memory, so the pod can be evicted."])

    def test_a_spec_the_service_cannot_read_is_refused(self):
        self.refused("POST", "/v1/pods", {"image": "t", "gpu": 1}, 400, ["no field gpu"])
        self.refused("POST", "/v1/pods", {"command": ["make"]}, 400, ["names its image"])
        self.refused("POST", "/v1/pods", {"image": "t", "args": ["a", 1]}, 400, ["args holds strings"])
        self.refused("POST", "/v1/pods", {"image": "t", "env": {"A": 1}}, 400, ["env maps names to strings"])
        self.refused("POST", "/v1/pods", {"image": "t", "requester": True}, 400, ["requester"])
        self.refused("POST", "/v1/pods", {"image": "t", "requester": -1}, 400, ["number from 0"])
        self.refused("POST", "/v1/pods", {"image": "t", "notify": {"thread": 3}}, 400, ["names its channel"])
        self.assertNotIn("create", self.kube.verbs())

    def test_a_pod_records_where_to_post(self):
        self.kube.answer("create", ran(pod_json()))
        self.call("POST", "/v1/pods", {"image": "t", "requester": 17, "notify": {"channel": "builds", "thread": 5}})
        notes = self.created()["metadata"]["annotations"]
        self.assertEqual(notes["ducktape.industries/notify-channel"], "builds")
        self.assertEqual(notes["ducktape.industries/notify-thread"], "5")
        self.assertEqual(notes["kubectl.kubernetes.io/default-container"], "run")


class Asked(Served):
    def test_a_call_without_the_nodes_headers_is_refused(self):
        status, answer = self.call("GET", "/v1/pods", headers={"ducktape-chain": "duckpond"})
        self.assertEqual(status, 400)
        self.assertIn("ducktape-program, ducktape-request", answer["error"])
        self.assertEqual(self.kube.calls, [])

    def test_a_request_header_that_is_not_a_request_number_is_refused_before_anything_runs(self):
        for request in ("operator-probe", "-1", "1.5", str(1 << 64)):
            with self.subTest(request=request):
                status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}, asked(request))
                self.assertEqual(status, 400)
                self.assertIn("is not an egress request number", answer["error"])
        self.assertEqual(self.kube.calls, [])
        self.assertEqual(self.service.execs, {})

    def test_a_path_outside_the_routes_is_not_found(self):
        self.assertEqual(self.call("GET", "/v2/pods")[0], 404)
        self.assertEqual(self.call("PUT", "/v1/pods")[0], 404)
        self.assertEqual(self.call("OPTIONS", "/v1/pods")[0], 501)
        self.assertEqual(self.call("GET", "/v1/pods/a/b/c")[0], 404)

    def test_a_body_that_is_not_json_is_refused(self):
        connection = http.client.HTTPConnection(*self.http.server_address)
        connection.request("POST", "/v1/pods", body=b"{", headers=ASKED)
        response = connection.getresponse()
        self.assertEqual(response.status, 400)
        self.assertIn("not JSON", json.loads(response.read())["error"])
        connection.close()


class Read(Served):
    def test_status_reports_each_container_the_inputs_and_who_asked(self):
        pod = pod_json(state=ENDED, phase="Succeeded", notes={**NOTIFY, "ducktape.industries/notify-thread": "5"})
        pod["spec"]["resources"] = {"limits": {"cpu": "1"}}
        self.kube.answer("get", ran(pod))
        status, answer = self.call("GET", "/v1/pods/duckpond-41")
        self.assertEqual(status, 200)
        view = answer["pod"]
        self.assertEqual(view["phase"], "Succeeded")
        self.assertEqual(view["containers"][0]["status"]["exit_code"], 0)
        self.assertEqual(view["resources"], {"limits": {"cpu": "1"}})
        self.assertEqual(view["asked"], {"chain": "duckpond", "program": "harness", "request": "41"})
        self.assertEqual(view["requester"], 17)
        self.assertEqual(view["notify"], {"channel": "builds", "thread": 5})
        self.assertEqual(view["inputs"], [])

    def test_a_pod_the_scheduler_holds_says_why(self):
        pod = pod_json()
        pod["status"]["conditions"] = [{"type": "PodScheduled", "status": "False", "message": "0/1 nodes are available: Insufficient cpu."}]
        self.kube.answer("get", ran(pod))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41")[1]["pod"]["unscheduled"], "0/1 nodes are available: Insufficient cpu.")

    def test_a_missing_pod_is_not_found(self):
        self.kube.answer("get", ran(code=1, err=NOT_FOUND))
        status, answer = self.call("GET", "/v1/pods/duckpond-9")
        self.assertEqual(status, 404)
        self.assertIn(NOT_FOUND, answer["error"])

    def test_the_list_holds_every_pod_in_the_namespace(self):
        self.kube.answer("list", ran({"items": [pod_json(), pod_json(name="web-7c9", requester=None)]}))
        status, answer = self.call("GET", "/v1/pods")
        self.assertEqual([pod["name"] for pod in answer["pods"]], ["duckpond-41", "web-7c9"])
        self.assertIsNone(answer["pods"][1]["requester"])

    def test_logs_come_whole_unless_the_caller_asks_for_a_tail_a_window_or_a_container(self):
        self.kube.answer("logs", ran(b"compiling\n"))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs"), (200, b"compiling\n"))
        self.call("GET", "/v1/pods/duckpond-41/logs?tail=20&since=60&container=fetch")
        self.assertEqual(self.kube.calls[-1], ("logs", "duckpond-41", ["--container", "fetch", "--tail=20", "--since=60s"]))

    def test_logs_refuse_an_option_they_do_not_take(self):
        self.refused("GET", "/v1/pods/duckpond-41/logs?follow=1", None, 400, ["no follow"])
        self.refused("GET", "/v1/pods/duckpond-41/logs?tail=-1", None, 400, ["whole number"])


class Delete(Served):
    def test_delete_removes_the_pod_without_waiting(self):
        self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-41"), (200, {"deleted": "duckpond-41"}))
        self.assertEqual(self.kube.calls, [("delete", "duckpond-41")])

    def test_deleting_a_missing_pod_is_not_found(self):
        self.kube.answer("delete", ran(code=1, err=NOT_FOUND))
        self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-9")[0], 404)


class Exec(Served):
    def test_exec_answers_the_commands_output_and_exit_code(self):
        self.kube.answer("exec", ran(b"hello\n", err="warn"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["echo", "hello"]})
        self.assertEqual((status, answer), (200, {"exit_code": 0, "stdout": "hello\n", "stderr": "warn"}))
        self.assertEqual(self.kube.calls, [("exec", "duckpond-41", None, ["echo", "hello"])])

    def test_a_command_that_fails_is_still_an_answer(self):
        self.kube.answer("exec", ran(b"", code=2, err="no such file\ncommand terminated with exit code 2"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["ls", "/x"], "container": "out"})
        self.assertEqual((status, answer), (200, {"exit_code": 2, "stdout": "", "stderr": "no such file"}))
        self.assertEqual(self.kube.calls[-1][2], "out")

    def test_output_that_is_not_text_comes_as_hex(self):
        self.kube.answer("exec", ran(b"\xff\x00"))
        self.assertEqual(self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["cat", "b"]})[1]["stdout_hex"], "ff00")

    def test_a_pod_kubernetes_cannot_reach_is_refused_with_why(self):
        self.kube.answer("exec", ran(code=1, err=NOT_FOUND))
        self.assertEqual(self.call("POST", "/v1/pods/duckpond-9/exec", {"command": ["true"]})[0], 404)
        self.refused("POST", "/v1/pods/duckpond-41/exec", {"command": []}, 400, ["command holds strings"])


class Blob(Served):
    def setUp(self):
        super().setUp()
        self.kube.answer("get", ran(pod_json(notes=NOTIFY)))
        self.kube.answer("exec", ran(b"\x00asm"))

    def test_a_file_the_pod_wrote_is_put_as_a_blob_of_its_kind_and_answers_its_id(self):
        status, answer = self.call("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/fixture.wasm", "kind": "program"})
        self.assertEqual(status, 200)
        self.assertEqual(
            answer,
            {"blob": "sha256:ab12cd34ef56ab78cd90", "size": 4, "kind": "program", "bundle": 7, "put_at": 900, "held_since": 901},
        )
        self.assertEqual(self.kube.calls[1], ("exec", "duckpond-41", "out", ["cat", "--", "/out/fixture.wasm"]))
        self.assertEqual(self.signer.puts, [([b"\x00asm"], "program")])

    def test_a_blob_is_read_from_the_container_asked_and_posts_nothing_where_the_pod_asked(self):
        self.call("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program", "container": "run"})
        self.assertEqual(self.kube.calls[1][2], "run")
        self.assertEqual((self.signer.submits, self.kube.verbs()), ([], ["get", "exec"]))

    def test_a_blob_names_its_path_and_kind_and_the_path_is_absolute(self):
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a"}, 400, ["names its kind"])
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "out/a", "kind": "program"}, 400, ["absolute path"])

    def test_a_file_that_cannot_be_read_is_refused_with_why(self):
        self.kube.answer("exec", ran(code=1, err="cat: /out/a: No such file or directory\ncommand terminated with exit code 1"))
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"}, 502, ["No such file"])
        self.assertEqual(self.signer.puts, [])

    def test_a_put_the_network_refuses_is_refused_with_why(self):
        self.signer.put_ran = ran(code=1, err="blob kind program refused: too large")
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"}, 502, ["too large"])

    def test_several_files_are_put_in_one_bundle_and_answered_in_the_order_asked(self):
        self.kube.answer("exec", ran(b"\x00asm"), ran(b"\x00asm\x01"))
        self.signer.put_ran = ran(b"sha256:aa\nsha256:bb\nbundle 7 put at block 900\nbundle 7 held since block 901\n")
        asked = {"paths": ["/out/a.wasm", "/out/b.wasm"], "kind": "program", "container": "run"}
        status, answer = self.call("POST", "/v1/pods/duckpond-41/blob", asked)
        self.assertEqual(status, 200)
        listed = [{"path": "/out/a.wasm", "blob": "sha256:aa", "size": 4}, {"path": "/out/b.wasm", "blob": "sha256:bb", "size": 5}]
        self.assertEqual(answer, {"blobs": listed, "kind": "program", "bundle": 7, "put_at": 900, "held_since": 901})
        cats = [call for call in self.kube.calls if call[0] == "exec"]
        self.assertEqual(cats, [("exec", "duckpond-41", "run", ["cat", "--", path]) for path in asked["paths"]])
        self.assertEqual(self.signer.puts, [([b"\x00asm", b"\x00asm\x01"], "program")])

    def test_a_blob_names_one_path_or_several_paths_and_every_one_is_absolute(self):
        both = {"path": "/out/a", "paths": ["/out/b"], "kind": "program"}
        self.refused("POST", "/v1/pods/duckpond-41/blob", both, 400, ["its path, or its paths"])
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"kind": "program"}, 400, ["its path, or its paths"])
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"paths": [], "kind": "program"}, 400, ["paths holds strings"])
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"paths": ["/out/a", 7], "kind": "program"}, 400, ["paths holds strings"])
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"paths": ["/out/a", "out/b"], "kind": "program"}, 400, ["absolute path"])
        self.assertEqual(self.signer.puts, [])

    def test_a_blob_from_a_missing_pod_is_not_found(self):
        self.kube.answer("get", ran(code=1, err=NOT_FOUND))
        self.assertEqual(self.call("POST", "/v1/pods/duckpond-9/blob", {"path": "/out/a", "kind": "program"})[0], 404)


class File(Served):
    def setUp(self):
        super().setUp()
        self.kube.answer("get", ran(pod_json(notes=NOTIFY)))
        self.kube.answer("exec", ran(b"abcdefghij"))
        self.signer.put_ran = ran(("\n".join(CHUNKS) + "\nbundle 8 put at block 950\nbundle 8 held since block 951\n").encode())
        chunk_size = server.CHUNK_SIZE
        server.CHUNK_SIZE = 4
        self.addCleanup(setattr, server, "CHUNK_SIZE", chunk_size)

    def test_an_output_is_put_as_chunks_and_committed_onto_files_head_as_pods_posting_nothing(self):
        status, answer = self.call("POST", "/v1/pods/duckpond-41/file", {"path": "/out/report.md", "to": "/reports/41.md", "message": "build report"})
        self.assertEqual(status, 200, answer)
        self.assertEqual(answer, {"written": "/reports/41.md", "size": 10, "chunks": CHUNKS, "base": SNAPSHOT, "height": 960})
        self.assertEqual(self.signer.puts, [([b"abcd", b"efgh", b"ij"], "chunk")])
        self.assertEqual(self.signer.queries, [("files", bytes([0]))])
        ((program, payload),) = self.signer.submits
        self.assertEqual(program, "files")
        self.assertEqual(payload, server.files_commit(SNAPSHOT, "build report", "/reports/41.md", 10, CHUNKS))
        self.assertEqual(self.kube.verbs(), ["get", "exec"])

    def test_files_refusing_the_write_passes_its_reason_through(self):
        self.signer.submitted = ran(POOLED + b"landed 960 rejected unauthorized: /reports is written by its write list only\n")
        answer = self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/reports/41.md"}, 403, [])
        self.assertEqual(answer["error"], "files refused the write at block 960: unauthorized: /reports is written by its write list only")
        self.assertEqual(len(self.signer.submits), 1)

    def test_a_write_no_block_carried_answers_that_it_never_landed(self):
        self.signer.submitted = ran(POOLED + b"unlanded\n")
        answer = self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/reports/41.md"}, 502, [])
        self.assertEqual(answer["error"], "the write never landed: no block carried the frame before its anchor aged out")
        self.assertEqual(len(self.signer.submits), 1)

    def test_a_write_whose_wait_for_its_block_failed_is_not_made_again(self):
        self.signer.submitted = [ran(POOLED, code=1, err="ducktape: error sending request: connection reset"), ran(POOLED + b"landed 960 applied 00ff\n")]
        answer = self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/reports/41.md"}, 502, ["the write was sent", "not known", "connection reset"])
        self.assertEqual((len(self.signer.submits), self.paused), (1, []), answer)

    def test_an_empty_output_commits_an_empty_file_without_chunks(self):
        self.kube.answer("exec", ran(b""))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/e"})
        self.assertEqual((status, answer["chunks"], answer["size"]), (200, [], 0))
        self.assertEqual(self.signer.puts, [])

    def test_an_empty_files_tree_commits_without_a_base(self):
        self.signer.head = ran(bytes([0, 0]))
        self.assertIsNone(self.call("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/e"})[1]["base"])

    def test_a_write_names_an_absolute_destination(self):
        self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r"}, 400, ["names its to"])
        self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "e"}, 400, ["to is an absolute path"])

    def test_a_blob_or_a_write_names_no_notify_or_requester_because_neither_posts(self):
        notify = {"path": "/out/a", "kind": "program", "notify": {"channel": "builds"}}
        self.refused("POST", "/v1/pods/duckpond-41/blob", notify, 400, ["no field notify"])
        self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/e", "requester": 17}, 400, ["no field requester"])
        self.assertEqual((self.signer.submits, self.kube.calls), ([], []))


class Push(Served):
    def setUp(self):
        super().setUp()
        self.signer.put_ran = ran(b"sha256:" + b"cd" * 32 + b"\nbundle 9 put at block 990\nbundle 9 held since block 991\n")

    def execs(self):
        return [call for call in self.kube.calls if call[0] == "exec"]

    def test_a_commit_is_resolved_packed_against_what_forge_has_and_put_as_a_pack_blob(self):
        self.kube.answer("exec", ran(f"{COMMIT}\n".encode()), ran(pack_of(3)))
        body = {"path": "/work/programs", "ref": "j-057/x", "have": [HAVE]}
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", body)
        self.assertEqual(status, 200, answer)
        expected = {
            "commit": COMMIT,
            "objects": 3,
            "size": len(pack_of(3)),
            "kind": "pack",
            "pack": "sha256:" + "cd" * 32,
            "bundle": 9,
            "put_at": 990,
            "held_since": 991,
        }
        self.assertEqual(answer, expected)
        resolve, pack = self.execs()
        self.assertEqual(resolve, ("exec", "duckpond-41", None, ["sh", "-c", server.RESOLVING, "resolve", "/work/programs", "j-057/x"]))
        self.assertEqual(pack, ("exec", "duckpond-41", None, ["sh", "-c", server.PACKING, "pack", "/work/programs", COMMIT, HAVE]))
        self.assertEqual(self.signer.puts, [([pack_of(3)], "pack")])
        self.assertEqual((self.signer.submits, self.paused), ([], []))

    def test_head_is_the_default_ref_and_the_container_asked_runs_git(self):
        self.kube.answer("exec", ran(f"{COMMIT}\n".encode()), ran(pack_of(1)))
        self.call("POST", "/v1/pods/duckpond-41/push", {"path": "/work/core", "container": "out"})
        resolve, pack = self.execs()
        self.assertEqual((resolve[2], resolve[3][-1], pack[3][4:]), ("out", "HEAD", ["/work/core", COMMIT]))

    def test_a_commit_forge_already_holds_answers_no_pack_and_puts_nothing(self):
        self.kube.answer("exec", ran(f"{COMMIT}\n".encode()), ran(pack_of(0)))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", {"path": "/work/core", "have": [COMMIT]})
        self.assertEqual((status, answer["pack"], answer["objects"], answer["commit"]), (200, None, 0, COMMIT))
        self.assertEqual(self.signer.puts, [])

    def test_a_ref_that_names_no_commit_is_refused_in_gits_words(self):
        self.kube.answer("exec", ran(code=1, err="fatal: not a git repository\ncommand terminated with exit code 128"))
        self.refused("POST", "/v1/pods/duckpond-41/push", {"path": "/work/x"}, 422, ["HEAD names no commit in /work/x", "not a git repository"])
        self.assertEqual((len(self.execs()), self.signer.puts), (1, []))

    def test_a_pod_kubernetes_cannot_reach_is_refused_with_why(self):
        self.kube.answer("exec", ran(code=1, err=NOT_FOUND))
        self.refused("POST", "/v1/pods/duckpond-9/push", {"path": "/work/x"}, 404, ["duckpond-9"])

    def test_a_push_names_an_absolute_path_a_revision_and_commit_ids(self):
        self.refused("POST", "/v1/pods/duckpond-41/push", {}, 400, ["names its path"])
        self.refused("POST", "/v1/pods/duckpond-41/push", {"path": "work"}, 400, ["absolute path"])
        self.refused("POST", "/v1/pods/duckpond-41/push", {"path": "/w", "ref": "--all"}, 400, ["not an option"])
        self.refused("POST", "/v1/pods/duckpond-41/push", {"path": "/w", "have": ["main"]}, 400, ["commit ids"])
        self.refused("POST", "/v1/pods/duckpond-41/push", {"path": "/w", "repo": "core"}, 400, ["no field repo"])
        self.assertEqual(self.kube.calls, [])


class Retried(Served):
    def setUp(self):
        super().setUp()
        self.kube.answer("get", ran(pod_json()))

    def test_a_read_that_loses_its_connection_is_read_again_while_its_request_waits(self):
        self.kube.answer("exec", ran(code=1, err=DROPPED), ran(b"\x00asm"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"})
        self.assertEqual((status, answer["size"]), (200, 4), answer)
        self.assertEqual(self.kube.verbs(), ["get", "exec", "exec"])
        self.assertEqual(self.signer.queries, [("egress", server.egress_request(41))])
        self.assertEqual(self.paused, [1])

    def test_the_pause_doubles_with_each_lost_connection(self):
        dropped = ran(code=1, err="connection closed before message completed")
        self.signer.put_ran = [dropped, dropped, dropped, ran(b"sha256:ab12cd34ef56ab78cd90\n")]
        self.kube.answer("exec", ran(b"\x00asm"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"})
        self.assertEqual((status, answer["blob"]), (200, "sha256:ab12cd34ef56ab78cd90"), answer)
        self.assertEqual((len(self.signer.puts), self.paused), (4, [1, 2, 4]))

    def test_nothing_is_tried_again_once_the_request_that_asked_is_gone(self):
        self.kube.answer("exec", ran(code=1, err=DROPPED))
        self.signer.request = ran(GONE)
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"}, 502, ["connection reset", "request 41", "is gone"])
        self.assertEqual((self.kube.verbs(), self.paused), (["get", "exec"], []))

    def test_any_other_failure_is_answered_at_once(self):
        self.kube.answer("exec", ran(code=1, err="cat: /out/a: No such file or directory\ncommand terminated with exit code 1"))
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"}, 502, ["No such file"])
        self.signer.put_ran = ran(code=1, err="blob kind program refused: too large")
        self.kube.answer("exec", ran(b"x"))
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"}, 502, ["too large"])
        self.assertEqual((self.signer.queries, self.paused), ([], []))

    def test_a_write_whose_submit_lost_its_connection_reads_the_head_again_and_commits_onto_it(self):
        self.kube.answer("exec", ran(b"abc"))
        self.signer.put_ran = ran(CHUNKS[0].encode() + b"\n")
        self.signer.submitted = [ran(code=1, err="the node failed: connection closed before message completed"), ran(POOLED + b"landed 960 applied 00ff\n")]
        status, answer = self.call("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/r", "message": "m"})
        self.assertEqual((status, answer["base"]), (200, SNAPSHOT), answer)
        files = [query for query in self.signer.queries if query[0] == "files"]
        self.assertEqual((len(files), len(self.signer.submits), self.paused), (2, 2, [1]))
        self.assertEqual(self.signer.submits[0], self.signer.submits[1])

    def test_a_write_files_moved_under_reads_the_head_again_and_commits_onto_it(self):
        moved = "sha256:" + "cd" * 32
        for refusal in [b"not_found: files minted no snapshot " + b"ab" * 32, b"stale: /r changed since its base"]:
            with self.subTest(refusal=refusal):
                self.kube.answer("exec", ran(b"abc"))
                self.signer.put_ran = ran(CHUNKS[0].encode() + b"\n")
                head = bytes([0, 1, 0]) + bytes.fromhex("cd" * 32) + b"rest of the row"
                self.signer.head = [Signer().head, ran(head)]
                self.signer.submitted = [ran(POOLED + b"landed 960 rejected " + refusal + b"\n"), ran(POOLED + b"landed 960 applied 00ff\n")]
                self.signer.queries, self.signer.submits, self.paused[:] = [], [], []
                status, answer = self.call("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/r", "message": "m"})
                self.assertEqual((status, answer["base"]), (200, moved), answer)
                ((_, first), (_, second)) = self.signer.submits
                self.assertEqual(first, server.files_commit(SNAPSHOT, "m", "/r", 3, [CHUNKS[0]]))
                self.assertEqual(second, server.files_commit(moved, "m", "/r", 3, [CHUNKS[0]]))
                self.assertEqual(self.paused, [1])

    def test_a_failure_the_service_did_not_foresee_answers_500_with_why(self):
        self.kube.answer("get", ran(b"not json"))
        status, answer = self.call("GET", "/v1/pods/duckpond-41")
        self.assertEqual(status, 500)
        self.assertIn("the pods service failed", answer["error"])


class Unreached(Streamed):
    def test_an_exec_the_api_server_could_not_take_is_sent_again_while_its_request_waits(self):
        self.kube.answer("exec", ran(code=1, err=APISERVER), ran(code=1, err=APISERVER), ran(b"built\n"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})
        self.assertEqual((status, answer["stdout"]), (200, "built\n"), answer)
        self.assertEqual((self.kube.verbs(), self.paused), (["exec"] * 3, [1, 2]))
        self.assertEqual(self.signer.queries, [("egress", server.egress_request(41))] * 2)

    def test_once_its_request_is_gone_a_streamed_exec_ends_saying_the_apiserver_was_unavailable(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.answer("exec", ran(code=1, err=APISERVER))
        self.signer.request = ran(GONE)
        kind, data = self.event(self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 30}))
        self.assertEqual((kind, data["status"]), ("refused", 503))
        self.assertTrue(data["error"].startswith("apiserver unavailable: "), data)
        self.assertIn("nothing ran", data["error"])
        self.assertEqual(self.kube.verbs().count("exec"), 1)

    def test_an_exec_whose_stream_was_cut_mid_run_is_not_run_again_and_says_so(self):
        self.kube.answer("exec", ran(code=1, err=WEBSOCKET), ran(b"twice\n"))
        answer = self.refused("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}, 502, ["apiserver unavailable", "may still be running", "not run again"])
        self.assertIn("close 1006", answer["error"])
        self.assertEqual((self.kube.verbs(), self.paused), (["exec"], []))

    def test_a_command_that_prints_the_apiserver_phrases_and_fails_is_answered_once_as_it_ran(self):
        for printed in ("Unable to connect to the server: dial tcp", "websocket: close 1006 (abnormal closure)"):
            with self.subTest(printed=printed):
                self.kube.calls.clear()
                self.kube.answer("exec", ran(code=1, err=f"{printed}\ncommand terminated with exit code 1"))
                status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}, asked(60 + len(printed)))
                self.assertEqual((status, answer["exit_code"], answer["stderr"]), (200, 1, printed))
                self.assertEqual((self.kube.verbs(), self.paused), (["exec"], []))

    def test_a_command_that_fails_on_its_own_is_answered_as_it_ran(self):
        self.kube.answer("exec", ran(code=2, err="make: *** [all] Error 2"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})
        self.assertEqual((status, answer["exit_code"]), (200, 2))
        self.assertEqual((self.kube.verbs(), self.paused, self.signer.queries), (["exec"], [], []))


class Unstarted(Streamed):
    def setUp(self):
        super().setUp()
        self.watch = Watch()
        self.kube.watches.append(self.watch)
        self.kube.answer("exec", ran(code=1, err=UNSTARTED), ran(b"built\n"))
        self.addCleanup(self.watch.stdout.close)

    def asked_egress(self):
        heard = threading.Event()
        query = self.signer.query

        def told(program, request):
            heard.set()
            return query(program, request)

        self.signer.query = told
        return heard

    def test_an_exec_before_its_container_starts_runs_once_the_container_runs(self):
        heard = self.asked_egress()
        self.watch.send("ADDED", pod_json(state=FETCHING_STATE, init=RUNNING))
        answers = []
        call = threading.Thread(target=lambda: answers.append(self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})))
        call.start()
        self.assertTrue(heard.wait(10))
        self.assertEqual(self.kube.verbs(), ["exec", "watch_pod"])
        self.watch.send("MODIFIED", pod_json(state=RUNNING, phase="Running", init=ENDED))
        call.join(10)
        self.assertEqual(answers, [(200, {"exit_code": 0, "stdout": "built\n", "stderr": ""})])
        self.assertEqual(self.kube.verbs(), ["exec", "watch_pod", "exec"])
        self.assertEqual(self.signer.queries, [("egress", server.egress_request(41))] * 2)
        self.assertTrue(self.watch.killed)

    def test_an_exec_before_the_pod_has_a_node_runs_once_its_default_container_runs(self):
        self.kube.answer("exec", ran(code=1, err=UNPLACED), ran(b"built\n"))
        heard = self.asked_egress()
        self.watch.send("ADDED", pod_json())
        answers = []
        call = threading.Thread(target=lambda: answers.append(self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})))
        call.start()
        self.assertTrue(heard.wait(10))
        self.assertEqual(self.kube.verbs(), ["exec", "watch_pod"])
        self.watch.send("MODIFIED", pod_json(state=FETCHING_STATE, init=RUNNING))
        self.watch.send("MODIFIED", pod_json(state=RUNNING, phase="Running", init=ENDED))
        call.join(10)
        self.assertEqual(answers, [(200, {"exit_code": 0, "stdout": "built\n", "stderr": ""})])
        self.assertEqual(self.kube.verbs(), ["exec", "watch_pod", "exec"])
        self.assertEqual(self.signer.queries, [("egress", server.egress_request(41))] * 3)

    def test_an_exec_before_the_pod_has_a_node_watches_the_container_it_names(self):
        self.kube.answer("exec", ran(code=1, err=UNPLACED), ran(b"read\n"))
        self.watch.send("ADDED", pod_json())
        self.watch.send("MODIFIED", pod_json(state=FETCHING_STATE, phase="Pending", init=ENDED))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["ls"], "container": "out"})
        self.assertEqual((status, answer["stdout"]), (200, "read\n"))
        self.assertEqual(self.kube.calls[-1][2], "out")
        self.assertEqual(self.signer.queries, [("egress", server.egress_request(41))] * 2)

    def test_a_streamed_exec_is_alive_while_its_container_starts(self):
        self.watch.send("ADDED", pod_json(state=FETCHING_STATE, init=RUNNING))
        self.kube.answer("get", ran(pod_json(state=FETCHING_STATE, init=RUNNING)))
        response = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 1})
        self.assertEqual(self.event(response), ("alive", {}))
        self.watch.send("MODIFIED", pod_json(state=RUNNING, phase="Running", init=ENDED))
        self.assertEqual(self.past_alive(response), ("ended", {"exit_code": 0, "stdout": "built\n", "stderr": ""}))
        self.assertEqual(self.kube.verbs().count("exec"), 2)

    def test_a_blob_or_a_push_waits_for_its_container_too(self):
        self.kube.answer("get", ran(pod_json()))
        self.kube.answer("exec", ran(code=1, err='error: unable to upgrade connection: container not found ("out")'), ran(b"\x00asm"))
        self.watch.send("ADDED", pod_json(state=RUNNING, phase="Running"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"})
        self.assertEqual((status, answer["size"]), (200, 4), answer)
        self.assertEqual(self.kube.verbs(), ["get", "exec", "watch_pod", "exec"])

    def test_a_container_that_ended_or_a_pod_that_failed_answers_the_failure_at_once(self):
        endings = (("MODIFIED", pod_json(state=ENDED, phase="Succeeded")), ("MODIFIED", pod_json(phase="Failed", init=FAILED)), ("DELETED", pod_json()))
        for request, (event, pod) in enumerate(endings, start=50):
            with self.subTest(event=event, phase=pod["status"]["phase"]):
                self.kube.calls.clear()
                self.kube.answer("exec", ran(code=1, err=UNSTARTED))
                watch = Watch()
                self.kube.watches = [watch]
                watch.send(event, pod)
                status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}, asked(request))
                self.assertEqual((status, answer["exit_code"], answer["stderr"]), (200, 1, UNSTARTED))
                self.assertEqual(self.kube.verbs(), ["exec", "watch_pod"])
        self.assertEqual(self.signer.queries, [])

    def test_nothing_runs_once_the_request_that_asked_is_gone(self):
        self.signer.request = ran(GONE)
        self.watch.send("ADDED", pod_json(state=RUNNING, phase="Running"))
        self.refused("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}, 502, ["container run", "had not started", "request 41", "nothing ran"])
        self.assertEqual(self.kube.verbs(), ["exec", "watch_pod"])

    def test_a_watch_that_ends_reads_the_pod_again(self):
        self.watch.close()
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})
        self.assertEqual((status, answer["stdout"]), (200, "built\n"))
        self.assertEqual(self.kube.verbs(), ["exec", "watch_pod", "get", "exec"])

    def test_a_pod_gone_between_watches_answers_the_failure(self):
        self.watch.close()
        self.kube.answer("get", ran(code=1, err=NOT_FOUND))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})
        self.assertEqual((status, answer["stderr"]), (200, UNSTARTED))
        self.assertEqual(self.kube.verbs(), ["exec", "watch_pod", "get"])

class Repeated(Streamed):
    def setUp(self):
        super().setUp()
        self.kube.answer("exec", ran(b"once\n"), ran(b"twice\n"))

    def execs(self):
        return self.kube.verbs().count("exec")

    def arrivals(self):
        arrived = queue.Queue()
        execution = self.service.execution

        def told(*args):
            running = execution(*args)
            arrived.put(running)
            return running

        self.service.execution = told
        return arrived

    def test_a_repeat_call_while_its_command_runs_waits_for_that_command_and_runs_nothing(self):
        self.kube.gate = threading.Event()
        arrived = self.arrivals()
        answers = []

        def call():
            answers.append(self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}))

        first, second = threading.Thread(target=call), threading.Thread(target=call)
        first.start()
        running = arrived.get(timeout=10)
        second.start()
        self.assertIs(arrived.get(timeout=10), running)
        self.assertFalse(running.ended.is_set())
        self.kube.gate.set()
        first.join(10)
        second.join(10)
        self.assertEqual(answers, [(200, {"exit_code": 0, "stdout": "once\n", "stderr": ""})] * 2)
        self.assertEqual(self.execs(), 1)

    def test_a_streamed_exec_whose_caller_went_away_answers_its_exit_code_to_the_repeat_call(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.answer("exec", ran(b"built\n", code=3, err="warn"))
        self.kube.gate = threading.Event()
        lost = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 1})
        self.assertEqual(self.event(lost), ("alive", {}))
        lost.close()
        again = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 1})
        self.assertEqual(self.event(again), ("alive", {}))
        self.kube.gate.set()
        self.assertEqual(self.past_alive(again), ("ended", {"exit_code": 3, "stdout": "built\n", "stderr": "warn"}))
        self.assertEqual(self.execs(), 1)

    def test_a_repeat_call_after_its_command_ended_answers_what_it_answered(self):
        for _ in range(2):
            self.assertEqual(self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})[1]["stdout"], "once\n")
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        kind, data = self.past_alive(self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 1}))
        self.assertEqual((kind, data["stdout"]), ("ended", "once\n"))
        self.assertEqual(self.execs(), 1)

    def test_each_request_runs_its_own_command(self):
        stdout = [self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}, asked(request))[1]["stdout"] for request in (41, 42)]
        self.assertEqual((stdout, self.execs()), (["once\n", "twice\n"], 2))

    def test_a_record_is_kept_while_its_request_is_open_and_forgotten_once_it_is_gone(self):
        self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})
        self.service.sweep()
        self.assertIn(("duckpond", "harness", "41"), self.service.execs)
        self.signer.request = ran(GONE)
        self.service.sweep()
        self.assertEqual((self.service.execs, self.signer.queries), ({}, [("egress", server.egress_request(41))] * 2))

    def ended(self, *requests):
        for request in requests:
            running = server.Execution({"chain": "duckpond", "program": "operator", "request": request})
            running.end(ran())
            self.service.execs[("duckpond", "operator", request)] = running

    def said(self):
        events = []
        self.addCleanup(setattr, server, "say", server.say)
        server.say = lambda event, **fields: events.append((event, fields))
        return events

    def test_a_record_that_cannot_be_asked_about_stays_and_the_records_after_it_are_still_swept(self):
        said = self.said()
        self.ended("operator-probe", "5")
        self.signer.request = ran(GONE)
        for _ in range(4):
            self.service.sweep()
        self.assertEqual(list(self.service.execs), [("duckpond", "operator", "operator-probe")])
        self.assertEqual([fields["attempts"] for event, fields in said if event == "pods_unswept"], [1, 2, 4])
        self.assertEqual({fields["reason"] for event, fields in said}, {"request_unreadable"})

    def test_a_record_egress_did_not_answer_for_stays_until_egress_says_its_request_is_gone(self):
        said = self.said()
        self.ended("7", "8")
        self.signer.request = [ran(code=1, err="Error: the node refused the query"), ran(GONE)]
        self.service.sweep()
        self.assertEqual(list(self.service.execs), [("duckpond", "operator", "7")])
        self.assertEqual([(event, fields["reason"]) for event, fields in said], [("pods_unswept", "egress_unanswered")])
        self.service.sweep()
        self.assertEqual(self.service.execs, {})

    def test_a_refusal_is_recorded_too(self):
        self.kube.answer("exec", ran(code=1, err=NOT_FOUND))
        self.assertEqual([self.call("POST", "/v1/pods/duckpond-9/exec", {"command": ["true"]})[0] for _ in range(2)], [404, 404])
        self.assertEqual(self.execs(), 1)

class Keyless(Served):
    def setUp(self):
        self.serve(Signer(keyed=False))

    def test_a_service_without_a_key_says_so_and_reads_nothing(self):
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"}, 503, ["holds no key to put blobs"])
        self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/a", "to": "/a"}, 503, ["holds no key to write files"])
        self.refused("POST", "/v1/pods/duckpond-41/push", {"path": "/work/a"}, 503, ["holds no key to put the pack"])
        self.assertEqual(self.kube.calls, [])


def deployment(**template_spec):
    spec = {"containers": [{"name": "web", "image": "nginx"}], **template_spec}
    return {
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": "web"},
        "spec": {"replicas": 1, "template": {"metadata": {"labels": {"app": "web"}}, "spec": spec}},
    }


class Manifests(Served):
    def setUp(self):
        super().setUp()
        self.kube.answer("apply", ran({"kind": "List", "items": [{"kind": "Deployment", "metadata": {"name": "web"}}]}))

    def applied(self):
        return [call for call in self.kube.calls if call[0] == "apply"]

    def test_a_deployment_is_labelled_confined_tried_then_applied(self):
        status, answer = self.call("POST", "/v1/manifests", {"manifests": [deployment()], "requester": 17})
        self.assertEqual((status, answer), (200, {"applied": [{"kind": "Deployment", "name": "web"}]}))
        self.assertEqual(self.kube.verbs(), ["cluster_kinds", "trial", "apply", "apply"])
        trial = self.kube.calls[1][1]
        self.assertEqual(trial["kind"], "Pod")
        self.assertEqual(trial["metadata"]["generateName"], "web-")
        self.assertEqual(trial["metadata"]["labels"]["app"], "web")
        (_, objects, dry), (_, again, real) = self.applied()
        self.assertEqual((dry, real), (True, False))
        self.assertEqual(objects, again)
        (manifest,) = objects
        labels = {
            "ducktape.industries/chain": "duckpond",
            "ducktape.industries/program": "harness",
            "ducktape.industries/request": "41",
            "ducktape.industries/requester": "17",
        }
        self.assertEqual(manifest["metadata"]["namespace"], "agents")
        self.assertEqual(manifest["metadata"]["labels"], labels)
        template = manifest["spec"]["template"]
        self.assertEqual(template["metadata"]["labels"], {"app": "web", **labels})
        self.assertEqual(template["spec"]["priorityClassName"], "agents")
        self.assertFalse(template["spec"]["automountServiceAccountToken"])

    def test_kubernetes_refusing_a_templates_pod_passes_its_reason_through_before_anything_applies(self):
        self.kube.answer("trial", ran(code=1, err=BASELINE))
        answer = self.refused("POST", "/v1/manifests", {"manifests": [deployment()]}, 403, ["manifest 0 (Deployment web)", "violates PodSecurity"])
        self.assertNotIn("apply", self.kube.verbs())
        self.assertNotIn("requester", answer)

    def test_kubernetes_refusing_the_dry_run_applies_nothing(self):
        self.kube.answer("apply", ran(code=1, err='Error from server (Invalid): Deployment.apps "web" is invalid'))
        self.refused("POST", "/v1/manifests", {"manifests": [deployment()]}, 422, ["is invalid"])
        self.assertEqual([call[2] for call in self.applied()], [True])

    def test_a_pod_and_a_clusterip_service_and_a_claim_and_a_config_map_apply(self):
        manifests = [
            {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"containers": [{"name": "c", "image": "i"}]}},
            {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "s"}, "spec": {"type": "ClusterIP", "ports": [{"port": 80}]}},
            {"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": "c"}, "spec": {}},
            {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m", "namespace": "agents"}, "data": {}},
            {"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": "a"}},
        ]
        self.assertEqual(self.call("POST", "/v1/manifests", {"manifests": manifests})[0], 200)
        self.assertNotIn("trial", self.kube.verbs())
        objects = self.applied()[0][1]
        self.assertEqual(objects[0]["spec"]["priorityClassName"], "agents")
        self.assertFalse(objects[4]["automountServiceAccountToken"])

    def test_each_refused_manifest_says_why(self):
        def refuses(manifest, *words):
            self.refused("POST", "/v1/manifests", {"manifests": [manifest]}, 403, words)

        refuses({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "k"}}, "Secret k", "credential")
        refuses({"apiVersion": "networking.k8s.io/v1", "kind": "Ingress", "metadata": {"name": "i"}}, "outside the cluster")
        refuses({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": "n"}}, "undo")
        refuses({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": {"name": "r"}}, "grants permissions")
        refuses({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": "r"}}, "grants permissions")
        refuses({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "x"}}, "cluster-scoped")
        refuses({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "x"}}, "cluster-scoped")
        refuses({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "s"}, "spec": {"type": "LoadBalancer"}}, "LoadBalancer")
        refuses({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "s"}, "spec": {"type": "NodePort"}}, "NodePort")
        refuses({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "s"}, "spec": {"externalIPs": ["1.2.3.4"]}}, "externalIPs")
        refuses({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m", "namespace": "duckpond"}}, "namespace duckpond")
        refuses(deployment(volumes=[{"name": "h", "hostPath": {"path": "/"}}]), "hostPath")
        refuses(deployment(hostNetwork=True), "hostNetwork")
        refuses(deployment(hostPID=True, hostIPC=True), "hostPID, hostIPC")
        refuses(deployment(containers=[{"name": "c", "image": "i", "securityContext": {"privileged": True}}]), "privileged")
        refuses(deployment(volumes=[{"name": "t", "projected": {"sources": [{"serviceAccountToken": {"path": "t"}}]}}]), "token")
        refuses(deployment(priorityClassName="system-node-critical"), "priority class system-node-critical")
        self.assertEqual([verb for verb in self.kube.verbs() if verb != "cluster_kinds"], [])

    def test_a_manifest_is_a_kubernetes_object(self):
        self.refused("POST", "/v1/manifests", {"manifests": [{"kind": "Pod"}]}, 400, ["apiVersion and a kind"])
        self.refused("POST", "/v1/manifests", {"manifests": [{"apiVersion": "v1", "kind": "List"}]}, 400, ["send its items"])
        self.refused("POST", "/v1/manifests", {}, 400, ["names its manifests"])

    def test_every_templated_kind_is_confined(self):
        job = {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "j"}, "spec": {"template": {"spec": {"hostNetwork": True}}}}
        cron = {
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "metadata": {"name": "c"},
            "spec": {"jobTemplate": {"spec": {"template": {"spec": {"hostPID": True}}}}},
        }
        stateful = {"apiVersion": "apps/v1", "kind": "StatefulSet", "metadata": {"name": "s"}, "spec": {"template": {"spec": {"hostIPC": True}}}}
        for manifest in (job, cron, stateful):
            self.refused("POST", "/v1/manifests", {"manifests": [manifest]}, 403, ["host namespace"])

    def test_delete_removes_a_kind_by_name_and_refuses_the_refused_kinds_by_any_name(self):
        self.kube.answer("remove", ran(b"deployment.apps/web\n"))
        self.assertEqual(self.call("DELETE", "/v1/manifests", {"kind": "deployment", "name": "web"}), (200, {"deleted": "deployment.apps/web"}))
        self.assertEqual(self.kube.calls[-1], ("remove", "deployment", "web"))
        for kind in ("secrets", "Secret", "netpol", "networkpolicies.networking.k8s.io", "roles", "ing"):
            self.refused("DELETE", "/v1/manifests", {"kind": kind, "name": "x"}, 403, ["is refused"])
        self.assertEqual(self.kube.verbs().count("remove"), 1)

    def test_reading_returns_one_object_or_every_object_of_a_kind(self):
        self.kube.answer("objects", ran({"kind": "Deployment", "metadata": {"name": "web"}}), ran({"items": [{"metadata": {"name": "web"}}]}))
        self.assertEqual(self.call("GET", "/v1/manifests/deployment/web")[1], {"object": {"kind": "Deployment", "metadata": {"name": "web"}}})
        self.assertEqual(self.call("GET", "/v1/manifests/deployments")[1], {"items": [{"metadata": {"name": "web"}}]})
        self.assertEqual(self.kube.calls[-2:], [("objects", "deployment", "web"), ("objects", "deployments", None)])
        self.refused("GET", "/v1/manifests/secrets", None, 403, ["credential"])


MERGE = {**ASKED, "content-type": "application/merge-patch+json"}
DEPLOYMENTS = "/v1/k8s/apis/apps/v1/namespaces/agents/deployments"


def answered(status, body):
    return server.Answered(status, "application/json", json.dumps(body).encode())


class Passthrough(Served):
    def forced(self, **template_spec):
        return deployment(priorityClassName="agents", automountServiceAccountToken=False, **template_spec)

    def test_a_read_reaches_its_path_in_the_namespace_and_answers_kubernetes_verbatim(self):
        missing = {"kind": "Status", "status": "Failure", "reason": "NotFound", "code": 404}
        self.api.answers = [answered(404, missing)]
        status, answer = self.call("GET", "/v1/k8s/api/v1/namespaces/agents/configmaps/m?labelSelector=a%3Db")
        self.assertEqual((status, answer), (404, missing))
        self.assertEqual(self.api.calls, [("GET", "/api/v1/namespaces/agents/configmaps/m", "labelSelector=a%3Db", b"", None)])

    def test_a_path_outside_the_namespace_is_refused_before_kubernetes_sees_it(self):
        paths = (
            "/v1/k8s/api/v1/namespaces/kube-system/pods",
            "/v1/k8s/api/v1/nodes",
            "/v1/k8s/version",
            "/v1/k8s/api/v1/namespaces/agents",
            DEPLOYMENTS + "/../../kube-system/deployments",
            "/v1/k8s/api/v1/namespaces/agents/pods/%2e%2e",
        )
        for path in paths:
            self.refused("GET", path, None, 403, ["not inside namespace agents"])
        self.assertEqual(self.api.calls, [])

    def test_a_refused_resource_is_refused_for_every_method(self):
        self.refused("GET", "/v1/k8s/api/v1/namespaces/agents/secrets", None, 403, ["credential"])
        self.refused("DELETE", "/v1/k8s/apis/rbac.authorization.k8s.io/v1/namespaces/agents/roles/r", None, 403, ["grants permissions"])
        self.refused("POST", "/v1/k8s/apis/networking.k8s.io/v1/namespaces/agents/networkpolicies", {}, 403, ["undo"])
        self.refused("PATCH", "/v1/k8s/apis/networking.k8s.io/v1/namespaces/agents/ingresses/i", {}, 403, ["outside the cluster"])
        self.assertEqual(self.api.calls, [])

    def test_a_created_object_is_confined_labelled_tried_and_sent_as_json(self):
        self.api.answers = [answered(201, {"kind": "Deployment"})]
        status, answer = self.call("POST", DEPLOYMENTS + "?fieldManager=agents", deployment(), headers={**ASKED, "content-type": "application/json"})
        self.assertEqual((status, answer), (201, {"kind": "Deployment"}))
        self.assertEqual(self.kube.verbs(), ["cluster_kinds", "trial"])
        ((method, target, query, body, kind),) = self.api.calls
        self.assertEqual((method, target, query, kind), ("POST", "/apis/apps/v1/namespaces/agents/deployments", "fieldManager=agents", "application/json"))
        sent = json.loads(body)
        self.assertEqual(sent["metadata"]["namespace"], "agents")
        self.assertEqual(sent["metadata"]["labels"]["ducktape.industries/request"], "41")
        self.assertEqual(sent["spec"]["template"]["spec"]["priorityClassName"], "agents")
        self.assertFalse(sent["spec"]["template"]["spec"]["automountServiceAccountToken"])

    def test_a_created_object_that_breaks_a_rule_is_refused_before_kubernetes_sees_it(self):
        self.refused("POST", DEPLOYMENTS, deployment(hostNetwork=True), 403, ["the object (Deployment web)", "host namespace"])
        service = {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "s"}, "spec": {"type": "NodePort"}}
        self.refused("PUT", "/v1/k8s/api/v1/namespaces/agents/services/s", service, 403, ["NodePort"])
        self.refused("POST", "/v1/k8s/api/v1/namespaces/agents/configmaps", {}, 400, ["apiVersion and a kind"])
        self.assertEqual(self.api.calls, [])

    def test_a_patch_is_dry_run_and_checked_then_sent(self):
        patch = b'{"spec": {"replicas": 2}}'
        self.api.answers = [answered(200, self.forced()), answered(200, self.forced())]
        self.assertEqual(self.call("PATCH", DEPLOYMENTS + "/web", patch, headers=MERGE)[0], 200)
        target = "/apis/apps/v1/namespaces/agents/deployments/web"
        kind = "application/merge-patch+json"
        self.assertEqual(self.api.calls, [("PATCH", target, "dryRun=All", patch, kind), ("PATCH", target, "", patch, kind)])

    def test_a_patch_whose_result_breaks_a_rule_is_refused_after_the_dry_run(self):
        results = (
            (deployment(), ["would run outside priority class agents", "PUT"]),
            (self.forced(volumes=[{"name": "h", "hostPath": {"path": "/"}}]), ["hostPath"]),
        )
        for result, words in results:
            self.api.calls, self.api.answers = [], [answered(200, result)]
            self.refused("PATCH", DEPLOYMENTS + "/web?fieldManager=agents", {"spec": {}}, 403, words)
            self.assertEqual([call[2] for call in self.api.calls], ["fieldManager=agents&dryRun=All"])

    def test_kubernetes_refusing_the_dry_run_answers_its_refusal_and_sends_nothing_else(self):
        invalid = {"kind": "Status", "reason": "Invalid", "code": 422}
        self.api.answers = [answered(422, invalid)]
        self.assertEqual(self.call("PATCH", DEPLOYMENTS + "/web", b"{}", headers=MERGE), (422, invalid))
        self.assertEqual(len(self.api.calls), 1)

    def test_a_subresource_passes_through_unchecked(self):
        self.call("PATCH", DEPLOYMENTS + "/web/scale", b'{"spec": {"replicas": 0}}', headers=MERGE)
        self.call("POST", "/v1/k8s/api/v1/namespaces/agents/pods/p/eviction", {"apiVersion": "policy/v1", "kind": "Eviction"})
        self.assertEqual([(call[0], call[2]) for call in self.api.calls], [("PATCH", ""), ("POST", "")])
        self.assertEqual(self.kube.calls, [])


class Usage(Served):
    def test_usage_reports_the_quota_and_each_requesters_share_and_measure(self):
        building = pod_json(name="b", phase="Running", requester="15")
        building["spec"]["resources"] = {"limits": {"cpu": "2", "memory": "4Gi"}}
        building["status"]["hostIP"] = "192.168.75.48"
        building["spec"]["volumes"] = [claimed("out", {}), claimed("in-0", {}), {"name": "cache", "persistentVolumeClaim": {"claimName": "cache"}}]
        done = pod_json(name="d", phase="Succeeded", requester="15")
        done["spec"]["resources"] = {"limits": {"cpu": "8", "memory": "8Gi"}}
        unstated = pod_json(name="u", phase="Running", requester="15")
        web = pod_json(name="w", phase="Running", requester=None)
        web["spec"]["containers"] = [{"name": "a", "resources": {"limits": {"cpu": "500m", "memory": "1Gi"}}}, {"name": "b", "resources": {"limits": {"cpu": "250m", "memory": "512Mi"}}}]
        web["spec"]["initContainers"] = [{"name": "i", "resources": {"limits": {"cpu": "1", "memory": "1Mi"}}}]
        web["status"]["hostIP"] = "192.168.75.48"
        claim = {"metadata": {"name": "cache", "labels": {"ducktape.industries/requester": "15"}}, "spec": {"resources": {"requests": {"storage": "10Gi"}}}}
        quota = {"metadata": {"name": "agents"}, "status": {"hard": {"requests.storage": "500Gi"}, "used": {"requests.storage": "10Gi"}}}
        self.kube.answer("list", ran({"items": [building, done, unstated, web]}))
        self.kube.answer("claims", ran({"items": [claim]}))
        self.kube.answer("quotas", ran({"items": [quota]}))
        whole_disk = {"usedBytes": 211394891776, "capacityBytes": 887466962944}
        self.stats.summaries["192.168.75.48"] = {
            "node": {"nodeName": "fabrics-1"},
            "pods": [
                {
                    "podRef": {"name": "b", "namespace": "agents"},
                    "cpu": {"usageNanoCores": 900},
                    "memory": {"workingSetBytes": 1000},
                    "ephemeral-storage": {"usedBytes": 50},
                    "volume": [
                        {"name": "out", **whole_disk, "pvcRef": {"name": "b-out", "namespace": "agents"}},
                        {"name": "in-0", **whole_disk, "pvcRef": {"name": "b-in-0", "namespace": "agents"}},
                        {"name": "cache", "usedBytes": 4000, "pvcRef": {"name": "cache", "namespace": "agents"}},
                    ],
                },
                {"podRef": {"name": "node", "namespace": "duckpond"}, "cpu": {"usageNanoCores": 1}},
            ]
        }
        status, answer = self.call("GET", "/v1/usage")
        self.assertEqual(status, 200)
        self.assertEqual(self.stats.hosts, ["192.168.75.48"])
        self.assertEqual(answer["quota"], {"agents": {"hard": {"requests.storage": "500Gi"}, "used": {"requests.storage": "10Gi"}}})
        first, second = answer["requesters"]
        self.assertEqual(
            first,
            {
                "requester": 15,
                "pods": 3,
                "claims": 1,
                "cpu_limit": 2,
                "memory_limit_bytes": 4 * 2**30,
                "cpu_nanocores": 900,
                "memory_bytes": 1000,
                "disk_bytes": 50,
                "storage_requested_bytes": 10 * 2**30,
            },
        )
        self.assertEqual((second["requester"], second["cpu_limit"], second["memory_limit_bytes"]), (None, 1, 1536 * 2**20))
        self.assertEqual([row["pod"] for row in answer["pods"]], ["b", "d", "u", "w"])
        self.assertEqual((answer["pods"][2]["cpu_limit"], answer["pods"][2]["memory_limit_bytes"]), (0, 0))
        self.assertEqual(answer["claims"], [{"claim": "cache", "requester": 15, "storage_requested_bytes": 10 * 2**30}])
        self.assertEqual(answer["volume_disks"], [{"node": "fabrics-1", "used_bytes": 211394891776, "capacity_bytes": 887466962944}])

    def test_usage_lists_no_claim_kubernetes_made_for_a_pods_volume(self):
        building = pod_json(name="b", phase="Running", requester="15")
        building["spec"]["volumes"] = [claimed("out", {}), {"name": "cache", "persistentVolumeClaim": {"claimName": "cache"}}]
        labels = {"ducktape.industries/requester": "15"}
        made = {"metadata": {"name": "b-out", "labels": labels, "ownerReferences": [{"apiVersion": "v1", "kind": "Pod", "name": "b", "controller": True}]}, "spec": {"resources": {"requests": {"storage": "1"}}}}
        cache = {"metadata": {"name": "cache", "labels": labels}, "spec": {"resources": {"requests": {"storage": "10Gi"}}}}
        self.kube.answer("list", ran({"items": [building]}))
        self.kube.answer("claims", ran({"items": [made, cache]}))
        self.kube.answer("quotas", ran({"items": []}))
        status, answer = self.call("GET", "/v1/usage")
        self.assertEqual(status, 200)
        self.assertEqual(answer["claims"], [{"claim": "cache", "requester": 15, "storage_requested_bytes": 10 * 2**30}])
        (total,) = answer["requesters"]
        self.assertEqual((total["pods"], total["claims"], total["storage_requested_bytes"]), (1, 1, 10 * 2**30))
        self.assertEqual(answer["volume_disks"], [])

    def test_quantities_read_every_suffix(self):
        self.assertEqual(server.quantity("250m"), 0.25)
        self.assertEqual(server.quantity("2Gi"), 2 * 2**30)
        self.assertEqual(server.quantity("3k"), 3000)
        self.assertEqual(server.quantity("1e3"), 1000)
        with self.assertRaises(server.Refused):
            server.quantity("2Qi")


class Posts(unittest.TestCase):
    def setUp(self):
        self.kube = Kube()
        self.signer = Signer()
        self.notifier = server.Notifier(self.kube, self.signer)

    def test_the_post_is_chats_post_message_with_a_mention_of_the_requester(self):
        line = "pod duckpond-41 ended: exit code 0 (Completed)"
        self.assertEqual(server.post_message("builds", "pods-duckpond-41-ended", 17, line, None), POSTED)
        threaded = server.post_message("builds", "pods-duckpond-41-ended", 17, line, 5)
        self.assertEqual(threaded, POSTED[:-1] + b"\x01" + (5).to_bytes(8, "little"))

    def test_a_post_without_a_requester_mentions_no_one(self):
        posted = server.post_message("builds", "m", None, "pod p ended", None)
        self.assertNotIn(b"<@", posted)

    def test_an_ended_pod_is_posted_once_and_marked(self):
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [("chat", POSTED)])
        read = ("logs", "duckpond-41", ["--container", "run"])
        self.assertEqual(self.kube.calls, [read, ("annotate", "duckpond-41", "ducktape.industries/notified", "ended")])
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes={**NOTIFY, "ducktape.industries/notified": "ended"}))
        self.assertEqual(len(self.signer.submits), 1)

    def test_a_pod_still_running_or_asking_for_no_post_is_not_posted(self):
        self.notifier.seen(pod_json(state={"running": {}}, phase="Running", notes=NOTIFY))
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded"))
        self.assertEqual((self.signer.submits, self.kube.calls), ([], []))

    def test_a_notified_pods_notify_line_is_posted_led_by_the_pod(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.kube.logs_of["duckpond-41"] = b"building\nducktape-notify j-093 20261019-1 ended: exit 0, GATES ok\n"
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        line = "duckpond-41 \u00b7 20261019-1 ended: exit 0, GATES ok"
        posted = server.post_message("j-093", server.notify_id("duckpond-41", "j-093", line), 17, line, None)
        self.assertEqual(self.signer.submits, [("chat", posted)])
        self.assertEqual((self.kube.follows, self.kube.calls, self.notifier.following), (["duckpond-41"], [], set()))

    def test_a_pod_followed_again_posts_each_line_once(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.kube.logs_of["duckpond-41"] = b"ducktape-notify j-093 run 1 ended\n"
        for _ in range(2):
            self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(len(self.signer.submits), 1)

    def test_a_lines_id_keeps_its_whole_digest(self):
        line = "duckpond-41 \u00b7 run 1 ended"
        digest = server.hashlib.sha256(f"j-093\n{line}".encode()).hexdigest()
        self.assertEqual(server.notify_id("duckpond-41", "j-093", line), f"pods-duckpond-41-notify-{digest}")

    def test_a_line_to_a_room_its_requester_may_not_post_in_is_not_posted(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.signer.rooms[server.chat_channel("dm-22-23")] = room(owner=22, members_only=True)
        self.signer.rooms[server.chat_members("dm-22-23", None)] = members([22, 23])
        self.kube.logs_of["duckpond-41"] = b"ducktape-notify dm-22-23 relay this\n"
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [])

    def test_a_member_found_on_a_later_page_of_a_members_only_room_posts(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.signer.rooms[server.chat_channel("team")] = room(owner=22, members_only=True)
        self.signer.rooms[server.chat_members("team", None)] = members([5], after=b"next")
        self.signer.rooms[server.chat_members("team", b"next")] = members([17])
        self.kube.logs_of["duckpond-41"] = b"ducktape-notify team run 1 ended\n"
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(len(self.signer.submits), 1)

    def test_an_archived_room_and_a_pod_with_no_requester_post_nothing(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.signer.rooms[server.chat_channel("old")] = room(archived=True)
        self.kube.logs_of["duckpond-41"] = b"ducktape-notify old run 1 ended\n"
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.kube.logs_of["duckpond-42"] = b"ducktape-notify j-093 run 2 ended\n"
        self.notifier.seen(pod_json(name="duckpond-42", state=RUNNING, phase="Running", notes=NOTIFY, requester=None))
        self.assertEqual(self.signer.submits, [])

    def test_the_end_post_needs_its_requester_admitted_too(self):
        self.signer.rooms[server.chat_channel("builds")] = room(owner=22, members_only=True)
        self.signer.rooms[server.chat_members("builds", None)] = members([22])
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual((self.signer.submits, self.kube.verbs()), ([], ["logs"]))

    def test_a_pod_never_seen_running_posts_its_lines_before_its_end(self):
        self.kube.answer("logs", ran(b"building\nducktape-notify j-093 run 1 ended: exit 0, GATES ok\n"))
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        line = "duckpond-41 \u00b7 run 1 ended: exit 0, GATES ok"
        posted = server.post_message("j-093", server.notify_id("duckpond-41", "j-093", line), 17, line, None)
        self.assertEqual(self.signer.submits, [("chat", posted), ("chat", POSTED)])
        self.assertEqual(self.notifier.posted, {})

    def test_a_deleted_pod_forgets_the_lines_it_posted(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.kube.logs_of["duckpond-41"] = b"ducktape-notify j-093 run 1 ended\n"
        self.notifier.event({"type": "ADDED", "object": pod_json(state=RUNNING, phase="Running", notes=NOTIFY)})
        self.assertEqual(len(self.notifier.posted["duckpond-41"]), 1)
        self.notifier.event({"type": "DELETED", "object": pod_json(state=RUNNING, phase="Running", notes=NOTIFY)})
        self.assertEqual(self.notifier.posted, {})

    def test_a_failed_chat_read_posts_nothing_and_says_why_in_detail(self):
        self.signer.rooms[server.chat_channel("builds")] = ran(code=1, err="node unreachable")
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [])
        said.assert_any_call("pods_unposted", message="pods-duckpond-41-ended", reason="chat_unasked", detail=json.dumps("node unreachable"))

    def test_a_refusal_with_no_sentence_logs_its_token_alone(self):
        self.signer.rooms[server.chat_channel("builds")] = room(archived=True)
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        said.assert_any_call("pods_unposted", message="pods-duckpond-41-ended", reason="archived")

    def test_a_pod_already_followed_is_not_followed_twice(self):
        started = []
        self.notifier.spawn = lambda target, *args: started.append(args)
        for _ in range(2):
            self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(started, [("duckpond-41", 17)])

    def test_a_line_with_no_channel_or_no_text_posts_nothing(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.kube.logs_of["duckpond-41"] = b"ducktape-notify\nducktape-notify j-1\nducktape-notify j-1    \nnotify j-1 x\n"
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [])

    def test_a_pod_without_notify_or_not_running_is_not_followed(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.notifier.seen(pod_json(state=RUNNING, phase="Running"))
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes={**NOTIFY, "ducktape.industries/notified": "ended"}))
        self.assertEqual(self.kube.follows, [])

    def test_a_pod_whose_inputs_failed_is_posted_with_why(self):
        failed = {"terminated": {"exitCode": 1, "message": "cloning\nduck://duckpond/forge/x was not cloned: not found\n"}}
        pod = pod_json(phase="Failed", init=failed, notes=NOTIFY)
        self.assertEqual(server.ended_line(pod), "pod duckpond-41 ended: fetching its inputs failed, exit code 1: duck://duckpond/forge/x was not cloned: not found")

    def test_a_pod_the_kubelet_failed_is_posted_with_its_reason(self):
        self.assertEqual(server.ended_line(pod_json(phase="Failed", reason="Evicted")), "pod duckpond-41 ended: Evicted")

    def test_a_refused_post_is_not_marked_and_a_repeated_one_is(self):
        self.signer.submitted = ran(POOLED + b"landed 960 rejected unauthorized: pods may not post in builds\n")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.kube.verbs(), ["logs"])
        self.signer.submitted = ran(POOLED + b"landed 960 rejected already_exists: message pods-duckpond-41-ended exists\n")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.kube.verbs(), ["logs", "logs", "annotate"])

    def test_a_post_no_block_carried_is_not_marked(self):
        self.signer.submitted = ran(POOLED + b"unlanded\n")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.kube.verbs(), ["logs"])

    def test_a_post_that_was_not_sent_is_not_marked(self):
        self.signer.submitted = ran(code=1, err="node unreachable")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.kube.verbs(), ["logs"])

    def test_without_a_key_nothing_is_posted_or_marked(self):
        self.signer.keyed = False
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual((self.signer.submits, self.kube.verbs()), ([], ["logs"]))


class Encodings(unittest.TestCase):
    def test_blob_ids_encode_their_hash_kind_then_digest(self):
        self.assertEqual(server.blob_id("sha256:" + "ab" * 32), b"\x00" + b"\xab" * 32)
        self.assertEqual(server.blob_id("sha1:" + "cd" * 20), b"\x01" + b"\xcd" * 20)
        for wrong in ("sha256:ab", "md5:" + "ab" * 16, "ab" * 32):
            with self.assertRaises(server.Refused):
                server.blob_id(wrong)

    def test_a_commit_puts_one_chunked_file_onto_its_base(self):
        expected = (
            b"\x01"
            + b"\x01" + b"\x00" + b"\xab" * 32
            + b"\x02\x00\x00\x00hi"
            + b"\x01\x00\x00\x00"
            + b"\x00" + b"\x02\x00\x00\x00/a" + b"\x00" + b"\x00\x00\x00\x00"
            + b"\x01" + (5).to_bytes(8, "little") + b"\x01\x00\x00\x00" + b"\x00" + b"\x01" * 32
        )
        self.assertEqual(server.files_commit(SNAPSHOT, "hi", "/a", 5, CHUNKS[:1]), expected)

    def test_a_read_asks_for_a_path_at_a_snapshot_from_an_offset(self):
        expected = b"\x05" + b"\x02\x00\x00\x00/a" + b"\x01\x00" + b"\xab" * 32 + (3).to_bytes(8, "little") + (9).to_bytes(8, "little")
        self.assertEqual(server.files_read("/a", SNAPSHOT, 3, 9), expected)
        self.assertEqual(server.files_read("/a", None, 0, 1)[7], 0)

    def test_contents_and_heads_decode(self):
        reply = b"\x05\x00" + b"\xab" * 32 + (7).to_bytes(8, "little") + (2).to_bytes(8, "little") + b"\x03\x00\x00\x00xyz" + b"\x01"
        self.assertEqual(server.contents_of(reply), (SNAPSHOT, 7, 2, b"xyz", True))
        self.assertEqual(server.head_of(bytes([0, 1, 0]) + b"\xab" * 32 + b"row"), SNAPSHOT)
        self.assertIsNone(server.head_of(bytes([0, 0])))
        with self.assertRaises(server.Refused):
            server.contents_of(b"\x05\x00\xab")

    def test_receipts_read_the_block_that_carried_the_frame(self):
        self.assertEqual(server.receipt(ran(POOLED + b"landed 41 applied 00ff\nevent 0a\n")), ("applied", "00ff", "", 41))
        self.assertEqual(server.receipt(ran(POOLED + b"landed 42 rejected not_found: no channel x\n")), ("rejected", "not_found", "no channel x", 42))
        self.assertEqual(server.receipt(ran(POOLED + b"unlanded\n")), ("unlanded", "", server.UNLANDED, None))
        self.assertEqual(server.receipt(ran(code=1, err="refused")), ("failed", "", "refused", None))
        lost = server.receipt(ran(POOLED, code=1, err="ducktape: the node closed its change stream"))
        self.assertEqual(lost, ("unknown", "", f"the node took it as {'ab' * 32}, but whether a block carried it is not known: ducktape: the node closed its change stream", None))
        self.assertEqual(server.receipt(ran(b"preconfirmed applied 00ff\n")), ("failed", "", "preconfirmed applied 00ff", None))

    def test_a_put_answer_holds_every_id_and_the_bundle(self):
        text = "sha256:aa\nsha256:bb\nbundle 3 put at block 10\nbundle 3 held since block 12\n"
        self.assertEqual(server.put_answer(text, 2), {"blobs": ["sha256:aa", "sha256:bb"], "bundle": 3, "put_at": 10, "held_since": 12})
        with self.assertRaises(server.Refused):
            server.put_answer("bundle 3 put at block 10\n", 1)


class Stream(unittest.TestCase):
    def test_watch_events_read_across_chunk_boundaries(self):
        text = json.dumps({"type": "ADDED", "object": {"a": 1}}) + "\n" + json.dumps({"type": "MODIFIED", "object": {"b": 2}})

        class Chunked:
            def __init__(self):
                self.pieces = [text[:7].encode(), text[7:40].encode(), text[40:].encode()]

            def read1(self, size):
                return self.pieces.pop(0) if self.pieces else b""

        events = list(server.json_stream(Chunked()))
        self.assertEqual([event["type"] for event in events], ["ADDED", "MODIFIED"])


class Fetch(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.log = self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.root = pathlib.Path(temp.name) / "in"
        self.home = pathlib.Path(temp.name) / "home"
        self.calls = []
        self.answers = []
        self.fetcher = server.Fetcher(["ducktape"], "https://node.example", "duck://duckpond/forge", self.home, runner=self.recorded)
        self.paused = []
        self.fetcher.pause = self.paused.append

    def recorded(self, argv, stdin=None, env=None):
        self.calls.append((argv, stdin, env))
        if argv[:2] == ["git", "-c"] and "clone" in argv:
            target = pathlib.Path(argv[-1])
            (target / ".git").mkdir(parents=True)
            (target / "build.sh").write_text("make")
            (target / "build.sh").chmod(0o700)
        if "wallet" in argv:
            (self.home / "keys").mkdir(parents=True)
            (self.home / "keys" / "fetch").write_text("k")
        return self.answers.pop(0) if self.answers else ran(b"")

    def slot(self, index):
        (self.root / str(index)).mkdir(parents=True)

    def test_a_forge_input_is_cloned_from_the_callers_forge_and_checked_out_at_its_commit(self):
        self.slot(0)
        self.fetcher.fetch([{"path": "/w", "forge": {"repo": "programs", "commit": "83b18766"}}], self.root)
        clone, held, checkout = [call[0] for call in self.calls]
        git = ["git", "-c", "duck.duckpond.node=https://node.example"]
        self.assertEqual(clone, [*git, "clone", "--no-checkout", "duck://duckpond/forge/programs", str(self.root / "0" / "data")])
        self.assertEqual(held, [*git, "-C", str(self.root / "0" / "data"), "cat-file", "-e", "83b18766^{commit}"])
        self.assertEqual(checkout, [*git, "-C", str(self.root / "0" / "data"), "checkout", "--detach", "83b18766"])
        self.assertEqual((self.root / "0" / "data" / "build.sh").stat().st_mode & 0o777, 0o777)
        self.assertEqual(self.calls[0][2]["DUCKTAPE_DEVICE_KEY_STORE"], "file")

    def test_a_files_input_is_read_whole_at_one_snapshot_with_a_minted_reading_key(self):
        self.slot(0)
        first = b"\x05\x00" + b"\xab" * 32 + (6).to_bytes(8, "little") + (0).to_bytes(8, "little") + b"\x03\x00\x00\x00abc" + b"\x00"
        last = b"\x05\x00" + b"\xab" * 32 + (6).to_bytes(8, "little") + (3).to_bytes(8, "little") + b"\x03\x00\x00\x00def" + b"\x01"
        self.answers = [ran(b"minted"), ran(first), ran(last)]
        self.fetcher.fetch([{"path": "/w/spec.md", "files": {"path": "/docs/spec.md"}}], self.root)
        wallet, read_first, read_last = self.calls
        self.assertIn("wallet", wallet[0])
        self.assertEqual(read_first[1], server.files_read("/docs/spec.md", None, 0, server.CHUNK_SIZE))
        self.assertEqual(read_last[1], server.files_read("/docs/spec.md", SNAPSHOT, 3, server.CHUNK_SIZE))
        self.assertEqual(read_last[0][:5], ["ducktape", "--workspace", str(self.home), "--node", "https://node.example"])
        self.assertEqual((self.root / "0" / "data").read_bytes(), b"abcdef")
        self.assertEqual((self.root / "0" / "data").stat().st_mode & 0o777, 0o666)

    def test_a_blob_input_is_its_bytes_without_the_header(self):
        self.slot(0)
        self.answers = [ran(b"program 4\x00\x00asm")]
        self.fetcher.fetch([{"path": "/w/a.wasm", "blob": CHUNKS[0]}], self.root)
        self.assertEqual(self.calls[0][0][-3:], ["blob", "get", CHUNKS[0]])
        self.assertEqual((self.root / "0" / "data").read_bytes(), b"\x00asm")

    def test_a_scratch_input_fetches_nothing(self):
        self.fetcher.fetch([{"path": "/var/tmp/target", "scratch": {"medium": "Memory"}}], self.root)
        self.assertEqual(self.calls, [])
        self.assertFalse((self.root / "0").exists())

    def test_a_fetch_that_fails_says_what_and_why(self):
        self.slot(0)
        self.answers = [ran(code=128, err="fatal: repository not found")]
        with self.assertRaises(server.Refused) as caught:
            self.fetcher.fetch([{"path": "/w", "forge": {"repo": "nope", "commit": "1"}}], self.root)
        self.assertIn("duck://duckpond/forge/nope was not cloned: fatal: repository not found", caught.exception.sentence)
        self.assertEqual(self.paused, [])

    def test_a_clone_that_loses_its_connection_is_cloned_again_with_doubling_pauses_and_the_pod_runs(self):
        self.slot(0)
        self.answers = [ran(code=128, err=LOST), ran(code=128, err=LOST), ran(b""), ran(b"")]
        self.fetcher.fetch([{"path": "/w", "forge": {"repo": "programs", "commit": "83b18766"}}], self.root)
        verbs = [call[0][3] for call in self.calls]
        self.assertEqual((verbs, self.paused), (["clone", "clone", "clone", "-C", "-C"], [1, 2]))
        self.assertEqual((self.root / "0" / "data" / "build.sh").read_text(), "make")
        lines = self.log.getvalue().splitlines()
        self.assertEqual(len(lines), 3, lines)
        self.assertIn("duck://duckpond/forge/programs was not cloned, it lost its connection, so it is tried again in 2 s", lines[1])
        self.assertIn("error sending request", lines[1])
        self.assertEqual(lines[2], "cloned duck://duckpond/forge/programs at 83b18766")

    def test_a_commit_the_clone_did_not_bring_is_fetched_by_its_id_then_checked_out(self):
        self.slot(0)
        self.answers = [ran(b""), ran(code=128, err=f"fatal: Not a valid object name {MERGED}^{{commit}}")]
        self.fetcher.fetch([{"path": "/w", "forge": {"repo": "programs", "commit": MERGED}}], self.root)
        inside = ["git", "-c", "duck.duckpond.node=https://node.example", "-C", str(self.root / "0" / "data")]
        self.assertEqual([call[0] for call in self.calls[2:]], [[*inside, "fetch", "origin", MERGED], [*inside, "checkout", "--detach", MERGED]])
        self.assertEqual(self.log.getvalue().splitlines(), [f"{MERGED} is on no branch of duck://duckpond/forge/programs, so it was fetched by its id", f"cloned duck://duckpond/forge/programs at {MERGED}"])

    def test_a_fetch_by_id_that_loses_its_connection_is_tried_again_with_doubling_pauses(self):
        self.slot(0)
        self.answers = [ran(b""), ran(code=128), ran(code=128, err=LOST), ran(code=128, err=LOST), ran(b""), ran(b"")]
        self.fetcher.fetch([{"path": "/w", "forge": {"repo": "programs", "commit": MERGED}}], self.root)
        verbs = [call[0][5] for call in self.calls[1:]]
        self.assertEqual((verbs, self.paused), (["cat-file", "fetch", "fetch", "fetch", "checkout"], [1, 2]))
        self.assertIn(f"programs has no commit {MERGED} on any ref, it lost its connection, so it is tried again in 2 s", self.log.getvalue())

    def test_a_commit_the_forge_does_not_hold_says_so_without_trying_again(self):
        self.slot(0)
        self.answers = [ran(b""), ran(code=128), ran(code=128, err=f"fatal: remote error: object {MERGED} not found")]
        with self.assertRaises(server.Refused) as caught:
            self.fetcher.fetch([{"path": "/w", "forge": {"repo": "programs", "commit": MERGED}}], self.root)
        self.assertIn(f"programs has no commit {MERGED} on any ref: fatal: remote error: object {MERGED} not found", caught.exception.sentence)
        self.assertEqual((len(self.calls), self.paused), (3, []))

    def test_an_http_answer_is_not_tried_again(self):
        self.slot(0)
        self.answers = [ran(code=128, err="fatal: no node of duckpond answered: HTTP status server error (502 Bad Gateway)")]
        with self.assertRaises(server.Refused) as caught:
            self.fetcher.fetch([{"path": "/w", "forge": {"repo": "programs", "commit": "83b18766"}}], self.root)
        self.assertIn("502 Bad Gateway", caught.exception.sentence)
        self.assertEqual((len(self.calls), self.paused), (1, []))

    def test_a_files_read_and_a_blob_get_that_lose_their_connection_are_tried_again(self):
        self.slot(0)
        self.slot(1)
        whole = b"\x05\x00" + b"\xab" * 32 + (3).to_bytes(8, "little") + (0).to_bytes(8, "little") + b"\x03\x00\x00\x00abc" + b"\x01"
        lost = ran(code=1, err=LOST)
        self.answers = [ran(b"minted"), lost, ran(whole), lost, lost, ran(b"program 4\x00\x00asm")]
        inputs = [{"path": "/w/spec.md", "files": {"path": "/docs/spec.md"}}, {"path": "/w/a.wasm", "blob": CHUNKS[0]}]
        self.fetcher.fetch(inputs, self.root)
        self.assertEqual((len(self.calls), self.paused), (6, [1, 1, 2]))
        self.assertEqual(self.calls[1][1], self.calls[2][1])
        self.assertEqual((self.root / "0" / "data").read_bytes(), b"abc")
        self.assertEqual((self.root / "1" / "data").read_bytes(), b"\x00asm")


class ForgeRefs(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.log = self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.base = pathlib.Path(temp.name)
        self.root = self.base / "in"
        (self.root / "0").mkdir(parents=True)
        forge = self.base / "forge"
        forge.mkdir()
        isolated = {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": f"url.{forge.as_uri()}/.insteadOf",
            "GIT_CONFIG_VALUE_0": "duck://duckpond/forge/",
            "GIT_AUTHOR_NAME": "tech",
            "GIT_AUTHOR_EMAIL": "tech@example",
            "GIT_COMMITTER_NAME": "tech",
            "GIT_COMMITTER_EMAIL": "tech@example",
        }
        self.enterContext(unittest.mock.patch.dict(os.environ, isolated))
        self.dev, self.merge = self.forge_with_a_change(forge / "programs")
        self.fetcher = server.Fetcher(["ducktape"], "https://node.example", "duck://duckpond/forge", self.base / "home")

    def git(self, *argv, cwd):
        return subprocess.run(["git", *argv], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, work, name):
        (work / name).write_text(name)
        self.git("add", name, cwd=work)
        self.git("commit", "-q", "-m", name, cwd=work)

    def forge_with_a_change(self, bare):
        work = self.base / "work"
        self.git("init", "-q", "-b", "dev", str(work), cwd=self.base)
        self.commit(work, "base")
        self.git("switch", "-q", "-c", "topic", cwd=work)
        self.commit(work, "topic")
        self.git("switch", "-q", "dev", cwd=work)
        self.commit(work, "dev")
        self.git("merge", "-q", "--no-ff", "topic", "-m", "the change's preview", cwd=work)
        merge = self.git("rev-parse", "HEAD", cwd=work)
        self.git("reset", "-q", "--hard", "HEAD~1", cwd=work)
        self.git("init", "-q", "--bare", str(bare), cwd=self.base)
        self.git("push", "-q", str(bare), "dev", f"{merge}:refs/changes/7/merge", cwd=work)
        return self.git("rev-parse", "dev", cwd=work), merge

    def fetched(self, commit):
        self.fetcher.fetch([{"path": "/w", "forge": {"repo": "programs", "commit": commit}}], self.root)
        data = self.root / "0" / "data"
        return self.git("rev-parse", "HEAD", cwd=data), sorted(path.name for path in data.iterdir() if path.name != ".git")

    def test_a_commit_only_a_changes_merge_ref_holds_is_checked_out_with_the_merged_tree(self):
        self.assertEqual(self.fetched(self.merge), (self.merge, ["base", "dev", "topic"]))
        self.assertIn("is on no branch of duck://duckpond/forge/programs, so it was fetched by its id", self.log.getvalue())

    def test_a_branch_commit_is_checked_out_from_the_clone_alone(self):
        self.assertEqual(self.fetched(self.dev), (self.dev, ["base", "dev"]))
        self.assertEqual(self.log.getvalue().splitlines(), [f"cloned duck://duckpond/forge/programs at {self.dev}"])


class Stopped(unittest.TestCase):
    def test_a_sigterm_ends_the_service_at_once(self):
        with tempfile.TemporaryDirectory() as keys:
            env = dict(os.environ, PODS_OUT_IMAGE="pods:test", PODS_NODE="https://node", PODS_FORGE="duck://chain/forge", PODS_KUBECTL="false", PODS_LISTEN="127.0.0.1:0", PODS_KEYS=keys)
            process = subprocess.Popen([sys.executable, str(HERE / "server.py")], env=env, stderr=subprocess.PIPE, text=True)
            self.addCleanup(process.kill)
            serving = next(line for line in iter(process.stderr.readline, "") if "event=pods_serving" in line)
            self.assertIn("namespace=agents", serving)
            process.send_signal(signal.SIGTERM)
            said = process.stderr.read()
            process.stderr.close()
            self.assertEqual(process.wait(), 0)
            self.assertIn("event=pods_stopping signal=SIGTERM", said)


class Signing(unittest.TestCase):
    def test_a_submit_waits_for_the_block_that_carries_it(self):
        asked = []
        real = server.run
        server.run = lambda argv, stdin=None, env=None: asked.append(argv) or ran(POOLED + b"landed 1 applied \n")
        self.addCleanup(setattr, server, "run", real)
        with tempfile.TemporaryDirectory() as temp:
            keys = pathlib.Path(temp)
            (keys / "key").write_text("k")
            (keys / "password").write_text("p")
            server.Signer(["ducktape"], keys).submit("files", b"op")
        ((ducktape, verb, program, _, wait),) = asked
        self.assertEqual((ducktape, verb, program, wait), ("ducktape", "submit", "files", "--land"))


class Keys(unittest.TestCase):
    def test_a_key_is_held_when_both_its_files_are_mounted(self):
        with tempfile.TemporaryDirectory() as temp:
            keys = pathlib.Path(temp)
            signer = server.Signer(["ducktape"], keys)
            self.assertFalse(signer.held())
            (keys / "key").write_text("k")
            self.assertFalse(signer.held())
            (keys / "password").write_text("p\n")
            self.assertTrue(signer.held())
            self.assertEqual(signer.env()["DUCKTAPE_PASSWORD"], "p")


if __name__ == "__main__":
    unittest.main()
