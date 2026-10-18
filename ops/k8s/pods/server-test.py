"""Exercise the pods service over real HTTP against a recording kubectl, ducktape and kubelet, and its input fetcher against a recording runner; run by `make test`."""
import contextlib
import http.client
import importlib.util
import io
import json
import pathlib
import tempfile
import threading
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("server", HERE / "server.py")
server = importlib.util.module_from_spec(spec)
spec.loader.exec_module(server)
server.say = lambda event, **fields: None

ASKED = {"ducktape-chain": "duckpond", "ducktape-program": "harness", "ducktape-request": "41"}
NOT_FOUND = 'Error from server (NotFound): pods "duckpond-9" not found'
QUOTA = 'Error from server (Forbidden): error when creating "STDIN": pods "duckpond-41" is forbidden: failed quota: agents: must specify limits.cpu for: out,run'
SNAPSHOT = "sha256:" + "ab" * 32
CHUNKS = ["sha256:" + "01" * 32, "sha256:" + "02" * 32, "sha256:" + "03" * 32]
NOTIFY = {"ducktape.industries/notify-channel": "builds"}
ENDED = {"terminated": {"exitCode": 0, "reason": "Completed", "startedAt": "s", "finishedAt": "f"}}
POSTED = bytes.fromhex(
    "04060000006275696c647316000000706f64732d6475636b706f6e642d34312d656e646564010000000002000000050000003c4031"
    "373e01000000030011000000000000002f00000020706f64206475636b706f6e642d343120656e6465643a206578697420636f6465"
    "20302028436f6d706c65746564290000000000"
)
KINDS = b"namespaces  ns  v1  false  Namespace\nclusterroles    rbac.authorization.k8s.io/v1  false  ClusterRole\n"
FETCHING = server.Fetching(image="localhost:5000/pods@sha256:00", node="https://node.example", forge="duck://duckpond/forge")


def ran(out=b"", code=0, err=""):
    return server.Ran(code, out if isinstance(out, bytes) else json.dumps(out).encode(), err)


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


class Signer:
    def __init__(self, keyed=True):
        self.keyed = keyed
        self.puts = []
        self.submits = []
        self.queries = []
        self.put_ran = ran(b"sha256:ab12cd34ef56ab78cd90\nbundle 7 put at block 900\nbundle 7 held since block 901\n")
        self.submitted = ran(b"preconfirmed applied 00ff\n")
        self.head = ran(bytes([0, 1, 0]) + bytes.fromhex("ab" * 32) + b"rest of the row")

    def held(self):
        return self.keyed

    def put(self, paths, kind):
        self.puts.append(([pathlib.Path(path).read_bytes() for path in paths], kind))
        return self.put_ran

    def submit(self, program, payload):
        self.submits.append((program, payload))
        return self.submitted

    def query(self, program, request):
        self.queries.append((program, request))
        return self.head


class Stats:
    def __init__(self):
        self.hosts = []
        self.summaries = {}

    def summary(self, host):
        self.hosts.append(host)
        return self.summaries.get(host, {"pods": []})


class Served(unittest.TestCase):
    def serve(self, signer=None):
        self.kube = Kube()
        self.kube.answer("cluster_kinds", ran(KINDS))
        self.signer = signer or Signer()
        self.stats = Stats()
        service = server.Service(self.kube, self.signer, FETCHING, self.stats)
        quiet = type("Quiet", (server.handler(service),), {"log_message": lambda *_: None})
        self.http = server.ThreadingHTTPServer(("127.0.0.1", 0), quiet)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        self.addCleanup(self.http.server_close)
        self.addCleanup(self.http.shutdown)

    def setUp(self):
        self.serve()

    def call(self, method, path, body=None, headers=ASKED):
        connection = http.client.HTTPConnection(*self.http.server_address)
        payload = None if body is None else json.dumps(body).encode()
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
        self.assertEqual(body["restartPolicy"], "Never")
        self.assertNotIn("resources", body)
        self.assertNotIn("initContainers", body)
        run, out = body["containers"]
        self.assertEqual((run["name"], run["image"], run["command"], run["args"], run["workingDir"]), ("run", "toolchain", ["make"], ["wasm"], "/w"))
        self.assertEqual(run["env"], [{"name": "A", "value": "1"}])
        self.assertNotIn("resources", run)
        self.assertFalse(run["securityContext"]["allowPrivilegeEscalation"])
        self.assertEqual((out["name"], out["image"], out["command"]), ("out", FETCHING.image, ["sleep", "infinity"]))

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

    def test_kubernetes_refusing_a_pod_without_limits_passes_its_reason_through(self):
        self.kube.answer("create", ran(code=1, err=QUOTA))
        answer = self.refused("POST", "/v1/pods", {"image": "toolchain"}, 403, [])
        self.assertIn("must specify limits.cpu for: out,run", answer["error"])

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
        self.assertEqual([volume["name"] for volume in body["volumes"]], ["out", "in-0", "in-1", "in-2"])

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

    def test_a_path_outside_the_routes_is_not_found(self):
        self.assertEqual(self.call("GET", "/v2/pods")[0], 404)
        self.assertEqual(self.call("PUT", "/v1/pods")[0], 501)
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

    def test_a_held_blob_is_posted_where_the_pod_asked_mentioning_its_requester(self):
        self.call("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program", "container": "run"})
        self.assertEqual(self.kube.calls[1][2], "run")
        ((program, payload),) = self.signer.submits
        self.assertEqual(program, "chat")
        line = "pod duckpond-41: blob sha256:ab12cd34ef56ab78cd90 is held since block 901"
        self.assertEqual(payload, server.post_message("builds", "pods-duckpond-41-blob-ab12cd34ef56ab78", 17, line, None))

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

    def test_an_output_is_put_as_chunks_and_committed_onto_files_head_as_pods(self):
        status, answer = self.call("POST", "/v1/pods/duckpond-41/file", {"path": "/out/report.md", "to": "/reports/41.md", "message": "build report"})
        self.assertEqual(status, 200, answer)
        self.assertEqual(answer, {"written": "/reports/41.md", "size": 10, "chunks": CHUNKS, "base": SNAPSHOT, "preconfirmed": True})
        self.assertEqual(self.signer.puts, [([b"abcd", b"efgh", b"ij"], "chunk")])
        self.assertEqual(self.signer.queries, [("files", bytes([0]))])
        (program, payload), post = self.signer.submits
        self.assertEqual(program, "files")
        self.assertEqual(payload, server.files_commit(SNAPSHOT, "build report", "/reports/41.md", 10, CHUNKS))
        self.assertEqual(post[0], "chat")
        line = "pod duckpond-41: wrote /reports/41.md into files (10 bytes)"
        self.assertEqual(post[1], server.post_message("builds", "pods-duckpond-41-file-0101010101010101", 17, line, None))

    def test_files_refusing_the_write_passes_its_reason_through(self):
        self.signer.submitted = ran(b"preconfirmed rejected unauthorized: /reports is written by its write list only\n")
        answer = self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/r", "to": "/reports/41.md"}, 403, [])
        self.assertEqual(answer["error"], "files refused the write: unauthorized: /reports is written by its write list only")
        self.assertEqual(len(self.signer.submits), 1)

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


class Keyless(Served):
    def setUp(self):
        self.serve(Signer(keyed=False))

    def test_a_service_without_a_key_says_so_and_reads_nothing(self):
        self.refused("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/a", "kind": "program"}, 503, ["holds no key to put blobs"])
        self.refused("POST", "/v1/pods/duckpond-41/file", {"path": "/out/a", "to": "/a"}, 503, ["holds no key to write files"])
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
        self.kube.answer("trial", ran(code=1, err=QUOTA))
        answer = self.refused("POST", "/v1/manifests", {"manifests": [deployment()]}, 403, ["manifest 0 (Deployment web)", "must specify limits.cpu"])
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


class Usage(Served):
    def test_usage_reports_the_quota_and_each_requesters_share_and_measure(self):
        building = pod_json(name="b", phase="Running", requester="15")
        building["spec"]["resources"] = {"limits": {"cpu": "2", "memory": "4Gi"}}
        building["status"]["hostIP"] = "192.168.75.48"
        done = pod_json(name="d", phase="Succeeded", requester="15")
        done["spec"]["resources"] = {"limits": {"cpu": "8", "memory": "8Gi"}}
        web = pod_json(name="w", phase="Running", requester=None)
        web["spec"]["containers"] = [{"name": "a", "resources": {"limits": {"cpu": "500m", "memory": "1Gi"}}}, {"name": "b", "resources": {"limits": {"cpu": "250m", "memory": "512Mi"}}}]
        web["spec"]["initContainers"] = [{"name": "i", "resources": {"limits": {"cpu": "1", "memory": "1Mi"}}}]
        web["status"]["hostIP"] = "192.168.75.48"
        claim = {"metadata": {"name": "cache", "labels": {"ducktape.industries/requester": "15"}}, "spec": {"resources": {"requests": {"storage": "10Gi"}}}}
        quota = {"metadata": {"name": "agents"}, "status": {"hard": {"limits.cpu": "48"}, "used": {"limits.cpu": "2750m"}}}
        self.kube.answer("list", ran({"items": [building, done, web]}))
        self.kube.answer("claims", ran({"items": [claim]}))
        self.kube.answer("quotas", ran({"items": [quota]}))
        self.stats.summaries["192.168.75.48"] = {
            "pods": [
                {"podRef": {"name": "b", "namespace": "agents"}, "cpu": {"usageNanoCores": 900}, "memory": {"workingSetBytes": 1000}, "ephemeral-storage": {"usedBytes": 50}},
                {"podRef": {"name": "node", "namespace": "duckpond"}, "cpu": {"usageNanoCores": 1}},
            ]
        }
        status, answer = self.call("GET", "/v1/usage")
        self.assertEqual(status, 200)
        self.assertEqual(self.stats.hosts, ["192.168.75.48"])
        self.assertEqual(answer["quota"], {"agents": {"hard": {"limits.cpu": "48"}, "used": {"limits.cpu": "2750m"}}})
        first, second = answer["requesters"]
        self.assertEqual(
            first,
            {
                "requester": 15,
                "pods": 2,
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
        self.assertEqual([row["pod"] for row in answer["pods"]], ["b", "d", "w"])
        self.assertEqual(answer["claims"], [{"claim": "cache", "requester": 15, "storage_requested_bytes": 10 * 2**30}])

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
        self.assertEqual(self.kube.calls, [("annotate", "duckpond-41", "ducktape.industries/notified", "ended")])
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes={**NOTIFY, "ducktape.industries/notified": "ended"}))
        self.assertEqual(len(self.signer.submits), 1)

    def test_a_pod_still_running_or_asking_for_no_post_is_not_posted(self):
        self.notifier.seen(pod_json(state={"running": {}}, phase="Running", notes=NOTIFY))
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded"))
        self.assertEqual((self.signer.submits, self.kube.calls), ([], []))

    def test_a_pod_whose_inputs_failed_is_posted_with_why(self):
        failed = {"terminated": {"exitCode": 1, "message": "cloning\nduck://duckpond/forge/x was not cloned: not found\n"}}
        pod = pod_json(phase="Failed", init=failed, notes=NOTIFY)
        self.assertEqual(server.ended_line(pod), "pod duckpond-41 ended: fetching its inputs failed, exit code 1: duck://duckpond/forge/x was not cloned: not found")

    def test_a_pod_the_kubelet_failed_is_posted_with_its_reason(self):
        self.assertEqual(server.ended_line(pod_json(phase="Failed", reason="Evicted")), "pod duckpond-41 ended: Evicted")

    def test_a_refused_post_is_not_marked_and_a_repeated_one_is(self):
        self.signer.submitted = ran(b"preconfirmed rejected unauthorized: pods may not post in builds\n")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.kube.calls, [])
        self.signer.submitted = ran(b"preconfirmed rejected already_exists: message pods-duckpond-41-ended exists\n")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(len(self.kube.calls), 1)

    def test_a_post_that_was_not_sent_is_not_marked(self):
        self.signer.submitted = ran(code=1, err="node unreachable")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.kube.calls, [])

    def test_without_a_key_nothing_is_posted_or_marked(self):
        self.signer.keyed = False
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual((self.signer.submits, self.kube.calls), ([], []))


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

    def test_receipts_read_applied_rejected_and_failed(self):
        self.assertEqual(server.receipt(ran(b"preconfirmed applied 00ff\n")), ("applied", "00ff", ""))
        self.assertEqual(server.receipt(ran(b"preconfirmed rejected not_found: no channel x\n")), ("rejected", "not_found", "no channel x"))
        self.assertEqual(server.receipt(ran(code=1, err="refused")), ("failed", "", "refused"))

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
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.root = pathlib.Path(temp.name) / "in"
        self.home = pathlib.Path(temp.name) / "home"
        self.calls = []
        self.answers = []
        self.fetcher = server.Fetcher(["ducktape"], "https://node.example", "duck://duckpond/forge", self.home, runner=self.recorded)

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
        clone, checkout = [call[0] for call in self.calls]
        git = ["git", "-c", "duck.duckpond.node=https://node.example"]
        self.assertEqual(clone, [*git, "clone", "--no-checkout", "duck://duckpond/forge/programs", str(self.root / "0" / "data")])
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

    def test_a_fetch_that_fails_says_what_and_why(self):
        self.slot(0)
        self.answers = [ran(code=128, err="fatal: repository not found")]
        with self.assertRaises(server.Refused) as caught:
            self.fetcher.fetch([{"path": "/w", "forge": {"repo": "nope", "commit": "1"}}], self.root)
        self.assertIn("duck://duckpond/forge/nope was not cloned: fatal: repository not found", caught.exception.sentence)


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
