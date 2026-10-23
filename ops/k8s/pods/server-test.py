"""Exercise the pods service over real HTTP against a recording kubectl, ducktape and kubelet, and its input fetcher against a recording runner; run by `make test`."""
import ast
import base64
import contextlib
import datetime
import http.client
import socket
import importlib.util
import io
import itertools
import tarfile
import json
import os
import pathlib
import queue
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import unittest.mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

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
DELETED_MARK = "ducktape.industries/deleted-by-requester"
SIGTERMED = {"terminated": {"exitCode": 143, "reason": "Error", "startedAt": "s", "finishedAt": "2026-10-22T09:30:31Z"}}
FETCHED = {"terminated": {"exitCode": 0, "reason": "Completed", "startedAt": "s", "finishedAt": "2026-10-22T09:29:00Z"}}
ASKED_AT = "2026-10-22T09:30:30Z"
DELETE_MARKED = ("annotate", "duckpond-41", DELETED_MARK, "uid-duckpond-41 2026-10-22T15:00:00Z")
ENDED = {"terminated": {"exitCode": 0, "reason": "Completed", "startedAt": "s", "finishedAt": "f"}}
RUNNING = {"running": {"startedAt": "s"}}
POSTED = bytes.fromhex(
    "1a060000006275696c647316000000706f64732d6475636b706f6e642d34312d656e64656400"
    "03000000706f640b0000006475636b706f6e642d34310f000000706f64206475636b706f6e642d3431"
    "00021e000000656e6465643a206578697420636f646520302028436f6d706c6574656429"
    "010000001100000000000000"
)


def notified(channel, message_id, requester, line, thread):
    name, _, text = line.partition(" \u00b7 ")
    return server.notice_message(channel, message_id, thread, server.line_said(name, text, requester))
KINDS = b"namespaces  ns  v1  false  Namespace\nclusterroles    rbac.authorization.k8s.io/v1  false  ClusterRole\n"
FETCHING = server.Fetching(image="localhost:5000/pods@sha256:00", node="https://node.example", forge="duck://duckpond/forge", block_ms=1000)


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
UNSCHEDULABLE = "0/1 nodes are available: 1 Insufficient memory. no new claims to deallocate, preemption: not eligible due to preemptionPolicy=Never."


def unschedulable(pod, reason="Unschedulable"):
    pod["status"]["conditions"] = [{"type": "PodScheduled", "status": "False", "reason": reason, "message": UNSCHEDULABLE}]
    return pod


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
        "metadata": {"name": name, "uid": f"uid-{name}", "creationTimestamp": "2026-10-18T12:00:00Z", "labels": labels, "annotations": notes or {}},
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
        self.kept = {}
        self.processes = []

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

    def exec(self, name, container, command):
        if self.gate is not None:
            self.gate.wait()
        return self.ran("exec", name, container, command)

    def keep(self, name, container, path, data):
        return self.ran("keep", name, container, path, data)

    def exec_process(self, name, container, command):
        process = ExecProcess(self.ran("exec", name, container, command), self.gate is not None)
        self.processes.append(process)
        return process

    def annotate(self, name, key, value):
        return self.ran("annotate", name, key, value)

    def unannotate(self, name, key):
        return self.ran("unannotate", name, key)

    def apply(self, objects, dry):
        return self.ran("apply", objects, dry)

    def held(self, objects):
        return self.ran("held", objects)

    def trial(self, pod):
        return self.ran("trial", pod)

    def objects(self, kind, name):
        return self.ran("objects", kind, name)

    def claims(self):
        return self.ran("claims")

    def quotas(self):
        return self.ran("quotas")

    def cluster_kinds(self):
        return self.ran("cluster_kinds")

    def watch_pod(self, name):
        self.calls.append(("watch_pod", name))
        return self.watches.pop(0)

    def follow(self, name):
        self.follows.append(name)
        return Log(self.logs_of.get(name, b""))

    def kept_load(self):
        return dict(self.kept)

    def kept_change(self, data):
        for key, value in data.items():
            if value is None:
                self.kept.pop(key, None)
            else:
                self.kept[key] = value
        return ran()

    def watch(self):
        raise Unwatched()


class Unwatched(Exception):
    pass


def lost_id(channel, run, pod="duckpond-41"):
    digest = server.hashlib.sha256(f"{channel}\n{run}".encode()).hexdigest()
    return f"pods-{pod}-lost-{digest}"


class Log:
    def __init__(self, data):
        self.stdout = io.BytesIO(data)

    def wait(self):
        return 0


class ExecProcess:
    def __init__(self, answer, held):
        self.answer = answer
        self.killed = False
        self.returncode = None
        self.ended = threading.Event()
        if not held:
            self.ended.set()

    def communicate(self):
        self.ended.wait()
        if self.killed:
            self.returncode = -9
            return b"", b""
        self.returncode = self.answer.code
        return self.answer.out, self.answer.err.encode()

    def poll(self):
        return self.returncode

    def kill(self):
        self.killed = True
        self.ended.set()


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
        self.messages = set()
        self.put_ran = ran(b"sha256:ab12cd34ef56ab78cd90\nbundle 7 put at block 900\nbundle 7 held since block 901\n")
        self.submitted = ran(POOLED + b"landed 960 applied 00ff\n")
        self.head = ran(bytes([0, 1, 0]) + bytes.fromhex("ab" * 32) + b"rest of the row")
        self.request = ran(OPEN)
        self.sender = ran(server.u8(server.EGRESS_SENDER) + server.u8(0))
        self.senders = []
        self.ducktape = ["ducktape", "--workspace", "/home/pods", "--node", "https://node.example"]

    def held(self):
        return self.keyed

    def env(self):
        return {"DUCKTAPE_USER_KEY": "/keys/key", "DUCKTAPE_PASSWORD": "p"}

    def put(self, paths, kind):
        self.puts.append(([pathlib.Path(path).read_bytes() for path in paths], kind))
        return taken(self, "put_ran")

    def submit(self, program, payload):
        self.submits.append((program, payload))
        return taken(self, "submitted")

    def query(self, program, request):
        if program == "chat":
            self.asked.append(request)
            if request[:1] == server.u8(2):
                return ran(server.u8(2) + server.u8(1 if request[5:].decode() in self.messages else 0))
            answer = self.rooms.get(request, room())
            return answer if isinstance(answer, server.Ran) else ran(answer)
        if program == "egress" and request[:1] == server.u8(server.EGRESS_SENDER):
            self.senders.append(request)
            return taken(self, "sender")
        self.queries.append((program, request))
        return taken(self, "request" if program == "egress" else "head")


class Stats:
    def __init__(self):
        self.hosts = []
        self.summaries = {}

    def summary(self, host):
        self.hosts.append(host)
        return self.summaries.get(host, {"pods": []})


QUANTITY_NAME = "io.k8s.apimachinery.pkg.api.resource.Quantity"
CORE = "io.k8s.api.core.v1."
ZEROS = "1" + "0" * 10**6
SEVENS = "7" * 10**6
MOST = "9223372036854775807"


def ref(name, wrapped=False):
    pointer = {"$ref": f"#/components/schemas/{name}"}
    return {"allOf": [pointer], "default": {}} if wrapped else pointer


def kind_of(group, version, kind, **properties):
    known = {"apiVersion": {"type": "string"}, "kind": {"type": "string"}, "metadata": {"type": "object"}, **properties}
    return {"type": "object", "x-kubernetes-group-version-kind": [{"group": group, "version": version, "kind": kind}], "properties": known}


def amounts():
    return {"type": "object", "additionalProperties": ref(QUANTITY_NAME)}


def operations(name, *methods):
    answer = {"200": {"content": {"application/json": {"schema": ref(name)}}}}
    patches = ("application/json-patch+json", "application/merge-patch+json", "application/strategic-merge-patch+json", "application/apply-patch+yaml")
    patch = {media: {"schema": ref("io.k8s.apimachinery.pkg.apis.meta.v1.Patch")} for media in patches}
    return {method: {"requestBody": {"content": patch if method == "patch" else {"*/*": {"schema": ref(name)}}}, "responses": answer} for method in methods}


def unbodied(*methods):
    return {method: {"responses": {"200": {"content": {"*/*": {"schema": {"type": "string"}}}}}} for method in methods}


POD_SCHEMAS = {
    QUANTITY_NAME: {"type": "string"},
    CORE + "ResourceRequirements": {"type": "object", "properties": {"limits": amounts(), "requests": amounts()}},
    CORE + "EnvVar": {"type": "object", "properties": {"name": {"type": "string"}, "value": {"type": "string"}}},
    CORE + "Container": {
        "type": "object",
        "properties": {"name": {"type": "string"}, "env": {"type": "array", "items": ref(CORE + "EnvVar", True)}, "resources": ref(CORE + "ResourceRequirements", True)},
    },
    CORE + "EmptyDirVolumeSource": {"type": "object", "properties": {"medium": {"type": "string"}, "sizeLimit": {"allOf": [ref(QUANTITY_NAME)]}}},
    CORE + "Volume": {"type": "object", "properties": {"name": {"type": "string"}, "emptyDir": ref(CORE + "EmptyDirVolumeSource", True)}},
    CORE + "PodSpec": {
        "type": "object",
        "properties": {
            "containers": {"type": "array", "items": ref(CORE + "Container")},
            "volumes": {"type": "array", "items": ref(CORE + "Volume", True)},
            "resources": ref(CORE + "ResourceRequirements", True),
            "overhead": amounts(),
        },
    },
    CORE + "PodTemplateSpec": {"type": "object", "properties": {"metadata": {"type": "object"}, "spec": ref(CORE + "PodSpec", True)}},
    "io.k8s.example.Circle": {"type": "object", "properties": {"next": ref("io.k8s.example.Circle"), "all": {"type": "array", "items": ref("io.k8s.example.Circle")}}},
    "io.k8s.example.Spiral": {"type": "object", "properties": {"next": ref("io.k8s.example.Spiral", True), "size": ref(QUANTITY_NAME)}},
}
CORE_V1 = {
    "components": {
        "schemas": {
            **POD_SCHEMAS,
            CORE + "Pod": kind_of("", "v1", "Pod", spec=ref(CORE + "PodSpec", True)),
            CORE + "ConfigMap": kind_of("", "v1", "ConfigMap", data={"type": "object", "additionalProperties": {"type": "string"}}),
            CORE + "Service": kind_of("", "v1", "Service", spec={"type": "object"}),
            CORE + "ServiceAccount": kind_of("", "v1", "ServiceAccount"),
            CORE + "VolumeResourceRequirements": {"type": "object", "properties": {"limits": amounts(), "requests": amounts()}},
            CORE + "PersistentVolumeClaimSpec": {"type": "object", "properties": {"resources": ref(CORE + "VolumeResourceRequirements", True)}},
            CORE + "PersistentVolumeClaim": kind_of("", "v1", "PersistentVolumeClaim", spec=ref(CORE + "PersistentVolumeClaimSpec", True)),
            CORE + "ResourceQuotaSpec": {"type": "object", "properties": {"hard": amounts()}},
            CORE + "ResourceQuota": kind_of("", "v1", "ResourceQuota", spec=ref(CORE + "ResourceQuotaSpec", True)),
            "io.k8s.api.policy.v1.Eviction": kind_of("policy", "v1", "Eviction"),
            "io.k8s.api.authentication.v1.TokenRequest": kind_of("authentication.k8s.io", "v1", "TokenRequest", spec={"type": "object"}),
            CORE + "Binding": kind_of("", "v1", "Binding", target={"type": "object"}),
        }
    },
    "paths": {
        "/api/v1/namespaces/{namespace}/pods": operations(CORE + "Pod", "post"),
        "/api/v1/namespaces/{namespace}/pods/{name}": operations(CORE + "Pod", "put", "patch"),
        "/api/v1/namespaces/{namespace}/pods/{name}/resize": operations(CORE + "Pod", "put", "patch"),
        "/api/v1/namespaces/{namespace}/pods/{name}/eviction": operations("io.k8s.api.policy.v1.Eviction", "post"),
        "/api/v1/namespaces/{namespace}/configmaps": operations(CORE + "ConfigMap", "post"),
        "/api/v1/namespaces/{namespace}/resourcequotas/{name}": operations(CORE + "ResourceQuota", "put", "patch"),
        "/api/v1/namespaces/{namespace}/configmaps/{name}": operations(CORE + "ConfigMap", "put", "patch"),
        "/api/v1/namespaces/{namespace}/services/{name}": operations(CORE + "Service", "put", "patch"),
        "/api/v1/namespaces/{namespace}/pods/{name}/binding": operations(CORE + "Binding", "post"),
        "/api/v1/namespaces/{namespace}/serviceaccounts/{name}/token": operations("io.k8s.api.authentication.v1.TokenRequest", "post"),
        "/api/v1/namespaces/{namespace}/pods/{name}/exec": unbodied("get", "post"),
        "/api/v1/namespaces/{namespace}/pods/{name}/proxy": unbodied("post", "put", "patch"),
        "/api/v1/namespaces/{namespace}/pods/{name}/proxy/{path}": unbodied("post", "put", "patch"),
    },
}
APPS_V1 = {
    "components": {
        "schemas": {
            **POD_SCHEMAS,
            "io.k8s.api.apps.v1.DeploymentSpec": {"type": "object", "properties": {"replicas": {"type": "integer"}, "template": ref(CORE + "PodTemplateSpec", True)}},
            "io.k8s.api.apps.v1.Deployment": kind_of("apps", "v1", "Deployment", spec=ref("io.k8s.api.apps.v1.DeploymentSpec", True)),
            "io.k8s.api.autoscaling.v1.Scale": kind_of("autoscaling", "v1", "Scale", spec={"type": "object"}),
        }
    },
    "paths": {
        "/apis/apps/v1/namespaces/{namespace}/deployments": operations("io.k8s.api.apps.v1.Deployment", "post"),
        "/apis/apps/v1/namespaces/{namespace}/deployments/{name}": operations("io.k8s.api.apps.v1.Deployment", "put", "patch"),
        "/apis/apps/v1/namespaces/{namespace}/deployments/{name}/scale": operations("io.k8s.api.autoscaling.v1.Scale", "put", "patch"),
    },
}
POLICY_V1 = {"components": {"schemas": {"io.k8s.api.policy.v1.Eviction": kind_of("policy", "v1", "Eviction")}}, "paths": {}}
PUBLISHED = {"api/v1": ("c0", CORE_V1), "apis/apps/v1": ("a0", APPS_V1), "apis/policy/v1": ("p0", POLICY_V1)}


class Api:
    def __init__(self):
        self.calls = []
        self.answers = []
        self.published = dict(PUBLISHED)
        self.read = []

    def openapi(self, path, query):
        if path == server.OPENAPI:
            index = {name: {"serverRelativeURL": f"{server.OPENAPI}/{name}?hash={held}"} for name, (held, _) in self.published.items()}
            return answered(200, {"paths": index})
        held, document = self.published.get(path.removeprefix(server.OPENAPI + "/"), (None, None))
        return answered(200, document) if query == f"hash={held}" else answered(404, {})

    def call(self, method, path, query, body, kind):
        if path.startswith(server.OPENAPI):
            self.read.append(path + (f"?{query}" if query else ""))
            return self.openapi(path, query)
        self.calls.append((method, path, query, body, kind))
        return self.answers.pop(0) if self.answers else server.Answered(200, "application/json", b"{}")


class Served(unittest.TestCase):
    def serve(self, signer=None, runners=None, building=None):
        self.kube = Kube()
        self.kube.answer("cluster_kinds", ran(KINDS))
        self.signer = signer or Signer()
        self.stats = Stats()
        self.api = Api()
        service = self.service = server.Service(self.kube, self.signer, FETCHING, self.stats, self.api, runners, building=building)
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
    def stream(self, path, method="GET", body=None, headers=ASKED):
        connection = http.client.HTTPConnection(*self.http.server_address)
        self.addCleanup(connection.close)
        connection.request(method, path, body=None if body is None else json.dumps(body).encode(), headers=headers)
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

    def told(self, response):
        said = [self.past_alive(response)]
        while said[-1][0] not in ("ended", "refused"):
            said.append(self.past_alive(response))
        return said


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

    def test_a_wait_stream_says_the_same_request_may_pick_it_up_again(self):
        self.kube.answer("get", ran(pod_json(state=ENDED, phase="Succeeded")))
        response = self.stream("/v1/pods/duckpond-41/wait?every=30", headers={**ASKED, "ducktape-heard": "3"})
        self.assertEqual(response.getheader("ducktape-reattach"), "request")
        kind, data = self.event(response)
        self.assertEqual((kind, data["exit_code"]), ("ended", 0))

    def test_a_wait_names_every_in_whole_seconds_from_one(self):
        for query, words in (("", "names every"), ("?every=0", "from 1"), ("?every=1.5", "whole number"), ("?every=5&tail=3", "no tail; it takes every and log")):
            self.refused("GET", f"/v1/pods/duckpond-41/wait{query}", None, 400, [words])
        self.assertNotIn("get", self.kube.verbs())

    def test_a_wait_on_no_pod_is_refused_before_any_stream(self):
        self.kube.answer("get", ran(code=1, err=NOT_FOUND))
        self.refused("GET", "/v1/pods/duckpond-9/wait?every=5", None, 404, ["duckpond-9"])

    def test_a_wait_takes_every_from_its_every_header(self):
        self.kube.answer("get", ran(pod_json(state=ENDED, phase="Succeeded")))
        response = self.stream("/v1/pods/duckpond-41/wait", headers={**ASKED, "every": "30"})
        self.assertEqual(response.status, 200)
        kind, data = self.event(response)
        self.assertEqual((kind, data["exit_code"]), ("ended", 0))

    def test_a_waits_every_header_is_a_whole_number_of_seconds_from_one(self):
        for every, words in (("0", "from 1"), ("1.5", "whole number"), ("x", "the every header")):
            code, answer = self.call("GET", "/v1/pods/duckpond-41/wait", None, {**ASKED, "every": every})
            self.assertEqual(code, 400, answer)
            self.assertIn(words, answer["error"])
        self.assertNotIn("get", self.kube.verbs())

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

    def test_a_wait_naming_a_log_carries_its_size_and_last_line_in_each_alive_beat(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        unread = "sh: can't open /out/log: no such file\ncommand terminated with exit code 2"
        self.kube.answer("exec", ran(b"5120\n$ make check\n"), ran(b"  9000\ntest words ... ok"), ran(code=2, err=unread))
        watch = Watch()
        self.kube.watches.append(watch)
        watch.send("ADDED", pod_json(state=RUNNING, phase="Running"))
        response = self.stream("/v1/pods/duckpond-41/wait?every=1", headers={**ASKED, "log": "/out/log"})
        first = {"log": {"bytes": 5120, "line": "$ make check"}}
        second = {"log": {"bytes": 9000, "line": "test words ... ok"}}
        unread = {"log": {"error": "sh: can't open /out/log: no such file"}}
        said = []
        while unread not in said:
            kind, data = self.event(response)
            self.assertEqual(kind, "alive")
            if data not in ({}, *said[-1:]):
                self.assertEqual(data, [first, second, unread][len(said)])
                said.append(data)
        self.assertEqual(said, [first, second, unread])
        watch.send("MODIFIED", pod_json(state=ENDED, phase="Succeeded"))
        kind, data = self.past_alive(response)
        self.assertEqual((kind, data["exit_code"]), ("ended", 0))
        read = ("exec", "duckpond-41", "out", ["sh", "-c", server.LOG_READ, "sh", "/out/log"])
        execs = [call for call in self.kube.calls if call[0] == "exec"]
        self.assertGreaterEqual(len(execs), 3)
        self.assertTrue(all(call == read for call in execs), execs)

    def test_a_log_read_that_hangs_holds_back_no_beat_and_no_end(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.gate = threading.Event()
        self.addCleanup(self.kube.gate.set)
        watch = Watch()
        self.kube.watches.append(watch)
        watch.send("ADDED", pod_json(state=RUNNING, phase="Running"))
        response = self.stream("/v1/pods/duckpond-41/wait?every=1", headers={**ASKED, "log": "/out/log"})
        self.assertEqual(self.event(response), ("alive", {}))
        self.assertEqual(self.event(response), ("alive", {}))
        watch.send("MODIFIED", pod_json(state=ENDED, phase="Succeeded"))
        kind, data = self.past_alive(response)
        self.assertEqual((kind, data["exit_code"]), ("ended", 0))
        self.assertEqual([process.killed for process in self.kube.processes], [True], "the one read in flight ends with its wait")

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

    def test_a_wait_on_a_pod_the_scheduler_cannot_place_notes_why_once_while_it_waits(self):
        self.kube.answer("get", ran(pod_json()))
        watch = Watch()
        self.kube.watches.append(watch)
        watch.send("ADDED", unschedulable(pod_json()))
        watch.send("MODIFIED", unschedulable(pod_json()))
        response = self.stream("/v1/pods/duckpond-41/wait?every=1")
        self.assertEqual(self.past_alive(response), ("note", {"line": f"pod duckpond-41 is pending, waiting to be placed: {UNSCHEDULABLE}"}))
        watch.send("MODIFIED", unschedulable(pod_json()))
        watch.send("MODIFIED", pod_json(state=ENDED, phase="Succeeded"))
        kind, data = self.past_alive(response)
        self.assertEqual((kind, data["exit_code"]), ("ended", 0))

    def test_a_wait_on_a_pod_only_gated_or_not_yet_placed_notes_nothing(self):
        for pending in (unschedulable(pod_json(), reason="SchedulingGated"), pod_json()):
            with self.subTest(conditions=pending["status"].get("conditions")):
                self.kube.answer("get", ran(pod_json()))
                watch = Watch()
                self.kube.watches = [watch]
                watch.send("ADDED", pending)
                watch.send("MODIFIED", pod_json(state=ENDED, phase="Succeeded"))
                said = self.told(self.stream("/v1/pods/duckpond-41/wait?every=30"))
                self.assertEqual([kind for kind, _ in said], ["ended"], said)


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

    def test_a_streamed_exec_past_its_inline_bytes_ends_with_where_its_output_was_kept(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.answer("exec", ran(b"built\n"))
        response = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 30, "inline": 2})
        kept = {"stdout": {"path": "/tmp/pods-exec/41/stdout", "bytes": 6, "lines": 1}, "stderr": {"path": "/tmp/pods-exec/41/stderr", "bytes": 0, "lines": 0}}
        self.assertEqual(self.past_alive(response), ("ended", {"exit_code": 0, "kept": kept}))

    def test_an_exec_stream_is_not_marked_for_another_pick_up(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.answer("exec", ran(b"built\n"))
        response = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 30})
        self.assertIsNone(response.getheader("ducktape-reattach"))
        self.assertEqual(self.past_alive(response)[0], "ended")

    def test_an_exec_without_every_still_answers_whole(self):
        self.kube.answer("exec", ran(b"built\n"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]})
        self.assertEqual((status, answer), (200, {"exit_code": 0, "stdout": "built\n", "stderr": ""}))
        self.assertNotIn("get", self.kube.verbs())

    def test_an_exec_with_an_every_header_streams_as_one_with_every_does(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running")))
        self.kube.answer("exec", ran(b"built\n"))
        response = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"]}, {**ASKED, "every": "30"})
        self.assertEqual((response.status, response.getheader("content-type")), (200, "text/event-stream"))
        self.assertEqual(self.past_alive(response), ("ended", {"exit_code": 0, "stdout": "built\n", "stderr": ""}))
        self.assertEqual(response.read(), b"")
        self.assertEqual(self.kube.calls[-1], ("exec", "duckpond-41", None, ["make"]))

    def test_an_execs_every_header_is_a_whole_number_of_seconds_from_one(self):
        for every, words in (("0", "from 1"), ("1.5", "whole number"), ("x", "the every header")):
            code, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"]}, {**ASKED, "every": every})
            self.assertEqual(code, 400, answer)
            self.assertIn(words, answer["error"])
        self.assertNotIn("exec", self.kube.verbs())

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
        self.signer.sender = sent_by(17)
        spec = {"image": "toolchain", "command": ["make"], "args": ["wasm"], "env": {"A": "1"}, "workdir": "/w"}
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
        self.refused("POST", "/v1/pods", {"image": "t", "resources": {"limits": {"memory": "48GB"}}}, 400, ["resources' limits memory is not a Kubernetes quantity"])
        self.assertNotIn("create", self.kube.verbs())

    def test_resources_reach_kubernetes_as_kubernetes_writes_them(self):
        self.kube.answer("create", ran(pod_json()))
        resources = {"limits": {"cpu": "1e-10000000", "memory": ZEROS}, "requests": {"cpu": "0.5", "memory": ZEROS + "e-1000000"}}
        status, _ = self.call("POST", "/v1/pods", {"image": "toolchain", "resources": resources})
        self.assertEqual(status, 201)
        self.assertEqual(self.created()["spec"]["resources"], {"limits": {"cpu": "1e-9", "memory": MOST}, "requests": {"cpu": "500m", "memory": "1"}})

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
        self.assertEqual(env["PODS_BLOCK_MS"], "1000")
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
        sizes = {
            "48Gi": "51539607552",
            "512M": "512000000",
            "1e9": "1000000000",
            "1.5Gi": "1610612736",
            "+64Mi": "67108864",
            ".5Ti": "549755813888",
            "1000000000000000000n": "1000000000",
            "1000000000000u": "1000000",
            " 48Gi": "51539607552",
            "1500m": "2",
            "1e-10000000": "1",
            "1e2147483647": MOST,
            ZEROS: MOST,
            ZEROS + "e-1000000": "1",
            SEVENS + "e-1000000": "1",
        }
        inputs = [{"path": f"/scratch/{index}", "scratch": {"medium": "Memory", "size": size}} for index, size in enumerate(sizes)]
        status, _ = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs})
        self.assertEqual(status, 201)
        volumes = self.created()["spec"]["volumes"][1:]
        self.assertEqual(volumes, [{"name": f"in-{index}", "emptyDir": {"medium": "Memory", "sizeLimit": limit}} for index, limit in enumerate(sizes.values())])

    def test_a_scratch_without_a_size_has_no_size_limit(self):
        self.kube.answer("create", ran(pod_json()))
        self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": [{"path": "/var/tmp/target", "scratch": {"medium": "Memory"}}]})
        self.assertNotIn("sizeLimit", self.created()["spec"]["volumes"][1]["emptyDir"])

    def test_a_scratch_size_is_a_kubernetes_quantity_above_zero(self):
        malformed = ("", "48GB", "48gi", "Gi", "1.2.3Gi", "48 Gi", "1e", "1e3Gi", "0x10", "forty", "48Gi\u001f")
        for size in malformed:
            self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/s", "scratch": {"medium": "Memory", "size": size}}]}, 400, ["is not a Kubernetes quantity"])
        for size in ("1e2147483648", "1e-2147483649", "1e9223372036854775808", "1e" + "9" * 100000):
            self.refused("POST", "/v1/pods", {"image": "t", "inputs": [{"path": "/s", "scratch": {"medium": "Memory", "size": size}}]}, 400, ["outside -2^31..2^31-1"])
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

    def test_a_scratch_in_a_pod_without_a_memory_request_or_limit_answers_no_warnings(self):
        self.kube.answer("create", ran(pod_json()))
        inputs = [{"path": "/work", "scratch": {"medium": "Memory", "size": "80Gi"}}, {"path": "/unsized", "scratch": {"medium": "Memory"}}]
        for resources in ({}, {"resources": {"limits": {"cpu": "8"}, "requests": {"cpu": "2"}}}):
            status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, **resources})
            self.assertEqual(status, 201)
            self.assertEqual(list(answer), ["pod"])

    def test_scratches_not_below_the_memory_limit_are_made_with_a_warning(self):
        self.kube.answer("create", ran(pod_json()))
        starved = "the kubelet sizes a memory scratch to the 96Gi limit at most, and a full one leaves the pod's processes no memory, so they are OOM-killed."
        cases = (
            ([{"path": "/var/tmp", "scratch": {"medium": "Memory", "size": "96Gi"}}], [f"scratch /var/tmp 96Gi is not below the 96Gi memory limit; {starved}"]),
            (
                [{"path": "/a", "scratch": {"medium": "Memory", "size": "50Gi"}}, {"path": "/b", "scratch": {"medium": "Memory", "size": "46Gi"}}],
                [f"scratch /a 50Gi + /b 46Gi is not below the 96Gi memory limit; {starved}"],
            ),
            ([{"path": "/var/tmp", "scratch": {"medium": "Memory"}}], [f"scratch /var/tmp has no size, so it can fill the 96Gi memory limit; {starved}"]),
        )
        for inputs, warnings in cases:
            status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, "resources": {"limits": {"memory": "96Gi"}}})
            self.assertEqual(status, 201)
            self.assertEqual(answer["warnings"], warnings)

    def test_a_limit_in_any_form_the_api_server_takes_is_read_after_the_pod_is_made(self):
        self.kube.answer("create", ran(pod_json()))
        starved = "the kubelet sizes a memory scratch to the {} limit at most, and a full one leaves the pod's processes no memory, so they are OOM-killed."
        cases = (
            ("96000000000000000000n", "100Gi", True),
            (" 96Gi", "90Gi", False),
            ("96000000000000000u", "100Gi", True),
            ("96Gi ", "96Gi", True),
        )
        for limit, size, warns in cases:
            inputs = [{"path": "/var/tmp", "scratch": {"medium": "Memory", "size": size}}]
            status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, "resources": {"limits": {"memory": limit}}})
            self.assertEqual(status, 201, answer)
            warned = [f"scratch /var/tmp {size} is not below the {limit} memory limit; {starved.format(limit)}"]
            self.assertEqual(answer.get("warnings"), warned if warns else None)

    def test_scratches_that_add_up_exactly_to_the_limit_warn(self):
        self.kube.answer("create", ran(pod_json()))
        inputs = [{"path": "/a", "scratch": {"medium": "Memory", "size": "0.1G"}}, {"path": "/b", "scratch": {"medium": "Memory", "size": "33.1G"}}]
        status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, "resources": {"limits": {"memory": "33.2G"}}})
        self.assertEqual(status, 201)
        self.assertEqual(len(answer["warnings"]), 1)
        self.assertIn("/a 0.1G + /b 33.1G is not below the 33.2G memory limit", answer["warnings"][0])

    def test_scratches_below_the_memory_limit_and_within_the_request_answer_no_warnings(self):
        self.kube.answer("create", ran(pod_json()))
        inputs = [{"path": "/var/tmp", "scratch": {"medium": "Memory", "size": "56Gi"}}, {"path": "/small", "scratch": {"medium": "Memory", "size": "8Gi"}}]
        resources = {"requests": {"memory": "72Gi"}, "limits": {"memory": "96Gi"}}
        status, answer = self.call("POST", "/v1/pods", {"image": "toolchain", "inputs": inputs, "resources": resources})
        self.assertEqual(status, 201)
        self.assertEqual(list(answer), ["pod"])
        self.assertEqual(self.created()["spec"]["resources"], resources)

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
        self.refused("POST", "/v1/pods", {"image": "t", "requester": True}, 400, ["names no requester"])
        self.refused("POST", "/v1/pods", {"image": "t", "notify": {"thread": 3}}, 400, ["names its channel"])
        self.assertNotIn("create", self.kube.verbs())

    def test_a_pod_records_where_to_post(self):
        self.kube.answer("create", ran(pod_json()))
        self.call("POST", "/v1/pods", {"image": "t", "notify": {"channel": "builds", "thread": 5}})
        notes = self.created()["metadata"]["annotations"]
        self.assertEqual(notes["ducktape.industries/notify-channel"], "builds")
        self.assertEqual(notes["ducktape.industries/notify-thread"], "5")
        self.assertEqual(notes["kubectl.kubernetes.io/default-container"], "run")


FLEETED = {**ASKED, "ducktape-program": "ducktape-industries/fleet", "fleet-run": "7", "fleet-attempt": "1", "fleet-asker": "17", "fleet-machine": "31"}


class Fleeted(Served):
    def test_a_pod_fleet_relays_is_requested_by_the_asker_fleet_stamps(self):
        self.kube.answer("create", ran(pod_json()))
        status, answer = self.call("POST", "/v1/pods", {"image": "t"}, FLEETED)
        self.assertEqual(status, 201, answer)
        labels = self.created()["metadata"]["labels"]
        self.assertEqual(labels["ducktape.industries/requester"], "17")
        self.assertEqual(labels["ducktape.industries/program"], "ducktape-industries-fleet")
        self.assertEqual((labels["ducktape.industries/fleet-run"], labels["ducktape.industries/fleet-attempt"]), ("7", "1"))

    def fleet_pod(self, run="7", attempt="1"):
        pod = pod_json()
        pod["metadata"]["labels"].update({"ducktape.industries/fleet-run": run, "ducktape.industries/fleet-attempt": attempt})
        return pod

    def test_a_call_fleet_relays_reaches_its_own_runs_pod(self):
        self.kube.answer("get", ran(self.fleet_pod()))
        status, answer = self.call("DELETE", "/v1/pods/duckpond-41", headers=FLEETED)
        self.assertEqual((status, answer), (200, {"deleted": "duckpond-41"}))
        self.assertEqual([call[:2] for call in self.api.calls], [("DELETE", "/api/v1/namespaces/agents/pods/duckpond-41")])
        self.assertEqual(self.signer.senders, [])

    def test_a_call_fleet_relays_deletes_its_runs_pod_only_while_its_asker_requested_it(self):
        pod = self.fleet_pod()
        pod["metadata"]["labels"]["ducktape.industries/requester"] = "15"
        self.kube.answer("get", ran(pod))
        status, answer = self.call("DELETE", "/v1/pods/duckpond-41", headers=FLEETED)
        self.assertEqual(status, 403, answer)
        self.assertIn("pod duckpond-41 is account 15's", answer["error"])
        self.assertEqual(self.api.calls, [])

    def test_a_call_fleet_relays_reaches_no_other_runs_pod(self):
        routes = (("DELETE", "/v1/pods/duckpond-41", None), ("GET", "/v1/pods/duckpond-41", None), ("GET", "/v1/pods/duckpond-41/wait?every=5", None), ("POST", "/v1/pods/duckpond-41/exec", {"command": ["true"]}), ("POST", "/v1/pods/duckpond-41/blob", {"path": "/out/x", "kind": "chunk"}))
        for run, attempt in (("6", "1"), ("7", "2"), (None, None)):
            pod = self.fleet_pod(run, attempt) if run else pod_json()
            self.kube.answer("get", ran(pod))
            for method, path, body in routes:
                with self.subTest(run=run, attempt=attempt, route=path, method=method):
                    status, answer = self.call(method, path, body, FLEETED)
                    self.assertEqual(status, 403, answer)
                    self.assertIn("run 7 attempt 1", answer["error"])
        self.assertEqual(set(self.kube.verbs()), {"get"})

    def test_a_call_fleet_relays_reaches_no_route_its_backend_does_not_use(self):
        routes = (("GET", "/v1/pods", None), ("POST", "/v1/pods/duckpond-41/file", {"path": "/out/x", "to": "/shared/x"}), ("POST", "/v1/pods/duckpond-41/read", {"path": "/out/x"}), ("POST", "/v1/pods/duckpond-41/push", {"path": "/work"}), ("GET", "/v1/pods/duckpond-41/logs", None), ("POST", "/v1/manifests", {"manifests": []}), ("GET", "/v1/usage", None), ("GET", "/v1/k8s/api/v1/namespaces/agents/pods", None), ("POST", "/v1/pods/builds/exec", {"command": ["sh", "/var/tmp/warm/build.sh", "busy"]}))
        for method, path, body in routes:
            with self.subTest(method=method, route=path):
                status, answer = self.call(method, path, body, FLEETED)
                self.assertEqual(status, 403, answer)
        self.assertEqual(self.kube.calls, [])

    def test_a_call_fleet_relays_without_its_run_is_refused_before_anything_runs(self):
        for header in ("fleet-run", "fleet-attempt"):
            for stamp in (None, "", "x"):
                with self.subTest(header=header, stamp=stamp):
                    headers = {name: value for name, value in FLEETED.items() if name != header}
                    if stamp is not None:
                        headers[header] = stamp
                    status, answer = self.call("POST", "/v1/pods", {"image": "t"}, headers)
                    self.assertEqual(status, 400, answer)
                    self.assertIn(header, answer["error"])
        self.assertEqual(self.kube.calls, [])

    def test_a_call_fleet_relays_names_no_requester_of_its_own(self):
        status, answer = self.call("POST", "/v1/pods", {"image": "t", "requester": 15}, FLEETED)
        self.assertEqual(status, 400, answer)
        self.assertIn("names no requester", answer["error"])
        self.assertNotIn("create", self.kube.verbs())

    def test_a_call_fleet_relays_with_no_asker_it_reads_is_refused_before_anything_runs(self):
        for asker in (None, "", "seventeen", "-1"):
            with self.subTest(asker=asker):
                headers = {name: value for name, value in FLEETED.items() if name != "fleet-asker"}
                if asker is not None:
                    headers["fleet-asker"] = asker
                status, answer = self.call("GET", "/v1/pods", headers=headers)
                self.assertEqual(status, 400, answer)
                self.assertIn("fleet-asker", answer["error"])
        self.assertEqual(self.kube.calls, [])

    def test_another_programs_call_naming_a_fleet_asker_is_requested_by_its_sender(self):
        self.kube.answer("create", ran(pod_json()))
        self.signer.sender = sent_by(15)
        self.call("POST", "/v1/pods", {"image": "t"}, {**ASKED, "fleet-asker": "17"})
        self.assertEqual(self.created()["metadata"]["labels"]["ducktape.industries/requester"], "15")


def sent_by(account):
    return ran(server.u8(server.EGRESS_SENDER) + server.u8(1) + server.u64(account))


class Requested(Served):
    def test_a_pods_requester_is_the_account_egress_says_sent_its_request(self):
        self.kube.answer("create", ran(pod_json()))
        self.signer.sender = sent_by(21)
        status, answer = self.call("POST", "/v1/pods", {"image": "t"})
        self.assertEqual(status, 201, answer)
        self.assertEqual(self.created()["metadata"]["labels"]["ducktape.industries/requester"], "21")
        self.assertEqual(self.signer.senders, [server.egress_sender(41)])

    def test_an_applys_requester_is_the_account_egress_says_sent_its_request(self):
        self.kube.answer("apply", ran({"kind": "List", "items": [{"kind": "Deployment", "metadata": {"name": "web"}}]}))
        self.signer.sender = sent_by(21)
        status, answer = self.call("POST", "/v1/manifests", {"manifests": [deployment()]})
        self.assertEqual(status, 200, answer)
        (manifest,) = [call for call in self.kube.calls if call[0] == "apply"][0][1]
        self.assertEqual(manifest["metadata"]["labels"]["ducktape.industries/requester"], "21")
        self.assertEqual(manifest["spec"]["template"]["metadata"]["labels"]["ducktape.industries/requester"], "21")

    def test_a_body_naming_a_requester_is_refused_whoever_it_names(self):
        self.signer.sender = sent_by(21)
        for requester in (17, 21):
            with self.subTest(requester=requester):
                self.refused("POST", "/v1/pods", {"image": "t", "requester": requester}, 400, ["names no requester", "egress says sent"])
                self.refused("POST", "/v1/manifests", {"manifests": [deployment()], "requester": requester}, 400, ["names no requester", "egress says sent"])
        self.assertNotIn("create", self.kube.verbs())
        self.assertNotIn("apply", self.kube.verbs())

    def test_a_request_egress_names_no_account_for_makes_a_pod_with_no_requester(self):
        self.kube.answer("create", ran(pod_json(requester=None)))
        status, answer = self.call("POST", "/v1/pods", {"image": "t"})
        self.assertEqual(status, 201, answer)
        self.assertNotIn("ducktape.industries/requester", self.created()["metadata"]["labels"])
        self.assertEqual(self.signer.senders, [server.egress_sender(41)])

    def test_a_create_egress_was_not_asked_about_makes_nothing(self):
        self.signer.sender = ran(code=1, err="egress: no such query")
        self.refused("POST", "/v1/pods", {"image": "t"}, 502, ["who sent request 41"])
        self.refused("POST", "/v1/manifests", {"manifests": [deployment()]}, 502, ["who sent request 41"])
        self.assertNotIn("create", self.kube.verbs())
        self.assertNotIn("apply", self.kube.verbs())

    def test_a_fleet_call_is_requested_by_its_asker_and_egress_is_not_asked(self):
        self.kube.answer("create", ran(pod_json()))
        self.signer.sender = sent_by(21)
        status, answer = self.call("POST", "/v1/pods", {"image": "t"}, FLEETED)
        self.assertEqual(status, 201, answer)
        self.assertEqual(self.created()["metadata"]["labels"]["ducktape.industries/requester"], "17")
        self.assertEqual(self.signer.senders, [])


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


class Status(Served):
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

    def test_logs_answer_the_slice_the_caller_names(self):
        self.kube.answer("logs", ran(b"compiling\n"))
        self.refused("GET", "/v1/pods/duckpond-41/logs", None, 400, ["tail", "bytes", "whole=1"])
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?whole=1"), (200, b"compiling\n"))
        self.assertEqual(self.kube.calls[-1], ("logs", "duckpond-41", []))
        self.call("GET", "/v1/pods/duckpond-41/logs?tail=20&since=60&container=fetch&bytes=9")
        self.assertEqual(self.kube.calls[-1], ("logs", "duckpond-41", ["--container", "fetch", "--tail=20", "--since=60s"]))

    def test_a_log_line_is_what_a_terminal_shows_and_bytes_keep_whole_lines_from_the_end(self):
        self.kube.answer("logs", ran(b"Receiving 10%\rReceiving 90%\rReceiving 100%, done.\r\nGATES ok\ncrlf\r\n"))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?tail=5"), (200, b"Receiving 100%, done.\nGATES ok\ncrlf\n"))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?bytes=14"), (200, b"GATES ok\ncrlf\n"))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?bytes=12"), (200, b"crlf\n"))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?bytes=0"), (200, b""))
        self.kube.answer("logs", ran("été".encode()))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?bytes=4"), (200, "té".encode()))
        self.kube.answer("logs", ran("été\n".encode()))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?bytes=4"), (200, "té\n".encode()))
        self.kube.answer("logs", ran(b"ok\n" + b"x" * 5000 + b"\n"))
        self.assertEqual(self.call("GET", "/v1/pods/duckpond-41/logs?bytes=4096"), (200, b"x" * 4095 + b"\n"))

    def test_logs_refuse_an_option_they_do_not_take(self):
        self.refused("GET", "/v1/pods/duckpond-41/logs?follow=1", None, 400, ["no follow"])
        self.refused("GET", "/v1/pods/duckpond-41/logs?tail=-1", None, 400, ["whole number"])
        self.refused("GET", "/v1/pods/duckpond-41/logs?bytes=%D9%A3", None, 400, ["whole number"])
        self.refused("GET", "/v1/pods/duckpond-41/logs?whole=1&tail=5", None, 400, ["whole=1"])
        self.refused("GET", "/v1/pods/duckpond-41/logs?whole=yes", None, 400, ["whole=1"])


class Delete(Served):
    def setUp(self):
        super().setUp()
        self.service.wall = lambda: 1792681200.4

    def test_delete_removes_the_pod_without_waiting(self):
        self.kube.answer("get", ran(pod_json()))
        self.signer.sender = sent_by(17)
        self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-41"), (200, {"deleted": "duckpond-41"}))
        self.assertEqual(self.kube.calls, [("get", "duckpond-41"), DELETE_MARKED])
        self.assertEqual([call[:2] for call in self.api.calls], [("DELETE", "/api/v1/namespaces/agents/pods/duckpond-41")])

    def test_deleting_a_missing_pod_is_not_found(self):
        self.kube.answer("get", ran(code=1, err=NOT_FOUND))
        self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-9")[0], 404)
        self.assertEqual(self.api.calls, [])

    def test_a_requesters_delete_of_a_live_pod_marks_it_with_its_uid_before_the_delete(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running", requester="21")))
        self.signer.sender = sent_by(21)
        order = []
        mark, call = self.kube.annotate, self.api.call
        self.kube.annotate = lambda *args: (order.append("annotate"), mark(*args))[1]
        self.api.call = lambda method, *rest: (order.append(method), call(method, *rest))[1]
        self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-41"), (200, {"deleted": "duckpond-41"}))
        self.assertEqual(order, ["annotate", "DELETE"])
        self.assertEqual(self.kube.calls, [("get", "duckpond-41"), DELETE_MARKED])

    def test_a_requesters_delete_of_a_pod_that_already_ended_leaves_it_unmarked(self):
        for pod in (pod_json(state=ENDED, phase="Succeeded", requester="21"), pod_json(phase="Failed", reason="Evicted", requester="21")):
            with self.subTest(phase=pod["status"]["phase"]):
                self.kube.calls.clear()
                self.kube.answer("get", ran(pod))
                self.signer.sender = sent_by(21)
                self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-41"), (200, {"deleted": "duckpond-41"}))
                self.assertEqual(self.kube.verbs(), ["get"])

    def test_a_delete_kubernetes_refuses_takes_its_mark_off_again(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running", requester="21")))
        self.signer.sender = sent_by(21)
        self.api.answers = [answered(500, {"kind": "Status", "message": "etcdserver: request timed out", "code": 500})]
        self.refused("DELETE", "/v1/pods/duckpond-41", None, 500, ["pod duckpond-41 was not deleted", "request timed out"])
        self.assertEqual(self.kube.calls[1:], [DELETE_MARKED, ("unannotate", "duckpond-41", DELETED_MARK)])

    def test_a_mark_kubernetes_refuses_still_deletes_the_pod(self):
        self.kube.answer("get", ran(pod_json(state=RUNNING, phase="Running", requester="21")))
        self.kube.answer("annotate", ran(code=1, err=APISERVER))
        self.signer.sender = sent_by(21)
        with unittest.mock.patch.object(server, "say") as said:
            self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-41"), (200, {"deleted": "duckpond-41"}))
        said.assert_any_call("pods_unmarked", pod="duckpond-41", reason="kubectl_failed", detail=json.dumps(APISERVER))
        self.assertEqual([call[0] for call in self.api.calls], ["DELETE"])


NOBODY = ran(server.u8(server.EGRESS_SENDER) + server.u8(0))


class Owned(Served):
    def test_a_pods_requester_deletes_it_held_to_the_uid_it_was_read_at(self):
        self.kube.answer("get", ran(pod_json(requester="21")))
        self.signer.sender = sent_by(21)
        self.assertEqual(self.call("DELETE", "/v1/pods/duckpond-41"), (200, {"deleted": "duckpond-41"}))
        ((method, path, query, body, kind),) = self.api.calls
        self.assertEqual((method, path, query, kind), ("DELETE", "/api/v1/namespaces/agents/pods/duckpond-41", "", "application/json"))
        self.assertEqual(json.loads(body), {"preconditions": {"uid": "uid-duckpond-41"}})
        self.assertEqual(self.kube.verbs(), ["get", "annotate"])

    def test_no_other_account_deletes_a_pod(self):
        self.kube.answer("get", ran(pod_json(requester="21")))
        for sender, words in ((sent_by(15), "account 15's"), (sent_by(17), "account 17's"), (NOBODY, "no account's")):
            with self.subTest(words=words):
                self.signer.sender = sender
                self.refused("DELETE", "/v1/pods/duckpond-41", None, 403, ["pod duckpond-41 is account 21's", words])
        self.assertEqual(self.api.calls, [])
        self.assertEqual(set(self.kube.verbs()), {"get"})

    def test_a_pod_that_names_no_requester_is_deleted_by_no_call(self):
        self.kube.answer("get", ran(pod_json(requester=None)))
        self.signer.sender = sent_by(21)
        self.refused("DELETE", "/v1/pods/duckpond-41", None, 403, ["names no requester", "kubectl"])
        self.assertEqual(self.api.calls, [])

    def test_the_program_that_made_a_pod_deletes_it_as_its_own_account(self):
        pod = pod_json(requester="38")
        pod["metadata"]["labels"]["ducktape.industries/program"] = "actions-pods"
        self.kube.answer("get", ran(pod))
        self.signer.sender = sent_by(38)
        status, answer = self.call("DELETE", "/v1/pods/duckpond-41", headers={**ASKED, "ducktape-program": "actions-pods"})
        self.assertEqual((status, answer), (200, {"deleted": "duckpond-41"}))
        self.assertEqual([call[0] for call in self.api.calls], ["DELETE"])

    def test_egress_not_saying_who_sent_a_delete_deletes_nothing(self):
        self.kube.answer("get", ran(pod_json(requester="21")))
        self.signer.sender = ran(code=1, err="egress: no such query")
        self.refused("DELETE", "/v1/pods/duckpond-41", None, 502, ["who sent request 41"])
        self.assertEqual(self.api.calls, [])

    def test_kubernetes_refusing_the_delete_answers_its_refusal(self):
        self.kube.answer("get", ran(pod_json(requester="21")))
        self.signer.sender = sent_by(21)
        self.api.answers = [answered(409, {"kind": "Status", "message": "Precondition failed: UID in precondition: uid-duckpond-41, UID in object meta: other", "code": 409})]
        self.refused("DELETE", "/v1/pods/duckpond-41", None, 409, ["pod duckpond-41 was not deleted", "Precondition failed"])

    def test_a_manifest_is_deleted_by_its_requester_alone_held_to_its_uid(self):
        web = {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "uid": "u-web", "labels": {"ducktape.industries/requester": "21"}}}
        self.kube.answer("objects", ran(web))
        self.signer.sender = sent_by(15)
        self.refused("DELETE", "/v1/manifests", {"kind": "deployment", "name": "web"}, 403, ["deployment web is account 21's"])
        self.assertEqual(self.api.calls, [])
        self.signer.sender = sent_by(21)
        self.assertEqual(self.call("DELETE", "/v1/manifests", {"kind": "deployment", "name": "web"}), (200, {"deleted": "deployment.apps/web"}))
        ((method, path, _, body, _),) = self.api.calls
        self.assertEqual((method, path), ("DELETE", "/apis/apps/v1/namespaces/agents/deployments/web"))
        self.assertEqual(json.loads(body), {"preconditions": {"uid": "u-web"}})
        self.assertEqual(self.kube.verbs(), ["objects", "objects"])

    def test_a_manifest_that_names_no_requester_is_deleted_by_no_call(self):
        self.kube.answer("objects", ran({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m", "uid": "u-m"}}))
        self.signer.sender = sent_by(21)
        self.refused("DELETE", "/v1/manifests", {"kind": "configmap", "name": "m"}, 403, ["configmap m names no requester"])
        self.assertEqual(self.api.calls, [])

    def test_a_passthrough_delete_is_its_objects_requesters_alone_held_to_its_uid(self):
        path = "/api/v1/namespaces/agents/configmaps/m"
        mine = {"kind": "ConfigMap", "metadata": {"name": "m", "uid": "u-m", "labels": {"ducktape.industries/requester": "21"}}}
        self.api.answers = [answered(200, mine)]
        self.signer.sender = sent_by(15)
        self.refused("DELETE", "/v1/k8s" + path, None, 403, ["configmaps m is account 21's"])
        self.assertEqual([call[:2] for call in self.api.calls], [("GET", path)])
        self.api.calls, self.api.answers = [], [answered(200, mine), answered(200, {"kind": "Status", "status": "Success"})]
        self.signer.sender = sent_by(21)
        status, answer = self.call("DELETE", "/v1/k8s" + path, {"propagationPolicy": "Background"})
        self.assertEqual((status, answer), (200, {"kind": "Status", "status": "Success"}))
        (_, (method, target, _, body, _)) = self.api.calls
        self.assertEqual((method, target), ("DELETE", path))
        self.assertEqual(json.loads(body), {"propagationPolicy": "Background", "preconditions": {"uid": "u-m"}})
        self.api.calls, self.api.answers = [], [answered(200, mine)]
        self.refused("DELETE", "/v1/k8s" + path, {"preconditions": {"uid": "u-other"}}, 409, ["uid u-other"])
        self.assertEqual([call[0] for call in self.api.calls], ["GET"])

    def test_a_passthrough_delete_of_a_collection_deletes_only_the_callers_own(self):
        self.signer.sender = sent_by(21)
        self.assertEqual(self.call("DELETE", PODS + "?labelSelector=app%3Dweb")[0], 200)
        ((method, target, query, _, _),) = self.api.calls
        self.assertEqual((method, target), ("DELETE", "/api/v1/namespaces/agents/pods"))
        self.assertEqual(server.parse_qs(query), {"labelSelector": ["app=web,ducktape.industries/requester=21"]})
        self.signer.sender = NOBODY
        self.refused("DELETE", PODS, None, 403, ["only the caller's own"])
        self.assertEqual(len(self.api.calls), 1)

    def test_an_eviction_is_its_pods_requesters_alone_held_to_its_uid(self):
        pod = {"kind": "Pod", "metadata": {"name": "p", "uid": "u-p", "labels": {"ducktape.industries/requester": "21"}}}
        eviction = {"apiVersion": "policy/v1", "kind": "Eviction", "metadata": {"name": "p"}}
        self.api.answers = [answered(200, pod)]
        self.signer.sender = sent_by(15)
        self.refused("POST", PODS + "/p/eviction", eviction, 403, ["pods p is account 21's"])
        self.assertEqual([call[0] for call in self.api.calls], ["GET"])
        self.api.calls, self.api.answers = [], [answered(200, pod)]
        self.signer.sender = sent_by(21)
        self.assertEqual(self.call("POST", PODS + "/p/eviction", eviction)[0], 200)
        ((_, read, _, _, _), (method, target, _, body, _)) = self.api.calls
        self.assertEqual((read, method, target), ("/api/v1/namespaces/agents/pods/p", "POST", "/api/v1/namespaces/agents/pods/p/eviction"))
        self.assertEqual(json.loads(body)["deleteOptions"], {"preconditions": {"uid": "u-p"}})

    def test_an_object_made_through_the_passthrough_is_requested_by_its_sender(self):
        self.signer.sender = sent_by(21)
        self.call("POST", "/v1/k8s/api/v1/namespaces/agents/configmaps", {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m"}})
        ((method, _, _, body, _),) = self.api.calls
        self.assertEqual(method, "POST")
        self.assertEqual(json.loads(body)["metadata"]["labels"].get("ducktape.industries/requester"), "21")

    def test_a_passthrough_put_or_patch_is_its_objects_requesters_alone_held_to_the_version_judged(self):
        path = "/api/v1/namespaces/agents/configmaps/m"
        labels = {"ducktape.industries/requester": "21"}
        mine = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m", "uid": "u-m", "resourceVersion": "7", "labels": labels}, "data": {"a": "b"}}
        put = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m", "labels": labels}, "data": {"a": "c"}}
        merge = b'{"data": {"a": "c"}}'
        for sender, words in ((sent_by(15), "account 15's"), (NOBODY, "no account's")):
            self.signer.sender = sender
            for method, body, headers in (("PUT", put, ASKED), ("PATCH", merge, MERGE)):
                self.api.calls, self.api.answers = [], [answered(200, mine), answered(200, mine), answered(200, mine)]
                code, answer = self.call(method, "/v1/k8s" + path, body, headers=headers)
                self.assertEqual(code, 403, (method, words, answer))
                for word in ("configmaps m is account 21's", "changes or deletes it", words):
                    self.assertIn(word, answer["error"])
                self.assertEqual([call[:2] for call in self.api.calls], [("GET", path)])
        self.signer.sender = sent_by(21)
        self.api.calls, self.api.answers = [], [answered(200, mine)]
        self.assertEqual(self.call("PUT", "/v1/k8s" + path, put)[0], 200)
        self.assertEqual([call[:2] for call in self.api.calls], [("GET", path), ("PUT", path)])
        self.assertEqual(json.loads(self.api.calls[1][3])["metadata"]["resourceVersion"], "7")
        self.api.calls, self.api.answers = [], [answered(200, mine), answered(200, mine)]
        self.assertEqual(self.call("PATCH", "/v1/k8s" + path, merge, headers=MERGE)[0], 200)
        self.assertEqual([(call[0], call[2]) for call in self.api.calls], [("GET", ""), ("PATCH", "dryRun=All"), ("PATCH", "")])

    def test_a_passthrough_put_or_patch_of_an_object_that_names_no_requester_changes_nothing(self):
        path = "/api/v1/namespaces/agents/configmaps/m"
        bare = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m", "uid": "u-m", "resourceVersion": "7"}}
        self.signer.sender = sent_by(21)
        for method, body, headers in (("PUT", {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m"}}, ASKED), ("PATCH", b'{"data": {}}', MERGE)):
            self.api.calls, self.api.answers = [], [answered(200, bare), answered(200, bare), answered(200, bare)]
            code, answer = self.call(method, "/v1/k8s" + path, body, headers=headers)
            self.assertEqual(code, 403, (method, answer))
            self.assertIn("configmaps m names no requester", answer["error"])
            self.assertEqual([call[0] for call in self.api.calls], ["GET"])

    def test_a_subresource_put_or_patch_is_judged_by_its_objects_requester_and_held_to_that_read(self):
        web = "/apis/apps/v1/namespaces/agents/deployments/web"
        mine = {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "uid": "u-web", "resourceVersion": "7", "labels": {"ducktape.industries/requester": "21"}}}
        scale = {"apiVersion": "autoscaling/v1", "kind": "Scale", "metadata": {"name": "web", "resourceVersion": "8"}, "spec": {"replicas": 1}}
        replicas = b'{"spec": {"replicas": 0}}'
        self.signer.sender = sent_by(15)
        for method, headers in (("PUT", ASKED), ("PATCH", MERGE)):
            self.api.calls, self.api.answers = [], [answered(200, mine), answered(200, scale), answered(200, scale)]
            code, answer = self.call(method, DEPLOYMENTS + "/web/scale", replicas, headers=headers)
            self.assertEqual(code, 403, (method, answer))
            self.assertIn("deployments web is account 21's", answer["error"])
            self.assertEqual([call[:2] for call in self.api.calls], [("GET", web)])
        self.signer.sender = sent_by(21)
        self.api.calls, self.api.answers = [], [answered(200, mine), answered(200, scale), answered(200, scale), answered(200, scale)]
        self.assertEqual(self.call("PATCH", DEPLOYMENTS + "/web/scale", replicas, headers=MERGE)[0], 200)
        self.assertEqual([call[:3] for call in self.api.calls], [("GET", web, ""), ("GET", web + "/scale", ""), ("PATCH", web + "/scale", "dryRun=All"), ("PATCH", web + "/scale", "")])
        self.assertEqual(json.loads(self.api.calls[-1][3]), {"spec": {"replicas": 0}, "metadata": {"resourceVersion": "7"}})

    def test_a_put_or_patch_whose_body_is_not_read_is_judged_by_its_objects_requester_too(self):
        pod = "/api/v1/namespaces/agents/pods/p"
        mine = {"kind": "Pod", "metadata": {"name": "p", "uid": "u-p", "labels": {"ducktape.industries/requester": "21"}}}
        cases = (
            ("PUT", PODS + "/p/proxy/a/b", b"raw", {**ASKED, "content-type": "application/octet-stream"}, pod),
            ("PATCH", DEPLOYMENTS + "/web/scale", b"", MERGE, "/apis/apps/v1/namespaces/agents/deployments/web"),
        )
        self.signer.sender = sent_by(15)
        for method, path, body, headers, read in cases:
            self.api.calls, self.api.answers = [], [answered(200, mine)]
            code, answer = self.call(method, path, body, headers=headers)
            self.assertEqual(code, 403, (path, answer))
            self.assertIn("is account 21's", answer["error"])
            self.assertEqual([call[:2] for call in self.api.calls], [("GET", read)])
        self.signer.sender = sent_by(21)
        self.api.calls, self.api.answers = [], [answered(200, mine)]
        method, path, body, headers, _ = cases[0]
        self.assertEqual(self.call(method, path, body, headers=headers)[0], 200)
        self.assertEqual([call[:4] for call in self.api.calls], [("GET", pod, "", b""), ("PUT", pod + "/proxy/a/b", "", b"raw")])

    def test_a_put_or_patch_of_an_absent_object_is_refused_naming_the_posts_that_make_one(self):
        path = "/api/v1/namespaces/agents/configmaps/m"
        made = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m"}}
        missing = {"kind": "Status", "status": "Failure", "reason": "NotFound", "code": 404}
        self.signer.sender = sent_by(21)
        for method, media in (("PUT", "application/json"), ("PATCH", "application/apply-patch+yaml"), ("PATCH", "application/merge-patch+json")):
            self.api.calls, self.api.answers = [], [answered(404, missing), answered(201, made), answered(201, made)]
            code, answer = self.call(method, "/v1/k8s" + path + "?fieldManager=me", json.dumps(made).encode(), headers={**ASKED, "content-type": media})
            self.assertEqual(code, 404, (method, media, answer))
            for words in ("configmaps m is not there", "a POST to /v1/k8s/api/v1/namespaces/agents/configmaps", "POST /v1/manifests"):
                self.assertIn(words, answer["error"])
            self.assertEqual([call[:2] for call in self.api.calls], [("GET", path)])

    def test_an_exec_attach_portforward_or_proxy_is_its_objects_requesters_alone_whatever_its_method(self):
        pod = "/api/v1/namespaces/agents/pods/p"
        service = "/api/v1/namespaces/agents/services/s"
        labels = {"ducktape.industries/requester": "21"}
        held = {
            pod: {"kind": "Pod", "metadata": {"name": "p", "uid": "u-p", "labels": labels}},
            service: {"kind": "Service", "metadata": {"name": "s", "uid": "u-s", "labels": labels}},
        }
        raw = {**ASKED, "content-type": "application/octet-stream"}
        cases = (
            ("POST", pod, "/exec", "command=sh&stdin=true", b""),
            ("GET", pod, "/exec", "command=sh", b""),
            ("POST", pod, "/attach", "stdin=true", b""),
            ("GET", pod, "/attach", "stdout=true", b""),
            ("POST", pod, "/portforward", "ports=8080", b""),
            ("GET", pod, "/portforward", "ports=8080", b""),
            ("GET", pod, "/proxy/a/b", "c=d", b""),
            ("POST", pod, "/proxy/a", "", b"\x00 raw"),
            ("DELETE", pod, "/proxy/a", "", b"\x00 raw"),
            ("GET", service, "/proxy", "", b""),
            ("DELETE", service, "/proxy/a", "c=d", b""),
        )
        for sender, words in ((sent_by(15), "account 15's"), (NOBODY, "no account's")):
            self.signer.sender = sender
            for method, read, tail, query, body in cases:
                self.api.calls, self.api.answers = [], [answered(200, held[read]), answered(200, {"kind": "Status"})]
                code, answer = self.call(method, "/v1/k8s" + read + tail + (f"?{query}" if query else ""), body, headers=raw)
                self.assertEqual(code, 403, (method, tail, words, answer))
                kind, name = read.rsplit("/", 2)[1:]
                for word in (f"{kind} {name} is account 21's", "changes or deletes it", words):
                    self.assertIn(word, answer["error"])
                self.assertEqual([call[:2] for call in self.api.calls], [("GET", read)], (method, tail))
        self.signer.sender = sent_by(21)
        for method, read, tail, query, body in cases:
            self.api.calls, self.api.answers = [], [answered(200, held[read]), answered(200, {"kind": "Status"})]
            code, answer = self.call(method, "/v1/k8s" + read + tail + (f"?{query}" if query else ""), body, headers=raw)
            self.assertEqual((code, answer), (200, {"kind": "Status"}), (method, tail))
            sent = [("GET", read, "", b"", None), (method, read + tail, query, body, "application/octet-stream")]
            self.assertEqual(self.api.calls, sent, (method, tail))

    def test_a_proxy_is_judged_by_the_object_its_name_names_as_kubernetes_splits_it(self):
        labels = {"ducktape.industries/requester": "21"}
        held = {
            "pods": {"kind": "Pod", "metadata": {"name": "p", "uid": "u-p", "labels": labels}},
            "services": {"kind": "Service", "metadata": {"name": "p", "uid": "u-s", "labels": labels}},
        }
        named = [(kind, segment) for kind in held for segment in ("p", "p:8080", "https:p:443", ":p:80")]
        self.signer.sender = sent_by(15)
        for kind, segment in named:
            self.api.calls, self.api.answers = [], [answered(200, held[kind])]
            self.refused("GET", f"/v1/k8s/api/v1/namespaces/agents/{kind}/{segment}/proxy/a", None, 403, [f"{kind} p is account 21's"])
            self.assertEqual([call[:2] for call in self.api.calls], [("GET", f"/api/v1/namespaces/agents/{kind}/p")], (kind, segment))
        self.signer.sender = sent_by(21)
        for kind, segment in named:
            self.api.calls, self.api.answers = [], [answered(200, held[kind]), answered(200, {"kind": "Status"})]
            self.assertEqual(self.call("GET", f"/v1/k8s/api/v1/namespaces/agents/{kind}/{segment}/proxy/a"), (200, {"kind": "Status"}), (kind, segment))
            read, forwarded = f"/api/v1/namespaces/agents/{kind}/p", f"/api/v1/namespaces/agents/{kind}/{segment}/proxy/a"
            self.assertEqual([call[:2] for call in self.api.calls], [("GET", read), ("GET", forwarded)], (kind, segment))
        for segment in ("p:1:2:3", "ftp:p:80", ":80", "http::80"):
            self.api.calls = []
            self.refused("GET", PODS + f"/{segment}/proxy", None, 400, [f"names its object as {segment}", "NAME:PORT"])
            self.assertEqual(self.api.calls, [], segment)

    def test_a_post_to_a_named_objects_subresource_is_its_objects_requesters_alone(self):
        pod = "/api/v1/namespaces/agents/pods/p"
        account = "/api/v1/namespaces/agents/serviceaccounts/a"
        labels = {"ducktape.industries/requester": "21"}
        held = {
            pod: {"kind": "Pod", "metadata": {"name": "p", "uid": "u-p", "labels": labels}},
            account: {"kind": "ServiceAccount", "metadata": {"name": "a", "uid": "u-a", "labels": labels}},
        }
        binding = {"apiVersion": "v1", "kind": "Binding", "metadata": {"name": "p"}, "target": {"name": "n"}}
        token = {"apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": {"audiences": ["a"]}}
        cases = ((pod, "/binding", binding), (pod, "/binding", None), (account, "/token", token), (account, "/token", None))
        for sender, words in ((sent_by(15), "account 15's"), (NOBODY, "no account's")):
            self.signer.sender = sender
            for read, tail, body in cases:
                self.api.calls, self.api.answers = [], [answered(200, held[read]), answered(201, {"kind": "Status"})]
                kind, name = read.rsplit("/", 2)[1:]
                self.refused("POST", "/v1/k8s" + read + tail, body, 403, [f"{kind} {name} is account 21's", "changes or deletes it", words])
                self.assertEqual([call[:2] for call in self.api.calls], [("GET", read)], (tail, body))
        self.signer.sender = sent_by(21)
        for read, tail, body in cases:
            self.api.calls, self.api.answers = [], [answered(200, held[read]), answered(201, {"kind": "Status"})]
            self.assertEqual(self.call("POST", "/v1/k8s" + read + tail, body), (201, {"kind": "Status"}), (tail, body))
            self.assertEqual([call[:2] for call in self.api.calls], [("GET", read), ("POST", read + tail)], (tail, body))
            sent = self.api.calls[1][3]
            self.assertEqual(json.loads(sent) if body else sent, body or b"", (tail, body))


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

    def test_a_script_runs_with_sh_c(self):
        self.kube.answer("exec", ran(b"hi\n"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"script": "echo hi | cat"})
        self.assertEqual((status, answer["stdout"]), (200, "hi\n"))
        self.assertEqual(self.kube.calls, [("exec", "duckpond-41", None, ["sh", "-c", "echo hi | cat"])])

    def test_an_exec_names_its_command_or_its_script(self):
        self.refused("POST", "/v1/pods/duckpond-41/exec", {"command": ["true"], "script": "true"}, 400, ["not both"])
        self.refused("POST", "/v1/pods/duckpond-41/exec", {"script": ""}, 400, ["script holds shell text"])
        self.refused("POST", "/v1/pods/duckpond-41/exec", {"script": ["true"]}, 400, ["script is a JSON str"])
        self.assertEqual(self.kube.calls, [])

    def test_an_exec_past_its_inline_bytes_keeps_its_output_in_the_pod_and_answers_where(self):
        self.kube.answer("exec", ran(b"a\nb\n", err="w"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"], "inline": 4})
        kept = {"stdout": {"path": "/tmp/pods-exec/41/stdout", "bytes": 4, "lines": 2}, "stderr": {"path": "/tmp/pods-exec/41/stderr", "bytes": 1, "lines": 0}}
        self.assertEqual((status, answer), (200, {"exit_code": 0, "kept": kept}))
        self.assertEqual(self.kube.calls[1:], [("keep", "duckpond-41", None, "/tmp/pods-exec/41/stdout", b"a\nb\n"), ("keep", "duckpond-41", None, "/tmp/pods-exec/41/stderr", b"w")])

    def test_a_kept_outputs_lines_are_its_newlines_as_wc_counts_them(self):
        self.kube.answer("exec", ran(b"a\rb\rc\nd"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["git", "clone"], "inline": 0})
        self.assertEqual((status, answer["kept"]["stdout"]["lines"]), (200, 1))

    def test_an_exec_within_its_inline_bytes_answers_whole(self):
        self.kube.answer("exec", ran(b"a\nb\n", err="w"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"], "inline": 5})
        self.assertEqual((status, answer), (200, {"exit_code": 0, "stdout": "a\nb\n", "stderr": "w"}))
        self.assertNotIn("keep", self.kube.verbs())

    def test_an_exec_keeps_its_output_in_the_container_it_ran_in(self):
        self.kube.answer("exec", ran(b"built\n"))
        self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"], "container": "out", "inline": 0})
        self.assertEqual([call[2] for call in self.kube.calls if call[0] == "keep"], ["out", "out"])

    def test_an_exec_whose_output_the_pod_could_not_keep_answers_whole(self):
        self.kube.answer("exec", ran(b"built\n"))
        self.kube.answer("keep", ran(code=1, err="sh: read-only file system"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"], "inline": 0})
        self.assertEqual((status, answer), (200, {"exit_code": 0, "stdout": "built\n", "stderr": ""}))

    def test_an_execs_inline_is_a_whole_number_of_bytes_from_0(self):
        self.refused("POST", "/v1/pods/duckpond-41/exec", {"command": ["make"], "inline": -1}, 400, ["inline is a whole number of bytes"])
        self.assertEqual(self.kube.calls, [])


class Read(Served):
    def test_a_read_answers_the_files_size_and_its_bytes_from_the_offset(self):
        self.kube.answer("exec", ran(b"11\nllo wo"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/read", {"path": "/out/a", "offset": 2, "length": 6})
        self.assertEqual((status, answer), (200, {"path": "/out/a", "size": 11, "offset": 2, "length": 6, "eof": False, "text": "llo wo"}))
        self.assertEqual(self.kube.calls, [("exec", "duckpond-41", None, ["sh", "-c", server.READING, "read", "/out/a", "2", "6"])])

    def test_a_read_to_the_end_says_eof_and_bytes_that_are_not_text_come_as_hex(self):
        self.kube.answer("exec", ran(b"3\n\xff\x00\x01"))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/read", {"path": "/out/b", "container": "out"})
        self.assertEqual((status, answer["eof"], answer["length"], answer["text_hex"]), (200, True, 3, "ff0001"))
        self.assertNotIn("text", answer)
        self.assertEqual(self.kube.calls[-1][2:], ("out", ["sh", "-c", server.READING, "read", "/out/b", "0", ""]))

    def test_a_missing_file_is_not_found_and_another_failure_is_refused_with_why(self):
        missing = ran(code=2, err="sh: 1: cannot open /nope: No such file\ncommand terminated with exit code 2")
        directory = ran(code=1, err="tail: error reading '/d': Is a directory\ncommand terminated with exit code 1")
        self.kube.answer("exec", missing, directory)
        self.refused("POST", "/v1/pods/duckpond-41/read", {"path": "/nope"}, 404, ["/nope was not read", "No such file"])
        self.refused("POST", "/v1/pods/duckpond-41/read", {"path": "/d"}, 400, ["/d was not read", "Is a directory"])

    def test_a_pod_kubernetes_cannot_reach_is_refused_with_why(self):
        self.kube.answer("exec", ran(code=1, err=NOT_FOUND))
        self.refused("POST", "/v1/pods/duckpond-9/read", {"path": "/out/a"}, 404, ["duckpond-9"])

    def test_a_read_names_an_absolute_path_an_offset_from_0_and_a_length_from_1(self):
        for body, words in (
            ({}, "names its path"),
            ({"path": "x"}, "absolute path"),
            ({"path": "/x", "offset": -1}, "from 0"),
            ({"path": "/x", "length": 0}, "from 1"),
            ({"path": "/x", "length": "5"}, "JSON int"),
            ({"path": "/x", "every": 5}, "no field every"),
        ):
            self.refused("POST", "/v1/pods/duckpond-41/read", body, 400, [words])
        self.assertEqual(self.kube.calls, [])

    def test_reading_prints_the_size_then_the_bytes_asked(self):
        with tempfile.TemporaryDirectory() as work:
            path = pathlib.Path(work) / "f"
            path.write_bytes(b"hello world")

            def read(file, *words):
                return subprocess.run(["sh", "-c", server.READING, "read", str(file), *words], capture_output=True)

            for words, printed in ((("0", ""), b"11\nhello world"), (("6", ""), b"11\nworld"), (("2", "3"), b"11\nllo"), (("20", ""), b"11\n")):
                done = read(path, *words)
                self.assertEqual((done.returncode, done.stdout), (0, printed), done.stderr)
            missing = read(pathlib.Path(work) / "nope", "0", "")
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn(b"No such file", missing.stderr)


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
        self.kube.answer("exec", ran(f"{COMMIT} {HAVE}\n1\n".encode()), ran(pack_of(3)))
        body = {"path": "/work/programs", "ref": "j-057/x", "have": [HAVE]}
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", body)
        self.assertEqual(status, 200, answer)
        expected = {
            "commit": COMMIT,
            "parents": [HAVE],
            "commits": 1,
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
        self.assertEqual(resolve, ("exec", "duckpond-41", None, ["sh", "-c", server.RESOLVING, "resolve", "/work/programs", "j-057/x", HAVE]))
        self.assertEqual(pack, ("exec", "duckpond-41", None, ["sh", "-c", server.PACKING, "pack", "/work/programs", COMMIT, HAVE]))
        self.assertEqual(self.signer.puts, [([pack_of(3)], "pack")])
        self.assertEqual((self.signer.submits, self.paused), ([], []))

    def test_head_is_the_default_ref_and_the_container_asked_runs_git(self):
        self.kube.answer("exec", ran(f"{COMMIT}\n1\n".encode()), ran(pack_of(1)))
        self.call("POST", "/v1/pods/duckpond-41/push", {"path": "/work/core", "container": "out"})
        resolve, pack = self.execs()
        self.assertEqual((resolve[2], resolve[3][-1], pack[3][4:]), ("out", "HEAD", ["/work/core", COMMIT]))

    def test_a_commit_forge_already_holds_answers_no_pack_and_puts_nothing(self):
        self.kube.answer("exec", ran(f"{COMMIT} {HAVE}\n0\n".encode()), ran(pack_of(0)))
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", {"path": "/work/core", "have": [COMMIT]})
        self.assertEqual((status, answer["pack"], answer["objects"], answer["commit"]), (200, None, 0, COMMIT))
        self.assertEqual((answer["parents"], answer["commits"]), ([HAVE], 0))
        self.assertEqual(self.signer.puts, [])

    def test_resolving_names_the_parents_and_counts_the_commits_forge_does_not_hold(self):
        with tempfile.TemporaryDirectory() as work:
            env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
            env.update(GIT_AUTHOR_NAME="pod", GIT_AUTHOR_EMAIL="pod@pods", GIT_COMMITTER_NAME="pod", GIT_COMMITTER_EMAIL="pod@pods")

            def git(*args):
                return subprocess.run(["git", "-C", work, *args], env=env, check=True, capture_output=True, text=True).stdout.strip()

            def resolve(*args):
                ran = subprocess.run(["sh", "-c", server.RESOLVING, "resolve", work, *args], env=env, capture_output=True, text=True)
                self.assertEqual(ran.returncode, 0, ran.stderr)
                return ran.stdout.splitlines()

            git("init", "-q")
            made = []
            for message in ("landed", "unlanded", "stacked"):
                git("commit", "-q", "--allow-empty", "-m", message)
                made.append(git("rev-parse", "HEAD"))
            landed, unlanded, stacked = made
            git("update-ref", "refs/remotes/origin/dev", landed)
            self.assertEqual(resolve("HEAD"), [f"{stacked} {unlanded}", "2"])
            self.assertEqual(resolve(unlanded), [f"{unlanded} {landed}", "1"])
            self.assertEqual(resolve("HEAD", unlanded), [f"{stacked} {unlanded}", "1"])
            self.assertEqual(resolve("HEAD", "ab" * 20), [f"{stacked} {unlanded}", "2"])
            self.assertEqual(resolve(landed), [landed, "0"])
            self.assertEqual(subprocess.run(["sh", "-c", server.RESOLVING, "resolve", work, "nowhere"], env=env).returncode, 1)

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

    def test_a_push_to_a_branch_takes_its_head_from_heads_and_answers_what_forges_push_takes(self):
        self.kube.answer("exec", ran(f"{COMMIT} {HAVE}\n1\n".encode()), ran(pack_of(3)))
        heads = [{"name": "refs/heads/j-1/x", "target": HAVE}, {"name": "refs/heads/j-1/x-2", "target": "ab" * 20}]
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", {"path": "/work/programs", "branch": "j-1/x", "heads": heads})
        self.assertEqual(status, 200, answer)
        line = f"{HAVE} {COMMIT} refs/heads/j-1/x".encode() + b"\0report-status"
        request = f"{len(line) + 4:04x}".encode() + line + b"0000"
        self.assertEqual((answer["branch"], answer["old"], answer["unchanged"], answer["commit"]), ("refs/heads/j-1/x", HAVE, False, COMMIT))
        self.assertEqual(answer["forge"], {"request": request.hex(), "pack": {"sha256": "cd" * 32}})
        resolve, pack = self.execs()
        self.assertEqual((resolve[3][-2:], pack[3][-2:]), (["HEAD", HAVE], [COMMIT, HAVE]))

    def test_a_new_branch_is_pushed_from_zeros_against_only_the_remote_refs(self):
        self.kube.answer("exec", ran(f"{COMMIT} {HAVE}\n1\n".encode()), ran(pack_of(1)))
        heads = [{"name": "refs/heads/j-1/x-2", "target": "ab" * 20}]
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", {"path": "/w", "branch": "j-1/x", "heads": heads})
        self.assertEqual((status, answer["old"], answer["unchanged"]), (200, None, False), answer)
        sent = bytes.fromhex(answer["forge"]["request"])
        self.assertEqual(sent[4:].split(b"\0")[0], f"{'0' * 40} {COMMIT} refs/heads/j-1/x".encode())
        resolve, pack = self.execs()
        self.assertEqual((resolve[3][-1], pack[3][-1]), ("HEAD", COMMIT))

    def test_a_commit_that_does_not_land_alone_is_refused_unless_allow_stack(self):
        stacked = ran(f"{COMMIT} {'ab' * 20}\n2\n".encode())
        self.kube.answer("exec", stacked, stacked, stacked, ran(pack_of(2)))
        onto = {"path": "/w", "branch": "j-1/x", "heads": [{"name": "refs/heads/j-1/x", "target": HAVE}]}
        self.refused("POST", "/v1/pods/duckpond-41/push", onto, 409, [f"is not a child of refs/heads/j-1/x's head {HAVE}", "allow_stack: true"])
        self.refused("POST", "/v1/pods/duckpond-41/push", {"path": "/w", "branch": "j-1/y", "heads": []}, 409, ["which forge does not hold yet", "2 commits", "allow_stack: true"])
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", {**onto, "allow_stack": True})
        self.assertEqual((status, answer["commits"]), (200, 2), answer)
        self.assertEqual((len(self.execs()), len(self.signer.puts)), (4, 1))

    def test_a_branch_already_at_the_commit_answers_unchanged(self):
        self.kube.answer("exec", ran(f"{COMMIT} {HAVE}\n0\n".encode()), ran(pack_of(0)))
        heads = [{"name": "refs/heads/j-1/x", "target": COMMIT}]
        status, answer = self.call("POST", "/v1/pods/duckpond-41/push", {"path": "/w", "branch": "j-1/x", "heads": heads})
        self.assertEqual((status, answer["unchanged"], answer["forge"]["pack"]), (200, True, None), answer)
        self.assertEqual(self.signer.puts, [])

    def test_a_push_to_a_branch_names_the_branch_by_name_its_heads_and_no_have(self):
        for body, words in (
            ({"path": "/w", "branch": "refs/heads/x", "heads": []}, "a branch's name under refs/heads/"),
            ({"path": "/w", "branch": "-x", "heads": []}, "a branch's name under refs/heads/"),
            ({"path": "/w", "branch": "x"}, "names heads"),
            ({"path": "/w", "branch": "x", "have": [HAVE], "heads": []}, "not both"),
            ({"path": "/w", "heads": []}, "heads only with its branch"),
            ({"path": "/w", "allow_stack": True}, "allow_stack only with its branch"),
            ({"path": "/w", "branch": "x", "heads": [{"name": "refs/heads/x"}]}, "each {name, target}"),
            ({"path": "/w", "branch": "x", "heads": [{"name": "refs/heads/x", "target": "main"}]}, "not a commit id"),
            ({"path": "/w", "branch": "x", "heads": [], "allow_stack": "yes"}, "allow_stack is a JSON bool"),
        ):
            self.refused("POST", "/v1/pods/duckpond-41/push", body, 400, [words])
        self.assertEqual(self.kube.calls, [])


def pkt(text):
    data = text.encode()
    return f"{len(data) + 4:04x}".encode() + data


class Report(Served):
    PUSHED = {"branch": "refs/heads/j-1/x", "commit": COMMIT, "old": HAVE, "objects": 3, "unchanged": False}

    def test_a_report_of_the_branch_moving_answers_the_push(self):
        report = (pkt("unpack ok\n") + pkt("ok refs/heads/j-1/x\n") + b"0000").hex()
        status, answer = self.call("POST", "/v1/report", {"report": report, "push": self.PUSHED})
        self.assertEqual((status, answer), (200, {"pushed": "refs/heads/j-1/x", "head": COMMIT, "old": HAVE, "objects": 3}))
        self.assertEqual((self.kube.calls, self.signer.submits), ([], []))

    def test_a_branch_forge_did_not_move_is_refused_in_forges_words(self):
        report = (pkt("unpack ok\n") + pkt("ng refs/heads/j-1/x non-fast-forward\n") + b"0000").hex()
        answer = self.refused("POST", "/v1/report", {"report": report, "push": self.PUSHED}, 409, [])
        self.assertEqual(answer["error"], "forge did not move the branch: ng refs/heads/j-1/x non-fast-forward")

    def test_a_pack_forge_did_not_unpack_is_refused_with_why(self):
        report = (pkt("unpack index-pack failed\n") + b"0000").hex()
        self.refused("POST", "/v1/report", {"report": report, "push": self.PUSHED}, 422, ["forge took none of the pack: index-pack failed"])

    def test_a_branch_already_at_its_commit_is_unchanged_whatever_the_report_says(self):
        report = (pkt("unpack ok\n") + pkt("ng refs/heads/j-1/x up to date\n") + b"0000").hex()
        status, answer = self.call("POST", "/v1/report", {"report": report, "push": {**self.PUSHED, "old": COMMIT, "unchanged": True}})
        self.assertEqual((status, answer), (200, {"pushed": "refs/heads/j-1/x", "head": COMMIT, "old": COMMIT, "objects": 3, "unchanged": True}))

    def test_bytes_that_are_no_git_report_are_refused(self):
        for report, status, words in (
            ("zz", 400, "as hex"),
            (b"0003abc".hex(), 400, "no git report"),
            (b"00ffabc".hex(), 400, "no git report"),
            ((pkt("unpack ok\n") + b"0000").hex(), 502, "names no ref"),
        ):
            self.refused("POST", "/v1/report", {"report": report, "push": self.PUSHED}, status, [words])
        self.refused("POST", "/v1/report", {"push": self.PUSHED}, 400, ["names its report"])


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

    def test_a_streamed_exec_on_a_pod_the_scheduler_cannot_place_notes_why_once_while_it_waits(self):
        self.kube.answer("exec", ran(code=1, err=UNPLACED), ran(b"built\n"))
        self.kube.answer("get", ran(unschedulable(pod_json())))
        self.watch.send("ADDED", unschedulable(pod_json()))
        response = self.stream("/v1/pods/duckpond-41/exec", "POST", {"command": ["make"], "every": 1})
        self.assertEqual(self.past_alive(response), ("note", {"line": f"pod duckpond-41 is pending, waiting to be placed: {UNSCHEDULABLE}"}))
        self.watch.send("MODIFIED", unschedulable(pod_json()))
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

        def told(*args, **named):
            running = execution(*args, **named)
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
        self.signer.sender = sent_by(17)
        status, answer = self.call("POST", "/v1/manifests", {"manifests": [deployment()]})
        self.assertEqual((status, answer), (200, {"applied": [{"kind": "Deployment", "name": "web"}]}))
        self.assertEqual(self.kube.verbs(), ["cluster_kinds", "held", "trial", "apply", "apply"])
        trial = self.kube.calls[2][1]
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
        web = {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "uid": "u-web", "labels": {"ducktape.industries/requester": "17"}}}
        self.kube.answer("objects", ran(web))
        self.signer.sender = sent_by(17)
        self.assertEqual(self.call("DELETE", "/v1/manifests", {"kind": "deployment", "name": "web"}), (200, {"deleted": "deployment.apps/web"}))
        self.assertEqual(self.kube.calls[-1], ("objects", "deployment", "web"))
        for kind in ("secrets", "Secret", "netpol", "networkpolicies.networking.k8s.io", "roles", "ing"):
            self.refused("DELETE", "/v1/manifests", {"kind": kind, "name": "x"}, 403, ["is refused"])
        self.assertEqual(self.kube.verbs().count("objects"), 1)
        self.assertEqual([call[:2] for call in self.api.calls], [("DELETE", "/apis/apps/v1/namespaces/agents/deployments/web")])

    def test_apply_cannot_transfer_an_existing_objects_delete_permission(self):
        theirs = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "victim", "uid": "u-victim", "labels": {"ducktape.industries/requester": "21"}}, "data": {"a": "1"}}
        self.kube.answer("held", ran(theirs))
        self.kube.answer("objects", ran(theirs))
        self.signer.sender = sent_by(17)
        taking = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "victim"}, "data": {"a": "2"}}
        self.refused("POST", "/v1/manifests", {"manifests": [taking]}, 403, ["manifest 0 (ConfigMap victim) is account 21's", "changes or deletes it", "this call is account 17's"])
        self.assertNotIn("apply", self.kube.verbs())
        self.refused("DELETE", "/v1/manifests", {"kind": "configmap", "name": "victim"}, 403, ["is account 21's"])
        self.assertEqual(self.api.calls, [])

    def test_apply_refuses_an_existing_object_that_names_no_requester(self):
        unnamed = {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "uid": "u-web", "labels": {"app": "web"}}}
        self.kube.answer("held", ran({"apiVersion": "v1", "kind": "List", "items": [unnamed]}))
        self.signer.sender = sent_by(17)
        self.refused("POST", "/v1/manifests", {"manifests": [deployment()]}, 403, ["manifest 0 (Deployment web) names no requester"])
        self.assertNotIn("apply", self.kube.verbs())

    def test_the_requester_applies_over_its_own_object_held_to_its_uid_after_one_read_of_every_object(self):
        mine = {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "uid": "u-web", "labels": {"ducktape.industries/requester": "17"}}}
        self.kube.answer("held", ran({"apiVersion": "v1", "kind": "List", "items": [mine]}))
        self.signer.sender = sent_by(17)
        config = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "web"}, "data": {}}
        status, answer = self.call("POST", "/v1/manifests", {"manifests": [deployment(), config]})
        self.assertEqual(status, 200, answer)
        (_, objects, _), (_, again, _) = self.applied()
        self.assertEqual(objects[0]["metadata"].get("uid"), "u-web")
        self.assertNotIn("uid", objects[1]["metadata"])
        self.assertEqual(objects, again)
        reads = [call[1] for call in self.kube.calls if call[0] == "held"]
        self.assertEqual([[(item["kind"], item["metadata"]["name"]) for item in read] for read in reads], [[("Deployment", "web"), ("ConfigMap", "web")]])

    def test_a_read_that_finds_nothing_applies_and_a_failed_read_applies_nothing(self):
        self.signer.sender = sent_by(17)
        self.kube.answer("held", ran(b""))
        self.assertEqual(self.call("POST", "/v1/manifests", {"manifests": [deployment()]})[0], 200)
        self.kube.calls = []
        self.kube.answer("held", ran(code=1, err='Error from server (Forbidden): deployments.apps "web" is forbidden'))
        self.refused("POST", "/v1/manifests", {"manifests": [deployment()]}, 403, ["whose they are could not be told", "is forbidden"])
        self.assertNotIn("apply", self.kube.verbs())

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


def owned(manifest, requester="21"):
    meta = manifest.get("metadata") or {}
    labels = {**(meta.get("labels") or {}), "ducktape.industries/requester": requester}
    return {**manifest, "metadata": {**meta, "uid": f"u-{meta.get('name', '')}", "labels": labels}}


class Holder(Served):
    def setUp(self):
        super().setUp()
        self.signer.sender = sent_by(21)


class Passthrough(Holder):
    def forced(self, **template_spec):
        return owned(deployment(priorityClassName="agents", automountServiceAccountToken=False, **template_spec))

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
        sent = b'{"spec":{"replicas":2}}'
        self.assertEqual(self.api.calls, [("GET", target, "", b"", None), ("PATCH", target, "dryRun=All", sent, kind), ("PATCH", target, "", sent, kind)])

    def test_a_patch_whose_result_breaks_a_rule_is_refused_after_the_dry_run(self):
        results = (
            (owned(deployment()), ["would run outside priority class agents", "PUT"]),
            (self.forced(volumes=[{"name": "h", "hostPath": {"path": "/"}}]), ["hostPath"]),
        )
        for result, words in results:
            self.api.calls, self.api.answers = [], [answered(200, result), answered(200, result)]
            self.refused("PATCH", DEPLOYMENTS + "/web?fieldManager=agents", {"spec": {}}, 403, words)
            self.assertEqual([call[2] for call in self.api.calls], ["", "fieldManager=agents&dryRun=All"])

    def test_kubernetes_refusing_the_dry_run_answers_its_refusal_and_sends_nothing_else(self):
        invalid = {"kind": "Status", "reason": "Invalid", "code": 422}
        self.api.answers = [answered(200, self.forced()), answered(422, invalid)]
        self.assertEqual(self.call("PATCH", DEPLOYMENTS + "/web", b"{}", headers=MERGE), (422, invalid))
        self.assertEqual([call[0] for call in self.api.calls], ["GET", "PATCH"])

    def test_a_subresource_is_read_for_quantities_but_not_confined(self):
        pod = {"kind": "Pod", "metadata": {"name": "p", "uid": "u-p", "labels": {"ducktape.industries/requester": "17"}}}
        self.api.answers = [answered(200, owned(deployment(), "17")), answered(200, {}), answered(200, {}), answered(200, {}), answered(200, pod)]
        self.signer.sender = sent_by(17)
        self.call("PATCH", DEPLOYMENTS + "/web/scale", b'{"spec": {"replicas": 0}}', headers=MERGE)
        self.call("POST", "/v1/k8s/api/v1/namespaces/agents/pods/p/eviction", {"apiVersion": "policy/v1", "kind": "Eviction"})
        self.assertEqual([(call[0], call[2]) for call in self.api.calls], [("GET", ""), ("GET", ""), ("PATCH", "dryRun=All"), ("PATCH", ""), ("GET", ""), ("POST", "")])
        self.assertEqual(self.kube.calls, [])


PODS = "/v1/k8s/api/v1/namespaces/agents/pods"
PLACED = owned({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"priorityClassName": "agents", "automountServiceAccountToken": False}})


def numbered(body, numerals):
    text = json.dumps(body)
    for name, numeral in numerals.items():
        text = text.replace(json.dumps(name), numeral)
    return text.encode()


class Quantities(Holder):
    def test_each_quantity_is_written_as_kubernetes_writes_it(self):
        cases = {
            "0": "0",
            "0Gi": "0",
            "-0.0": "0",
            "1": "1",
            "1000": "1k",
            "1024": "1024",
            "1500": "1500",
            "1Ki": "1Ki",
            "1024Ki": "1Mi",
            "1023Ki": "1023Ki",
            "1.5Gi": "1536Mi",
            "0.5Ki": "512",
            "96Gi": "96Gi",
            ".5Ti": "512Gi",
            "+64Mi": "64Mi",
            " 48Gi ": "48Gi",
            "0.5": "500m",
            "100m": "100m",
            "0.1m": "100u",
            "1n": "1n",
            "0.5n": "1n",
            "1e3": "1e3",
            "1E3": "1e3",
            "1500e-3": "1500e-3",
            "-1.5k": "-1500",
            "9E": "9E",
            "33.2G": "33200M",
            "1000000000000000000n": "1G",
            "8Ei": MOST,
            "7Ei": "7Ei",
            "1e4294967296": "1",
            "1e-10000000": "1e-9",
            "1e-2147483648": "1e-9",
            "-1e-2147483648": "-1e-9",
            ZEROS: MOST,
            ZEROS + "e-1000000": "1",
            SEVENS: MOST,
            SEVENS + "e-1000000": "777777778e-9",
            "0." + SEVENS: "777777778n",
            "\u00a01\u3000": "1",
            "\t\v\f\r\n\x851\u2028": "1",
        }
        for given, expected in cases.items():
            self.assertEqual(server.canonical(*server.reading(given)), expected, given[:24])
        for malformed in ("", "48GB", "1e", "1e3Gi", "1e9223372036854775808", "1e" + "9" * 100000, "NaN", "1\u001f", "\u001c1", "1\u001d", "\u001e1"):
            self.assertIsNone(server.reading(malformed))

    def test_a_schema_keeps_only_what_leads_to_a_quantity_and_ends_on_cycles(self):
        published = server.Published(CORE_V1)
        self.assertIn("io.k8s.example.Spiral", published.shapes)
        for empty in ("io.k8s.example.Circle", CORE + "EnvVar", CORE + "ConfigMap", CORE + "Service"):
            self.assertNotIn(empty, published.shapes)
        limits = {"values": QUANTITY_NAME, "declared": frozenset()}
        self.assertEqual(published.shapes[CORE + "ResourceRequirements"], {"properties": {"limits": limits, "requests": limits}})
        pod = ["api", "v1", "namespaces", "agents", "pods", "p"]
        self.assertEqual(published.operation([*pod, "resize"], "PATCH"), server.Operation(True, ("", "v1", "Pod")))
        self.assertEqual(published.operation([*pod, "binding"], "POST"), server.Operation(True, ("", "v1", "Binding")))
        self.assertEqual(published.operation([*pod, "exec"], "POST"), server.Operation(False, None))
        self.assertEqual(published.operation([*pod, "proxy", "a", "b"], "PUT"), server.Operation(False, None))
        self.assertIsNone(published.operation([*pod, "bogus"], "POST"))

    def test_every_quantity_in_a_manifest_reaches_kubernetes_as_kubernetes_writes_it(self):
        self.kube.answer("apply", ran({"kind": "List", "items": []}))
        container = {
            "name": "web",
            "image": "nginx",
            "env": [{"name": "SIZE", "value": "1e-10000000"}],
            "resources": {"limits": {"memory": ZEROS, "cpu": "@cpu@"}, "requests": {"memory": "1.5Gi"}},
        }
        manifests = [
            deployment(containers=[container], volumes=[{"name": "s", "emptyDir": {"medium": "Memory", "sizeLimit": SEVENS}}]),
            {"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": "c"}, "spec": {"resources": {"requests": {"storage": "1e-2147483648"}}}},
            {"apiVersion": "v1", "kind": "ResourceQuota", "metadata": {"name": "mine"}, "spec": {"hard": {"requests.memory": "0." + SEVENS}}},
            {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m"}, "data": {"zeros": ZEROS}},
        ]
        body = numbered({"manifests": manifests}, {"@cpu@": "1e-10000000"})
        self.signer.sender = sent_by(17)
        status, answer = self.call("POST", "/v1/manifests", body)
        self.assertEqual(status, 200, answer)
        (_, objects, _), (_, again, _) = [call for call in self.kube.calls if call[0] == "apply"]
        self.assertEqual(objects, again)
        made, claim, quota, config = json.loads(server.written(objects))
        spec = made["spec"]["template"]["spec"]
        self.assertEqual(spec["containers"][0]["resources"], {"limits": {"memory": MOST, "cpu": "1e-9"}, "requests": {"memory": "1536Mi"}})
        self.assertEqual(spec["containers"][0]["env"], [{"name": "SIZE", "value": "1e-10000000"}])
        self.assertEqual(spec["volumes"][0]["emptyDir"]["sizeLimit"], MOST)
        self.assertEqual(made["spec"]["replicas"], 1)
        self.assertEqual(made["metadata"]["labels"]["ducktape.industries/requester"], "17")
        self.assertEqual(claim["spec"]["resources"]["requests"]["storage"], "1e-9")
        self.assertEqual(quota["spec"]["hard"]["requests.memory"], "777777778n")
        self.assertEqual(config["data"]["zeros"], ZEROS)
        trial = [call[1] for call in self.kube.calls if call[0] == "trial"][0]
        self.assertEqual(json.loads(server.written(trial))["spec"]["containers"][0]["resources"]["limits"], {"memory": MOST, "cpu": "1e-9"})
        self.assertEqual(self.api.read, ["/openapi/v3", "/openapi/v3/apis/apps/v1?hash=a0", "/openapi/v3/api/v1?hash=c0"])

    def test_a_malformed_or_unpublished_manifest_is_refused_before_anything_applies(self):
        malformed = deployment(containers=[{"name": "web", "image": "nginx", "resources": {"limits": {"memory": "1e"}}}])
        words = ["manifest 0's spec.template.spec.containers[0].resources.limits.memory is not a Kubernetes quantity"]
        self.refused("POST", "/v1/manifests", {"manifests": [malformed]}, 400, words)
        self.refused("POST", "/v1/manifests", {"manifests": [{"apiVersion": "apps/v1", "kind": "Widget", "metadata": {"name": "w"}}]}, 400, ["apps/v1 Widget", "no schema"])
        self.refused("POST", "/v1/manifests", {"manifests": [{"apiVersion": "example.com/v1", "kind": "Widget"}]}, 400, ["no schema for apis/example.com/v1"])
        self.refused("POST", "/v1/manifests", {"manifests": [{"apiVersion": 1, "kind": "Pod"}]}, 400, ["manifest 0's 'apiVersion' is not a string"])
        self.refused("POST", "/v1/manifests", {"manifests": [{"apiVersion": "", "kind": "Pod"}]}, 400, ["apiVersion and a kind"])
        self.assertEqual([verb for verb in self.kube.verbs() if verb != "cluster_kinds"], [])

    def test_a_created_or_resized_pod_reaches_kubernetes_as_kubernetes_writes_it(self):
        pod = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"containers": [{"name": "c", "image": "i", "resources": {"limits": {"memory": ZEROS}}}], "overhead": {"cpu": "0.5"}}}
        resized = {"spec": {"containers": [{"name": "c", "resources": {"requests": {"cpu": "@cpu@"}}}]}}
        self.api.answers = [answered(200, {}), answered(200, PLACED), answered(200, {})]
        self.assertEqual(self.call("POST", PODS, pod)[0], 200)
        self.assertEqual(self.call("PUT", PODS + "/p/resize", numbered(resized, {"@cpu@": "1e-10000000"}))[0], 200)
        created, resize = (json.loads(call[3]) for call in self.api.calls if call[0] != "GET")
        self.assertEqual(created["spec"]["containers"][0]["resources"]["limits"]["memory"], MOST)
        self.assertEqual(created["spec"]["overhead"], {"cpu": "500m"})
        self.assertEqual(resize["spec"]["containers"][0]["resources"]["requests"]["cpu"], "1e-9")

    def test_every_patch_shape_reaches_kubernetes_as_kubernetes_writes_it(self):
        merge = {"spec": {"containers": [{"name": "c", "resources": {"limits": {"memory": ZEROS}}}]}}
        strategic = {
            "$patch": "merge",
            "spec": {
                "$setElementOrder/containers": [{"name": "c"}],
                "containers": [{"name": "c", "resources": {"limits": {"cpu": "1e-10000000"}}, "env": [{"name": "E", "value": ZEROS}]}],
            },
        }
        operations = [
            {"op": "replace", "path": "/spec/containers/0/resources/limits/memory", "value": ZEROS},
            {"op": "add", "path": "/spec/containers/-/resources", "value": {"requests": {"cpu": "0.5", "memory": "0." + SEVENS}}},
            {"op": "add", "path": "/spec/overhead/cpu", "value": "@cpu@"},
            {"op": "test", "path": "/spec/containers/0/resources/limits/cpu", "value": "1e-10000000"},
            {"op": "add", "path": "/metadata/annotations/size", "value": ZEROS},
            {"op": "add", "path": "/spec/overhead~1x", "value": ZEROS},
            {"op": "remove", "path": "/spec/overhead"},
        ]
        applied = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"overhead": {"memory": "1.5Gi"}}}
        cases = (
            ("application/merge-patch+json", merge, lambda sent: sent["spec"]["containers"][0]["resources"]["limits"], {"memory": MOST}),
            ("application/strategic-merge-patch+json", strategic, lambda sent: sent, {**strategic, "spec": {**strategic["spec"], "containers": [{**strategic["spec"]["containers"][0], "resources": {"limits": {"cpu": "1e-9"}}}]}}),
            (
                "application/json-patch+json",
                operations,
                lambda sent: sent,
                [
                    {**operations[0], "value": MOST},
                    {**operations[1], "value": {"requests": {"cpu": "500m", "memory": "777777778n"}}},
                    {**operations[2], "value": "1e-9"},
                    *operations[3:],
                ],
            ),
            ("application/apply-patch+yaml", applied, lambda sent: sent["spec"], {"overhead": {"memory": "1536Mi"}}),
        )
        for media, patch, part, expected in cases:
            self.api.calls, self.api.answers = [], [answered(200, PLACED), answered(200, PLACED)]
            status, answer = self.call("PATCH", PODS + "/p", numbered(patch, {"@cpu@": "1e-10000000"}), headers={**ASKED, "content-type": media})
            self.assertEqual(status, 200, answer)
            _, (_, _, dry, tried, _), (_, _, _, sent, kind) = self.api.calls
            self.assertEqual((dry, tried, kind), ("dryRun=All", sent, media))
            self.assertEqual(part(json.loads(sent)), expected, media)

    def test_a_strategic_directive_inside_a_map_of_quantities_is_a_directive(self):
        patch = {
            "spec": {
                "containers": [{"name": "c", "resources": {"limits": {"$patch": "replace", "cpu": "1e-10000000"}, "requests": {"$patch": "delete"}}}],
                "overhead": {"$patch": "replace", "$retainKeys": ["cpu"], "cpu": ZEROS},
            }
        }
        self.api.answers = [answered(200, PLACED), answered(200, PLACED)]
        status, answer = self.call("PATCH", PODS + "/p", json.dumps(patch).encode(), headers={**ASKED, "content-type": "application/strategic-merge-patch+json"})
        self.assertEqual(status, 200, answer)
        sent = json.loads(self.api.calls[-1][3])["spec"]
        self.assertEqual(sent["containers"][0]["resources"], {"limits": {"$patch": "replace", "cpu": "1e-9"}, "requests": {"$patch": "delete"}})
        self.assertEqual(sent["overhead"], {"$patch": "replace", "$retainKeys": ["cpu"], "cpu": MOST})
        self.api.calls = []
        merge = {"spec": {"overhead": {"$patch": "replace"}}}
        code, answer = self.call("PATCH", PODS + "/p", json.dumps(merge).encode(), headers={**ASKED, "content-type": "application/merge-patch+json"})
        self.assertEqual(code, 400, answer)
        self.assertIn("the body's spec.overhead.$patch is not a Kubernetes quantity", answer["error"])
        self.assertEqual(self.api.calls, [])

    def test_a_malformed_quantity_or_a_body_that_is_not_json_reaches_nothing(self):
        cases = (
            ("PATCH", PODS + "/p", [{"op": "add", "path": "/spec/overhead/cpu", "value": "48GB"}], "application/json-patch+json", 400, "the patch's operation 0's spec.overhead.cpu is not a Kubernetes quantity"),
            ("PATCH", PODS + "/p", {"spec": {"overhead": {"cpu": "48GB"}}}, "application/merge-patch+json", 400, "the body's spec.overhead.cpu is not a Kubernetes quantity"),
            ("PUT", PODS + "/p/resize", {"spec": {"overhead": {"cpu": "1e"}}}, None, 400, "the body's spec.overhead.cpu"),
            ("PUT", PODS + "/p/resize", b"\x0a\x02k8s", "application/vnd.kubernetes.protobuf", 415, "reads JSON only"),
            ("PATCH", PODS + "/p", b"\xa1", "application/apply-patch+cbor", 415, "reads JSON only"),
            ("POST", PODS, b"kind: Pod", "application/yaml", 415, "reads JSON only"),
            ("PATCH", PODS + "/p", b"kind: Pod", "application/apply-patch+yaml", 400, "not JSON"),
            ("POST", PODS + "/p/bogus", {"command": ["sh"]}, None, 400, "publishes no POST of /api/v1/namespaces/agents/pods/p/bogus"),
        )
        for method, path, body, media, status, words in cases:
            headers = ASKED if media is None else {**ASKED, "content-type": media}
            code, answer = self.call(method, path, body if isinstance(body, bytes) else json.dumps(body).encode(), headers=headers)
            self.assertEqual(code, status, answer)
            self.assertIn(words, answer["error"])
        self.assertEqual(self.api.calls, [])

    def test_a_schema_is_read_once_per_hash_and_again_when_it_changes(self):
        configmap = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "m"}, "data": {}}
        path = "/v1/k8s/api/v1/namespaces/agents/configmaps"
        self.call("POST", path, configmap)
        self.call("POST", path, configmap)
        self.api.published["api/v1"] = ("c1", CORE_V1)
        self.call("POST", path, configmap)
        self.assertEqual(self.api.read, ["/openapi/v3", "/openapi/v3/api/v1?hash=c0", "/openapi/v3", "/openapi/v3", "/openapi/v3/api/v1?hash=c1"])
        self.assertEqual(list(self.service.schemas.held), ["/openapi/v3/api/v1?hash=c1"])
        self.call("GET", path)
        self.assertEqual(len(self.api.read), 5)


CONFIGMAPS = "/v1/k8s/api/v1/namespaces/agents/configmaps"
CONFIGMAP = owned({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "x"}})
SLOW = "1e-2147483648"
LIMITED = {"containers": [{"name": "c", "image": "i", "resources": {"limits": {"cpu": SLOW}}}]}


def cpu_of(spec):
    return spec["containers"][0]["resources"]["limits"]["cpu"]


def typed(sent):
    return {key: value for key, value in sent.items() if key.casefold() in ("apiversion", "kind")}


class Kinds(Holder):
    def sent(self, method, path, body, media=None, answers=()):
        self.api.calls, self.api.answers = [], [answered(200, answer) for answer in answers]
        headers = ASKED if media is None else {**ASKED, "content-type": media}
        status, answer = self.call(method, path, body if isinstance(body, bytes) else json.dumps(body).encode(), headers=headers)
        self.assertEqual(status, 200, answer)
        return self.api.calls[-1][3]

    def test_a_strategic_patch_is_read_as_its_endpoints_kind_whatever_kind_it_states(self):
        forced = owned(deployment(priorityClassName="agents", automountServiceAccountToken=False))
        patch = {"apiVersion": "v1", "kind": "ConfigMap", "spec": {"template": {"spec": LIMITED}}}
        sent = json.loads(self.sent("PATCH", DEPLOYMENTS + "/web", patch, "application/strategic-merge-patch+json", [forced, forced]))
        self.assertEqual(typed(sent), {"apiVersion": "v1", "kind": "ConfigMap"})
        self.assertEqual(cpu_of(sent["spec"]["template"]["spec"]), "1e-9")

    def test_an_object_is_read_as_the_last_key_that_folds_to_kind_and_sent_with_that_kind_alone(self):
        body = b'{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"p"},"spec":' + json.dumps(LIMITED).encode() + b',"Kind":"Pod","kind":null}'
        sent = json.loads(self.sent("POST", PODS, body))
        self.assertEqual(typed(sent), {"apiVersion": "v1", "kind": "Pod"})
        self.assertEqual(cpu_of(sent["spec"]), "1e-9")
        self.assertEqual(sent["spec"]["priorityClassName"], "agents")

    def test_a_manifest_with_a_kelvin_sign_kind_is_applied_as_the_kind_kubernetes_reads(self):
        self.kube.answer("apply", ran({"kind": "List", "items": []}))
        manifest = {"apiVersion": "v1", "kind": "ConfigMap", "\u212aind": "Pod", "metadata": {"name": "m"}, "spec": LIMITED}
        status, answer = self.call("POST", "/v1/manifests", {"manifests": [manifest]})
        self.assertEqual(status, 200, answer)
        (applied,) = json.loads(server.written([call for call in self.kube.calls if call[0] == "apply"][0][1]))
        self.assertEqual(typed(applied), {"apiVersion": "v1", "kind": "Pod"})
        self.assertEqual(cpu_of(applied["spec"]), "1e-9")
        self.assertEqual(applied["spec"]["priorityClassName"], "agents")

    def test_a_merge_patch_is_read_as_the_kind_of_the_object_it_makes(self):
        cases = (
            ({"\u212aind": "Pod", "spec": LIMITED}, {"kind": "Pod"}, "1e-9"),
            ({"\u212aind": "Pod", "kind": "ConfigMap", "spec": LIMITED}, {"kind": "Pod"}, "1e-9"),
            ({"kind": "Pod", "spec": LIMITED}, {"kind": "Pod"}, "1e-9"),
            ({"Kind": "Pod", "spec": LIMITED}, {}, SLOW),
        )
        for patch, kind, cpu in cases:
            sent = json.loads(self.sent("PATCH", CONFIGMAPS + "/x", patch, "application/merge-patch+json", [CONFIGMAP, CONFIGMAP]))
            self.assertEqual(typed(sent), kind, patch)
            self.assertEqual(cpu_of(sent["spec"]), cpu, patch)

    def test_a_subresource_body_is_read_as_its_kind_with_the_rest_from_its_endpoint(self):
        sent = json.loads(self.sent("POST", PODS + "/p/binding", {"kind": "Pod", "metadata": {"name": "p"}, "spec": LIMITED}, None, [PLACED]))
        self.assertEqual((typed(sent), cpu_of(sent["spec"])), ({"kind": "Pod"}, "1e-9"))
        binding = {"apiVersion": "v1", "kind": "Binding", "metadata": {"name": "p"}, "target": {"name": "n"}}
        self.assertEqual(json.loads(self.sent("POST", PODS + "/p/binding", binding, None, [PLACED])), binding)
        for body in ({"kind": 5}, {"apiVersion": "v1", "Kind": ["Pod"]}):
            self.refused("POST", PODS + "/p/binding", body, 400, ["is not a string"])
        self.refused("POST", PODS + "/p/binding", {"apiVersion": "a/b/c"}, 400, ["not GROUP/VERSION"])

    def test_a_json_patch_that_makes_the_object_another_kind_is_refused(self):
        refused = (
            [{"op": "replace", "path": "/kind", "value": "Pod"}, {"op": "add", "path": "/spec", "value": LIMITED}],
            [{"op": "add", "path": "/\u212aind", "value": "Pod"}],
            [{"op": "replace", "path": "/apiVersion", "value": "apps/v1"}],
            [{"op": "replace", "path": "", "value": {"apiVersion": "v1", "kind": "Pod", "spec": LIMITED}}],
            [{"op": "copy", "from": "/metadata/name", "path": "/kind"}],
            [{"op": "add", "path": "/Kind", "value": "Pod"}, {"op": "move", "from": "/kind", "path": "/metadata/annotations/x"}, {"op": "add", "path": "/spec", "value": LIMITED}],
            [{"op": "add", "path": "/APIVERSION", "value": "apps/v1"}, {"op": "move", "from": "/apiVersion", "path": "/metadata/annotations/x"}],
            [{"op": "add", "path": "/Kind", "value": "Pod"}, {"op": "remove", "path": "/kind"}],
            [{"op": "add", "path": "/data", "value": {"kind": "Pod", "apiVersion": "v1"}}, {"op": "move", "from": "/data", "path": ""}],
            [{"op": "copy", "from": "/data", "path": ""}],
        )
        for patch in refused:
            self.api.calls = []
            code, answer = self.call("PATCH", CONFIGMAPS + "/x", json.dumps(patch).encode(), headers={**ASKED, "content-type": "application/json-patch+json"})
            self.assertEqual(code, 400, patch)
            self.assertIn(f"the patch's operation {next((at for at, change in enumerate(patch) if change['op'] in ('move', 'remove')), 0)}", answer["error"])
            self.assertEqual(self.api.calls, [])
        kept = [
            {"op": "add", "path": "/Kind", "value": "Pod"},
            {"op": "replace", "path": "/kind", "value": "ConfigMap"},
            {"op": "remove", "path": "/apiVersion"},
            {"op": "replace", "path": "", "value": {"apiVersion": "v1", "kind": "ConfigMap", "data": {"cpu": SLOW}}},
            {"op": "move", "from": "/kind", "path": "/metadata/annotations/x"},
        ]
        self.assertEqual(json.loads(self.sent("PATCH", CONFIGMAPS + "/x", kept, "application/json-patch+json", [CONFIGMAP, CONFIGMAP])), kept)

    def test_a_copy_or_move_is_refused_only_where_its_quantities_would_land_elsewhere(self):
        media = "application/json-patch+json"
        for op in ("copy", "move"):
            for target in ("/spec/containers/0/resources/limits/cpu", "/spec/containers/0/resources"):
                patch = [{"op": "add", "path": "/metadata/annotations", "value": {"a": SLOW}}, {"op": op, "from": "/metadata/annotations/a", "path": target}]
                self.api.calls = []
                code, answer = self.call("PATCH", PODS + "/p", json.dumps(patch).encode(), headers={**ASKED, "content-type": media})
                self.assertEqual(code, 400, patch)
                self.assertIn("the patch's operation 1", answer["error"])
                self.assertIn("write the quantity as a value (add or replace)", answer["error"])
                self.assertEqual(self.api.calls, [])
        alike = [
            {"op": "copy", "from": "/spec/containers/0/resources/limits/cpu", "path": "/spec/containers/0/resources/requests/cpu"},
            {"op": "move", "from": "/spec/overhead", "path": "/spec/resources/limits"},
            {"op": "copy", "from": "/metadata/annotations/a", "path": "/metadata/labels/a"},
        ]
        self.assertEqual(json.loads(self.sent("PATCH", PODS + "/p", alike, media, [PLACED, PLACED])), alike)

    def test_a_body_the_api_server_takes_no_schema_for_passes_as_sent(self):
        bodies = (
            (PODS + "/p/proxy/a/b", b"\x00 not json", "application/octet-stream"),
            (PODS + "/p/proxy", b'{"cpu": "1e-2147483648"}', "application/json"),
            (PODS + "/p/exec", b'{"Kind": 5}', None),
        )
        for path, body, media in bodies:
            self.assertEqual(self.sent("POST", path, body, media, [PLACED]), body)
        self.assertEqual(self.api.calls[-1][4], None)


STAMPED = {
    "ducktape.industries/chain": "duckpond",
    "ducktape.industries/program": "harness",
    "ducktape.industries/request": "41",
    "ducktape.industries/requester": "21",
}
MOVED = {"ducktape.industries/requester": "17"}
NOTED = "ducktape.industries/notify-channel"


def held_pod(labels=None, notes=None, claims=None):
    return {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "p", "uid": "u-p", "resourceVersion": "7", "labels": dict(STAMPED if labels is None else labels), "annotations": dict(notes or {})},
        "spec": {"priorityClassName": "agents", "automountServiceAccountToken": False, "volumes": [claimed("out", dict(STAMPED if claims is None else claims))]},
    }


def held_deployment(template_labels):
    made = deployment(priorityClassName="agents", automountServiceAccountToken=False)
    made["metadata"].update({"uid": "u-web", "resourceVersion": "7", "labels": dict(STAMPED)})
    made["spec"]["template"]["metadata"]["labels"] = {"app": "web", **template_labels}
    return made


class Stamped(Holder):
    def patched(self, path, patch, media, current, result, reads=1):
        self.api.calls, self.api.answers = [], [answered(200, current)] * reads + [answered(200, result), answered(200, result)]
        return self.call("PATCH", path, json.dumps(patch).encode(), headers={**ASKED, "content-type": media})

    def test_a_patch_setting_a_requester_on_ones_own_pod_is_refused_and_nothing_changes(self):
        patch = {"metadata": {"labels": MOVED}}
        code, answer = self.patched(PODS + "/p", patch, "application/merge-patch+json", held_pod(), held_pod(labels={**STAMPED, **MOVED}))
        self.assertEqual(code, 403, answer)
        self.assertIn("label ducktape.industries/requester on the object", answer["error"])
        self.assertIn("only the pods service writes ducktape.industries/* labels and annotations", answer["error"])
        self.assertEqual([(call[0], call[2]) for call in self.api.calls], [("GET", ""), ("PATCH", "dryRun=All")])

    def test_a_patch_that_sets_changes_or_removes_any_stamp_anywhere_is_refused(self):
        unrequested = {key: value for key, value in STAMPED.items() if key != "ducktape.industries/request"}
        cases = (
            (PODS + "/p", held_pod(), held_pod(labels={**STAMPED, **MOVED}), "label ducktape.industries/requester on the object"),
            (PODS + "/p", held_pod(), held_pod(labels=unrequested), "label ducktape.industries/request on the object"),
            (PODS + "/p", held_pod(), held_pod(labels={**STAMPED, "ducktape.industries/fleet-run": "7"}), "label ducktape.industries/fleet-run on the object"),
            (PODS + "/p", held_pod(notes={NOTED: "j-1"}), held_pod(notes={NOTED: "j-2"}), f"annotation {NOTED} on the object"),
            (PODS + "/p", held_pod(notes={NOTED: "j-1"}), held_pod(), f"annotation {NOTED} on the object"),
            (PODS + "/p", held_pod(), held_pod(notes={"ducktape.industries/notified": "ended"}), "annotation ducktape.industries/notified on the object"),
            (PODS + "/p/resize", held_pod(), held_pod(labels={**STAMPED, **MOVED}), "label ducktape.industries/requester on the object"),
            (PODS + "/p", held_pod(), held_pod(claims={**STAMPED, **MOVED}), "label ducktape.industries/requester on volume out's claim template"),
            (DEPLOYMENTS + "/web", held_deployment({}), held_deployment(MOVED), "label ducktape.industries/requester on its pod template"),
        )
        for path, current, result, words in cases:
            reads = 2 if path.endswith("/resize") else 1
            code, answer = self.patched(path, {"metadata": {}}, "application/merge-patch+json", current, result, reads)
            self.assertEqual(code, 403, (path, words, answer))
            self.assertIn(words, answer["error"])
            self.assertEqual([(call[0], call[2]) for call in self.api.calls], [("GET", "")] * reads + [("PATCH", "dryRun=All")], words)

    def test_a_patch_that_leaves_the_stamps_is_sent_held_to_the_version_it_was_judged_at(self):
        merge = {"spec": {"activeDeadlineSeconds": 5}}
        code, answer = self.patched(PODS + "/p", merge, "application/merge-patch+json", held_pod(), held_pod())
        self.assertEqual(code, 200, answer)
        self.assertEqual([(call[0], call[1], call[2]) for call in self.api.calls], [("GET", "/api/v1/namespaces/agents/pods/p", ""), ("PATCH", "/api/v1/namespaces/agents/pods/p", "dryRun=All"), ("PATCH", "/api/v1/namespaces/agents/pods/p", "")])
        held = {"spec": {"activeDeadlineSeconds": 5}, "metadata": {"resourceVersion": "7"}}
        self.assertEqual([json.loads(call[3]) for call in self.api.calls[1:]], [held, held])
        operations = [{"op": "replace", "path": "/spec/activeDeadlineSeconds", "value": 5}]
        code, answer = self.patched(PODS + "/p", operations, "application/json-patch+json", held_pod(), held_pod())
        self.assertEqual(code, 200, answer)
        tested = [{"op": "test", "path": "/metadata/resourceVersion", "value": "7"}, *operations]
        self.assertEqual([json.loads(call[3]) for call in self.api.calls[1:]], [tested, tested])

    def test_a_write_naming_another_version_than_the_one_judged_is_refused(self):
        code, answer = self.patched(PODS + "/p", {"metadata": {"resourceVersion": "6"}}, "application/merge-patch+json", held_pod(), held_pod())
        self.assertEqual(code, 409, answer)
        self.assertIn("names resourceVersion 6, and the object is at 7", answer.get("error", ""))
        self.assertEqual([call[0] for call in self.api.calls], ["GET"])

    def test_a_create_or_a_manifest_that_names_a_stamp_is_refused_before_kubernetes_sees_it(self):
        self.kube.answer("apply", ran({"kind": "List", "items": []}))
        labelled = deployment()
        labelled["metadata"]["labels"] = {"ducktape.industries/requester": "21"}
        noted = deployment()
        noted["spec"]["template"]["metadata"]["annotations"] = {NOTED: "j-1"}
        claiming = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"containers": [{"name": "c", "image": "i"}], "volumes": [claimed("v", {"ducktape.industries/requester": "21"})]}}
        cases = (
            (DEPLOYMENTS, labelled, "label ducktape.industries/requester on the object"),
            (DEPLOYMENTS, noted, f"annotation {NOTED} on its pod template"),
            (PODS, claiming, "label ducktape.industries/requester on volume v's claim template"),
        )
        for path, manifest, words in cases:
            self.refused("POST", path, manifest, 403, [words])
            self.refused("POST", "/v1/manifests", {"manifests": [manifest]}, 403, ["manifest 0", words])
        binding = {"apiVersion": "v1", "kind": "Binding", "metadata": {"name": "p", "annotations": {"ducktape.industries/requester": "21"}}, "target": {"name": "n"}}
        self.refused("POST", PODS + "/p/binding", binding, 403, ["annotation ducktape.industries/requester on the object"])
        self.assertEqual(self.api.calls, [])
        self.assertNotIn("apply", self.kube.verbs())

    def test_a_put_carries_the_stamps_the_object_holds(self):
        current = held_pod()
        given = held_pod()
        del given["metadata"]["resourceVersion"]
        self.api.answers = [answered(200, current)]
        status, answer = self.call("PUT", PODS + "/p", given)
        self.assertEqual(status, 200, answer)
        self.assertEqual([(call[0], call[1]) for call in self.api.calls], [("GET", "/api/v1/namespaces/agents/pods/p"), ("PUT", "/api/v1/namespaces/agents/pods/p")])
        sent = json.loads(self.api.calls[1][3])
        self.assertEqual((sent["metadata"]["resourceVersion"], sent["metadata"]["labels"]), ("7", STAMPED))
        dropped = held_pod(labels={key: value for key, value in STAMPED.items() if key != "ducktape.industries/requester"})
        relabelled = held_pod(labels={**STAMPED, **MOVED})
        for path, body, reads in ((PODS + "/p", dropped, 1), (PODS + "/p/resize", relabelled, 2)):
            self.api.calls, self.api.answers = [], [answered(200, current)] * reads
            self.refused("PUT", path, body, 403, ["label ducktape.industries/requester on the object"])
            self.assertEqual([call[0] for call in self.api.calls], ["GET"] * reads, path)

    def test_a_write_to_an_object_or_its_subresource_that_could_not_be_read_is_refused(self):
        self.api.answers = [answered(403, {"kind": "Status", "code": 403})]
        code, answer = self.call("PATCH", PODS + "/p", b'{"spec": {}}', headers=MERGE)
        self.assertEqual(code, 403, answer)
        self.assertIn("pods p was not read, so whose it is could not be told", answer.get("error", ""))
        self.assertEqual([call[0] for call in self.api.calls], ["GET"])
        self.api.calls, self.api.answers = [], [answered(200, held_pod()), answered(403, {"kind": "Status", "code": 403})]
        code, answer = self.call("PATCH", PODS + "/p/resize", b'{"spec": {}}', headers=MERGE)
        self.assertEqual(code, 403, answer)
        self.assertIn("/api/v1/namespaces/agents/pods/p/resize was not read before the write", answer.get("error", ""))
        self.assertEqual([call[0] for call in self.api.calls], ["GET", "GET"])


class Logged(Served):
    def test_a_request_is_logged_by_its_route_never_by_its_path(self):
        loud = server.ThreadingHTTPServer(("127.0.0.1", 0), server.handler(self.service))
        threading.Thread(target=loud.serve_forever, daemon=True).start()
        self.addCleanup(loud.server_close)
        self.addCleanup(loud.shutdown)
        said = []
        paths = {
            "/v1/pods": "pods",
            "/v1/pods/duckpond-41/logs?tail=5": "pods/NAME/logs",
            "/v1/manifests/deployment/web": "manifests/KIND/NAME",
            "/v1/k8s/api/v1/namespaces/kube-system/secrets/token?watch=1": "k8s",
            "/v1/pods/duckpond-41/token": "unknown",
            "/v1/nowhere/token": "unknown",
        }
        with unittest.mock.patch.object(server, "say", lambda event, **fields: said.append((event, fields))):
            for path in paths:
                connection = http.client.HTTPConnection(*loud.server_address)
                connection.request("GET", path, headers=ASKED)
                connection.getresponse().read()
                connection.close()
        logged = [fields for event, fields in said if event == "pods_request"]
        self.assertEqual([fields["route"] for fields in logged], list(paths.values()))
        for fields in logged:
            for secret in ("duckpond-41", "deployment", "web", "kube-system", "token", "tail", "watch"):
                self.assertNotIn(secret, " ".join(map(str, fields.values())))


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
                    "ephemeral-storage": {"usedBytes": 50, "time": "2026-10-22T03:21:07Z"},
                    "containers": [{"name": "run", "rootfs": {"time": "2026-10-22T03:21:00Z", "usedBytes": 40}}, {"name": "out", "rootfs": {"time": "2026-10-22T03:20:55Z", "usedBytes": 0}}],
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
        self.assertEqual([row["disk_read_at"] for row in answer["pods"]], ["2026-10-22T03:20:55Z", None, None, None])
        self.assertEqual(answer["claims"], [{"claim": "cache", "requester": 15, "storage_requested_bytes": 10 * 2**30, "pod": None, "phase": None}])
        self.assertEqual(answer["volume_disks"], [{"node": "fabrics-1", "used_bytes": 211394891776, "capacity_bytes": 887466962944}])

    def test_usage_lists_each_claim_kubernetes_made_for_a_pods_volume_with_its_pod_and_phase(self):
        building = pod_json(name="b", phase="Running", requester="15")
        building["spec"]["volumes"] = [claimed("out", {}), {"name": "cache", "persistentVolumeClaim": {"claimName": "cache"}}]
        ended = pod_json(name="e", phase="Succeeded", requester="15")
        labels = {"ducktape.industries/requester": "15"}
        made = lambda name, pod, storage: {"metadata": {"name": name, "labels": labels, "ownerReferences": [{"apiVersion": "v1", "kind": "Pod", "name": pod, "controller": True}]}, "spec": {"resources": {"requests": {"storage": storage}}}}
        cache = {"metadata": {"name": "cache", "labels": labels}, "spec": {"resources": {"requests": {"storage": "10Gi"}}}}
        self.kube.answer("list", ran({"items": [building, ended]}))
        self.kube.answer("claims", ran({"items": [made("b-out", "b", "1"), made("e-out", "e", "1"), made("g-out", "g", "2"), cache]}))
        self.kube.answer("quotas", ran({"items": []}))
        status, answer = self.call("GET", "/v1/usage")
        self.assertEqual(status, 200)
        self.assertEqual(
            answer["claims"],
            [
                {"claim": "b-out", "requester": 15, "storage_requested_bytes": 1, "pod": "b", "phase": "Running"},
                {"claim": "e-out", "requester": 15, "storage_requested_bytes": 1, "pod": "e", "phase": "Succeeded"},
                {"claim": "g-out", "requester": 15, "storage_requested_bytes": 2, "pod": "g", "phase": None},
                {"claim": "cache", "requester": 15, "storage_requested_bytes": 10 * 2**30, "pod": None, "phase": None},
            ],
        )
        (total,) = answer["requesters"]
        self.assertEqual((total["pods"], total["claims"], total["storage_requested_bytes"]), (2, 4, 10 * 2**30 + 4))
        self.assertEqual(answer["volume_disks"], [])

    def test_a_pods_disk_reading_says_when_containerd_last_measured_its_writable_layers(self):
        churning = pod_json(name="c", phase="Running", requester="15")
        churning["status"]["hostIP"] = "192.168.75.48"
        unread = pod_json(name="n", phase="Running", requester="15")
        unread["status"]["hostIP"] = "192.168.75.48"
        self.kube.answer("list", ran({"items": [churning, unread]}))
        self.kube.answer("claims", ran({"items": []}))
        self.kube.answer("quotas", ran({"items": []}))
        self.stats.summaries["192.168.75.48"] = {
            "node": {"nodeName": "fabrics-1"},
            "pods": [
                {
                    "podRef": {"name": "c", "namespace": "agents"},
                    "ephemeral-storage": {"usedBytes": 8800000, "time": "2026-10-22T03:15:00Z"},
                    "containers": [{"name": "run", "rootfs": {"time": "2026-10-21T16:58:00Z", "usedBytes": 8790000}}, {"name": "out", "rootfs": {"time": "2026-10-22T03:14:58Z", "usedBytes": 4096}}],
                },
                {
                    "podRef": {"name": "n", "namespace": "agents"},
                    "ephemeral-storage": {"usedBytes": 4096, "time": "2026-10-22T03:15:00Z"},
                    "containers": [{"name": "run"}, {"name": "out", "rootfs": {"time": "2026-10-22T03:14:58Z", "usedBytes": 4096}}],
                },
            ],
        }
        status, answer = self.call("GET", "/v1/usage")
        self.assertEqual(status, 200)
        self.assertEqual([(row["pod"], row["disk_bytes"], row["disk_read_at"]) for row in answer["pods"]], [("c", 8800000, "2026-10-21T16:58:00Z"), ("n", 4096, None)])

    def test_a_disk_reading_containerd_never_took_reads_null(self):
        never = pod_json(name="z", phase="Running", requester="15")
        never["status"]["hostIP"] = "192.168.75.48"
        half = pod_json(name="h", phase="Running", requester="15")
        half["status"]["hostIP"] = "192.168.75.48"
        self.kube.answer("list", ran({"items": [never, half]}))
        self.kube.answer("claims", ran({"items": []}))
        self.kube.answer("quotas", ran({"items": []}))
        zero = "1970-01-01T00:00:00Z"
        self.stats.summaries["192.168.75.48"] = {
            "node": {"nodeName": "fabrics-1"},
            "pods": [
                {
                    "podRef": {"name": "z", "namespace": "agents"},
                    "ephemeral-storage": {"usedBytes": 0, "time": "2026-10-22T09:30:00Z"},
                    "containers": [{"name": "run", "rootfs": {"time": zero, "usedBytes": 0}}, {"name": "out", "rootfs": {"time": zero, "usedBytes": 0}}],
                },
                {
                    "podRef": {"name": "h", "namespace": "agents"},
                    "ephemeral-storage": {"usedBytes": 4096, "time": "2026-10-22T09:30:00Z"},
                    "containers": [{"name": "run", "rootfs": {"time": zero, "usedBytes": 0}}, {"name": "out", "rootfs": {"time": "2026-10-22T09:29:58Z", "usedBytes": 4096}}],
                },
            ],
        }
        status, answer = self.call("GET", "/v1/usage")
        self.assertEqual(status, 200)
        self.assertEqual([(row["pod"], row["disk_read_at"]) for row in answer["pods"]], [("z", None), ("h", None)])

    def test_quantities_read_every_suffix(self):
        self.assertEqual(server.quantity("250m"), 0.25)
        self.assertEqual(server.quantity("2Gi"), 2 * 2**30)
        self.assertEqual(server.quantity("3k"), 3000)
        self.assertEqual(server.quantity("1e3"), 1000)
        self.assertEqual(server.quantity(" 96000000000000000000n "), 96 * 10**9)
        self.assertEqual(server.quantity("96000000000000000u"), 96 * 10**9)
        self.assertEqual(server.quantity("0.1G") + server.quantity("33.1G"), server.quantity("33.2G"))
        self.assertEqual(server.quantity("-1.5k"), -1500)
        with self.assertRaises(server.Refused):
            server.quantity("2Qi")

    def test_a_quantity_past_what_kubernetes_holds_reads_as_kubernetes_holds_it(self):
        nano = server.Fraction(1, 10**9)
        self.assertEqual(server.quantity("1e-10000000"), nano)
        self.assertEqual(server.quantity("1e-2147483648"), nano)
        self.assertEqual(server.quantity("-1e-2147483648"), -nano)
        self.assertEqual(server.quantity("0.0000000001"), nano)
        self.assertEqual(server.quantity("1.0000000001"), 1 + nano)
        self.assertEqual(server.quantity("0.5n"), nano)
        self.assertEqual(server.quantity("0e-2147483648"), 0)
        self.assertEqual(server.quantity("1e2147483647"), 2**63 - 1)
        self.assertEqual(server.quantity("-1e2147483647"), -(2**63 - 1))
        self.assertEqual(server.quantity("8Ei"), 2**63 - 1)
        self.assertEqual(server.quantity("7Ei"), 7 * 2**60)
        self.assertEqual(server.quantity("1e4294967296"), 1)
        self.assertEqual(server.quantity("1e" + "0" * 100000 + "3"), 1000)
        self.assertEqual(server.quantity("0." + "0" * 100000 + "1Ei"), nano)
        self.assertEqual(server.quantity("1" + "0" * 100000), 2**63 - 1)
        for past in ("1e9223372036854775808", "1e-9223372036854775809", "1e" + "9" * 100000):
            with self.assertRaises(server.Refused):
                server.quantity(past)


UNANSWERED = "ducktape: the node failed (524): error code: 524"


class Unanswering(Signer):
    def __init__(self, misses, why=UNANSWERED):
        super().__init__()
        self.misses = misses
        self.why = why

    def query(self, program, request):
        if program == "chat" and self.misses and request[:1] != server.u8(2):
            self.misses -= 1
            self.asked.append(request)
            return ran(code=1, err=self.why)
        return super().query(program, request)


class Reporting(Signer):
    def __init__(self):
        super().__init__()
        self.recorded = set()
        self.reports_asked = []

    def query(self, program, request):
        if program != server.FORGE_REACH:
            return super().query(program, request)
        self.reports_asked.append(request)
        reader = server.Reader(request)
        reader.u8()
        repo, commit = reader.bytes().decode(), reader.bytes().decode()
        items = [(context, url) for held_repo, held_commit, context, url in sorted(self.recorded) if (held_repo, held_commit) == (repo, commit)]
        rows = b"".join(server.u64(23) + server.borsh_text(context) + server.u8(1) + server.borsh_option(url, server.borsh_text) + server.u64(9) + server.borsh_vec([]) for context, url in items)
        return ran(server.u8(0) + server.u64(9) + len(items).to_bytes(4, "little") + rows + server.u8(0))


GATED = "a" * 40
OTHER = "b" * 40


def text(value):
    return len(value.encode()).to_bytes(4, "little") + value.encode()


def lane_run(run, commit=GATED, state="passed", reach="crates/a", repo="programs"):
    verdict = "GATES ok" if state == "passed" else "GATES FAIL: clippy"
    return f"job j-1\nrun {run}: {repo} {commit}, slot lane, lane\nGATE clippy ok\nreport sent: build.sh#lane {state}, reach: {reach}\n{verdict}\n"


class Reports(unittest.TestCase):
    def setUp(self):
        self.kube = Kube()
        self.signer = Reporting()
        self.notifier = server.Notifier(self.kube, self.signer, 1)
        self.notifier.spawn = lambda target, *args: target(*args)
        self.paused = []
        self.notifier.pause = self.paused.append
        self.runs = {}
        self.notifier.records = lambda name, run: self.runs.get((name, run))

    def report(self, url, commit=GATED, state="passed", reach=("crates/a",), repo="programs"):
        return ("forge-reach", server.reach_report({"repo": repo, "commit": commit, "context": "build.sh#lane", "state": state, "url": url, "reach": list(reach)}))

    def test_a_report_is_forge_reachs_report_op_in_its_own_field_order(self):
        said = {"repo": "programs", "commit": GATED, "context": "build.sh#lane", "state": "failed", "url": "desktop:r1", "reach": ["Cargo.lock", "crates/a"]}
        whole = b"\x01" + text("programs") + text(GATED) + text("build.sh#lane") + b"\x02" + b"\x01" + text("desktop:r1") + (2).to_bytes(4, "little") + text("Cargo.lock") + text("crates/a") + b"\x00"
        self.assertEqual(server.reach_report(said), whole)

    def test_a_lanes_report_line_goes_to_forge_reach_once_as_the_pods_own(self):
        self.kube.logs_of["duckpond-41"] = (
            f"ducktape-report programs {GATED} build.sh#lane passed r1 Cargo.lock crates/app/chat\n"
            f"ducktape-report core {OTHER} build.sh#lane failed r2\n"
            "ducktape-report programs abc123 build.sh#lane passed r3 x\n"
            f"ducktape-report programs {GATED} build.sh#lane maybe r4 x\n"
            f"ducktape-report programs {GATED}\n"
        ).encode()
        self.runs[("duckpond-41", "r1")] = lane_run("r1", reach="Cargo.lock crates/app/chat")
        self.runs[("duckpond-41", "r2")] = lane_run("r2", commit=OTHER, state="failed", reach="the whole tree", repo="core")
        for _ in range(2):
            self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        first = self.report("duckpond-41:r1", reach=("Cargo.lock", "crates/app/chat"))
        whole = self.report("duckpond-41:r2", commit=OTHER, state="failed", reach=("",), repo="core")
        self.assertEqual(self.signer.submits, [first, whole])

    def test_a_restart_reports_only_the_lines_newer_than_the_newest_forge_reach_holds(self):
        logged = "".join(f"ducktape-report programs {commit} build.sh#lane passed r{n} crates/a\n" for n, commit in ((1, GATED), (2, OTHER), (3, GATED))).encode()
        self.kube.answer("logs", ran(logged))
        self.kube.logs_of["duckpond-41"] = logged
        self.signer.recorded.add(("programs", OTHER, "build.sh#lane", "duckpond-41:r2"))
        self.runs[("duckpond-41", "r3")] = lane_run("r3")
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [self.report("duckpond-41:r3")])
        self.assertEqual(self.signer.reports_asked, [server.reach_reports("programs", GATED, None), server.reach_reports("programs", OTHER, None)])
        said.assert_any_call("pods_reports_caught_up", pod="duckpond-41", already_reported=2)

    def test_a_report_forge_reach_refuses_is_logged_and_not_sent_again(self):
        self.signer.submitted = ran(POOLED + b"landed 961 rejected invalid_input: reach path ./x is not a plain path\n")
        self.kube.logs_of["duckpond-41"] = f"ducktape-report programs {GATED} build.sh#lane passed r1 ./x\n".encode()
        self.runs[("duckpond-41", "r1")] = lane_run("r1", reach="./x")
        with unittest.mock.patch.object(server, "say") as said:
            for _ in range(2):
                self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [self.report("duckpond-41:r1", reach=("./x",))])
        said.assert_any_call("pods_unreported", run=json.dumps("duckpond-41:r1"), reason="invalid_input", detail=unittest.mock.ANY)

    def test_a_report_the_node_does_not_answer_is_sent_again(self):
        self.signer.submitted = [ran(code=1, err="ducktape: the node failed (502): bad gateway"), ran(POOLED + b"landed 961 applied 00ff\n")]
        self.kube.logs_of["duckpond-41"] = f"ducktape-report programs {GATED} build.sh#lane passed r1 crates/a\n".encode()
        self.runs[("duckpond-41", "r1")] = lane_run("r1")
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [self.report("duckpond-41:r1")] * 2)
        self.assertEqual(self.paused, [1])

    def test_a_line_no_build_machine_records_as_that_lane_is_never_reported(self):
        forged = {
            "agents-pod": ("r1", None),
            "sh-run": ("r2", f"job j-1\nrun r2: programs {GATED}, slot 1, sh echo ducktape-report programs {GATED} build.sh#lane passed r2 crates/a\nsh exit 0\n"),
            "other-reach": ("r3", lane_run("r3", reach="crates/b")),
            "other-verdict": ("r4", lane_run("r4", state="failed")),
            "other-commit": ("r5", lane_run("r5", commit=OTHER)),
        }
        lines = []
        for run, record in forged.values():
            if record is not None:
                self.runs[("duckpond-41", run)] = record
            lines.append(f"ducktape-report programs {GATED} build.sh#lane passed {run} crates/a\n")
        self.kube.logs_of["duckpond-41"] = "".join(lines).encode()
        with unittest.mock.patch.object(server, "say") as said:
            for _ in range(2):
                self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [])
        unrecorded = [call.kwargs["run"] for call in said.call_args_list if call.args == ("pods_unreported",) and call.kwargs.get("reason") == "unrecorded"]
        self.assertEqual(unrecorded, [json.dumps(f"duckpond-41:r{n}") for n in range(1, 6)])

    def test_a_line_whose_record_cannot_be_read_is_not_reported_nor_marked(self):
        self.kube.logs_of["duckpond-41"] = f"ducktape-report programs {GATED} build.sh#lane passed r1 crates/a\n".encode()

        def unread(name, run):
            raise server.Refused(502, "the warm pod's name was not read")

        self.notifier.records = unread
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.notifier.records = lambda name, run: lane_run("r1")
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [self.report("duckpond-41:r1")])

    def ended(self, channel, text, told):
        return ("chat", server.notice_message(channel, server.line_id("duckpond-41", channel, text), None, server.line_said("duckpond-41", told, 17)))

    def test_a_lanes_end_line_says_its_report_landed(self):
        self.kube.logs_of["duckpond-41"] = f"ducktape-report programs {GATED} build.sh#lane passed r1 crates/a\nducktape-notify j-1 r1 ended: exit 0, GATES ok\n".encode()
        self.runs[("duckpond-41", "r1")] = lane_run("r1")
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        text = "r1 ended: exit 0, GATES ok"
        told = f"{text} \u00b7 report landed: programs {GATED} build.sh#lane passed"
        self.assertEqual(self.signer.submits, [self.report("duckpond-41:r1"), self.ended("j-1", text, told)])

    def test_a_lanes_end_line_says_why_forge_reach_refused_its_report(self):
        self.signer.submitted = [ran(POOLED + b"landed 961 rejected invalid_input: reach path ./x is not a plain path\n"), ran(POOLED + b"landed 962 applied 00ff\n")]
        self.kube.logs_of["duckpond-41"] = f"ducktape-report programs {GATED} build.sh#lane passed r1 ./x\nducktape-notify j-1 r1 ended: exit 0, GATES ok\n".encode()
        self.runs[("duckpond-41", "r1")] = lane_run("r1", reach="./x")
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        text = "r1 ended: exit 0, GATES ok"
        told = f"{text} \u00b7 report not landed: invalid_input: reach path ./x is not a plain path"
        self.assertEqual(self.signer.submits, [self.report("duckpond-41:r1", reach=("./x",)), self.ended("j-1", text, told)])

    def test_each_end_line_of_a_lane_its_record_does_not_hold_says_so_and_other_runs_lines_say_nothing(self):
        self.kube.logs_of["duckpond-41"] = (
            f"ducktape-report programs {GATED} build.sh#lane passed r1 crates/a\n"
            "ducktape-notify j-2 r1 ended: exit 0, GATES ok\n"
            "ducktape-notify j-1 r1 ended: exit 0, GATES ok\n"
            "ducktape-notify j-1 r2 ended: exit 0, GATES ok\n"
        ).encode()
        self.runs[("duckpond-41", "r1")] = lane_run("r1", reach="crates/b")
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        text = "r1 ended: exit 0, GATES ok"
        told = f"{text} \u00b7 report not landed: unrecorded: duckpond-41 records no lane run r1 of programs {GATED} that ended passed with that reach"
        other = "r2 ended: exit 0, GATES ok"
        self.assertEqual(self.signer.submits, [self.ended("j-2", text, told), self.ended("j-1", text, told), self.ended("j-1", other, other)])

    def test_a_record_holds_whether_its_build_sh_says_report_sent_or_report(self):
        said = next(server.report_lines("duckpond-41", [f"ducktape-report programs {GATED} build.sh#lane passed r1 crates/a\n".encode()]))
        for lead in ("report sent:", "report:"):
            record = f"job j-1\nrun r1: programs {GATED}, slot lane, lane\n{lead} build.sh#lane passed, reach: crates/a\nGATES ok\n"
            self.assertTrue(server.lane_record_holds(said, record), lead)



def shots_tar(files):
    held = io.BytesIO()
    with tarfile.open(fileobj=held, mode="w") as archive:
        for name, data in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    return held.getvalue()


SHOT = "ducktape-files r1 /w/runs/r1/shots.tar /shared/jobs/J-1/shots\n"
SHOT_END = "ducktape-notify builds r1 ended: shot exit 0 (programs abc, slot quick)\n"


class Shots(unittest.TestCase):
    def setUp(self):
        self.kube = Kube()
        self.signer = Signer()
        self.signer.put_ran = ran(("\n".join(CHUNKS) + "\nbundle 8 put at block 950\n").encode())
        self.notifier = server.Notifier(self.kube, self.signer, 1)
        self.notifier.spawn = lambda target, *args: target(*args)
        self.paused = []
        self.notifier.pause = self.paused.append
        self.tars = {("duckpond-41", "/w/runs/r1/shots.tar"): shots_tar({"./a.png": b"abcdef", "phone/b.png": b"xy"})}
        self.read = []
        self.notifier.fetched = self.fetched
        chunk_size = server.CHUNK_SIZE
        server.CHUNK_SIZE = 4
        self.addCleanup(setattr, server, "CHUNK_SIZE", chunk_size)

    def fetched(self, name, path):
        self.read.append((name, path))
        if (name, path) not in self.tars:
            raise server.Refused(404, f"{path} was not read from pod {name}: No such file")
        return self.tars[(name, path)]

    def logged(self, text):
        self.kube.answer("logs", ran(text.encode()))
        self.kube.logs_of["duckpond-41"] = text.encode()

    def seen(self, times=1):
        for _ in range(times):
            self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))

    def sent(self, program):
        return [payload for sent_to, payload in self.signer.submits if sent_to == program]

    def written(self, base=SNAPSHOT):
        items = [("/shared/jobs/J-1/shots/a.png", 6, CHUNKS[:2]), ("/shared/jobs/J-1/shots/phone/b.png", 2, CHUNKS[2:])]
        return server.files_commit_many(base, "pods: duckpond-41 run r1 wrote /shared/jobs/J-1/shots", items)

    def test_a_commit_of_several_files_puts_each_in_order_and_one_file_reads_as_before(self):
        one = server.files_commit(SNAPSHOT, "m", "/a", 5, CHUNKS[:1])
        two = server.files_commit_many(SNAPSHOT, "m", [("/a", 5, CHUNKS[:1]), ("/b", 0, [])])
        self.assertEqual(one, server.files_commit_many(SNAPSHOT, "m", [("/a", 5, CHUNKS[:1])]))
        self.assertEqual(two[: len(one)][-len(server.files_put("/a", 5, CHUNKS[:1])) :], server.files_put("/a", 5, CHUNKS[:1]))
        self.assertEqual(two, server.u8(1) + server.borsh_option(SNAPSHOT, server.blob_id) + server.borsh_text("m") + server.borsh_vec([server.files_put("/a", 5, CHUNKS[:1]), server.files_put("/b", 0, [])]))

    def test_a_runs_shots_land_in_one_files_commit_and_its_end_notice_says_where(self):
        self.logged(SHOT + SHOT_END)
        self.seen()
        self.assertEqual(self.read, [("duckpond-41", "/w/runs/r1/shots.tar")])
        self.assertEqual(self.signer.puts, [([b"abcd", b"ef", b"xy"], "chunk")])
        self.assertEqual(self.sent("files"), [self.written()])
        (posted,) = self.sent("chat")
        detail = "r1 ended: shot exit 0 (programs abc, slot quick) \u00b7 shots written: 2 files under /shared/jobs/J-1/shots at block 960"
        self.assertIn(server.u8(server.STATUSES["passed"]) + server.borsh_text(detail), posted)

    def test_a_line_heard_again_writes_nothing_again(self):
        self.logged(SHOT + SHOT_END)
        self.seen(times=2)
        self.assertEqual(self.sent("files"), [self.written()])
        self.assertEqual(len(self.read), 1)

    def test_a_restart_writes_only_the_shots_of_runs_whose_end_chat_does_not_hold_yet(self):
        self.tars[("duckpond-41", "/w/runs/r2/shots.tar")] = shots_tar({"c.png": b"c"})
        later = "ducktape-files r2 /w/runs/r2/shots.tar /shared/jobs/J-1/later\nducktape-notify builds r2 ended: shot exit 0 (x)\n"
        self.logged(SHOT + SHOT_END + later)
        self.signer.messages.add(server.line_id("duckpond-41", "builds", SHOT_END.removeprefix("ducktape-notify builds ").strip()))
        self.seen()
        self.assertEqual(self.read, [("duckpond-41", "/w/runs/r2/shots.tar")])
        (written,) = self.sent("files")
        self.assertIn(b"/shared/jobs/J-1/later/c.png", written)

    def test_a_tar_naming_a_path_outside_its_folder_writes_nothing_and_the_end_says_why(self):
        self.tars[("duckpond-41", "/w/runs/r1/shots.tar")] = shots_tar({"a.png": b"a", "../x": b"x"})
        self.logged(SHOT + SHOT_END)
        self.seen()
        self.assertEqual((self.signer.puts, self.sent("files")), ([], []))
        (posted,) = self.sent("chat")
        self.assertIn("shots not written: the shots' tar holds '../x', which is no path under /shared/jobs/J-1/shots".encode(), posted)

    def test_a_tar_the_machine_cannot_read_writes_nothing_and_the_end_says_why(self):
        self.tars.clear()
        self.logged(SHOT + SHOT_END)
        self.seen()
        self.assertEqual(self.sent("files"), [])
        (posted,) = self.sent("chat")
        self.assertIn(b"shots not written: /w/runs/r1/shots.tar was not read from pod duckpond-41: No such file", posted)

    def test_files_refusing_the_commit_is_final_and_the_end_fails_saying_its_reason(self):
        self.signer.submitted = [ran(POOLED + b"landed 960 rejected unauthorized: /shared/jobs is written by its write list only\n"), ran(POOLED + b"landed 961 applied 00ff\n")]
        self.logged(SHOT + SHOT_END)
        self.seen()
        self.assertEqual(self.sent("files"), [self.written()])
        (posted,) = self.sent("chat")
        detail = "r1 ended: shot exit 0 (programs abc, slot quick) \u00b7 shots not written: unauthorized: /shared/jobs is written by its write list only"
        self.assertIn(server.u8(server.STATUSES["failed"]) + server.borsh_text(detail), posted)

    def test_a_commit_the_node_does_not_answer_is_made_again(self):
        self.signer.submitted = [ran(code=1, err="ducktape: the node failed (502): bad gateway"), ran(POOLED + b"landed 961 applied 00ff\n")]
        self.logged(SHOT + SHOT_END)
        self.seen()
        self.assertEqual(self.sent("files"), [self.written()] * 2)
        self.assertEqual(self.paused, [1])

    def test_a_commit_files_moved_under_is_made_again_onto_the_head_read_then(self):
        moved = "sha256:" + "cd" * 32
        self.signer.head = [ran(bytes([0, 1, 0]) + bytes.fromhex("ab" * 32)), ran(bytes([0, 1, 0]) + bytes.fromhex("cd" * 32))]
        self.signer.submitted = [ran(POOLED + b"landed 960 rejected stale: the head moved\n"), ran(POOLED + b"landed 961 applied 00ff\n")]
        self.logged(SHOT + SHOT_END)
        self.seen()
        self.assertEqual(self.sent("files"), [self.written(), self.written(moved)])


class ArchiveRetention(unittest.TestCase):
    fetched = Shots.fetched
    sent = Shots.sent

    def setUp(self):
        Shots.setUp(self)
        self.folder = "/shared/runs/programs/20261023-120000-run"
        self.parent = "/shared/runs/programs"
        self.said = {"run": "r1", "tar": "/w/runs/r1/shots.tar", "to": self.folder, "prune_before": "20261009"}
        self.pages = []
        self.listed = []
        self.exists = True
        original = self.signer.query
        def query(program, request):
            if program == "files" and request[:1] == server.u8(3):
                self.listed.append(request)
                entry = server.u8(1) + self.entry(self.parent, 1) if self.exists else server.u8(0)
                return ran(server.u8(3) + server.u8(1) + server.blob_id(SNAPSHOT) + entry)
            if program == "files" and request[:1] == server.u8(4):
                self.listed.append(request)
                return self.pages.pop(0)
            return original(program, request)
        self.signer.query = query

    def entry(self, path, kind, meta=()):
        node = {
            0: server.u8(0) + server.blob_id(SNAPSHOT) + server.u64(2) + server.u8(0),
            1: server.u8(1) + server.blob_id(SNAPSHOT) + server.u64(1),
            2: server.u8(2) + server.borsh_text("target"),
        }[kind]
        return server.borsh_text(path) + node + server.borsh_vec([server.borsh_text(k) + server.borsh_text(v) for k, v in meta])

    def page(self, entries, after=None):
        return ran(server.u8(4) + server.u8(1) + server.blob_id(SNAPSHOT) + server.u64(950) + server.borsh_vec(entries) + server.borsh_option(after, server.borsh_bytes))

    def test_the_optional_cutoff_is_a_calendar_date_not_part_of_the_destination(self):
        line = f"ducktape-files r1 /w/runs/r1/shots.tar {self.folder} prune-before 20261009\n"
        self.assertEqual(list(server.files_lines([line.encode()])), [self.said])
        plain = dict(self.said)
        del plain["prune_before"]
        self.assertEqual(list(server.files_lines([line.split(" prune-before")[0].encode()])), [plain])
        for suffix in ("20260230", "20260001", "00001009", "2026109", "tomorrow", "20261009 extra", ""):
            invalid = f"ducktape-files r1 /w/runs/r1/shots.tar {self.folder} prune-before {suffix}\n"
            self.assertEqual(list(server.files_lines([invalid.encode()])), [], suffix)
        self.assertEqual(list(server.files_lines([f"ducktape-files r1 /w/runs/r1/shots.tar {self.folder} prune-before".encode()])), [])

    def test_old_sibling_directories_across_pages_are_removed_in_the_write_commit(self):
        old = self.parent + "/20261008-old"
        older = self.parent + "/20260901-older"
        self.pages = [
            self.page([self.entry(old, 1, [("label", "old")]), self.entry(self.parent + "/20261008-file", 0)], b"next"),
            self.page([self.entry(older, 1), self.entry(self.parent + "/20261008-link", 2), self.entry(self.parent + "/20261009-equal", 1), self.entry(self.folder, 1), self.entry(self.parent + "/20261022-new", 1), self.entry(self.parent + "/notes", 1), self.entry(self.parent + "/20260230-invalid", 1)]),
        ]
        self.notifier.file("duckpond-41", self.said)
        items = [(self.folder + "/a.png", 6, CHUNKS[:2]), (self.folder + "/phone/b.png", 2, CHUNKS[2:])]
        expected = server.files_commit_many(SNAPSHOT, f"pods: duckpond-41 run r1 wrote {self.folder}", items, [old, older])
        self.assertEqual(self.sent("files"), [expected])
        self.assertEqual(self.listed, [server.files_stat(self.parent, SNAPSHOT), server.files_list(self.parent, SNAPSHOT, None), server.files_list(self.parent, SNAPSHOT, b"next")])
        self.assertIn(self.folder, self.notifier.told("duckpond-41", "r1 ended: exit 1"))

    def test_the_destination_is_kept_even_when_its_date_is_old(self):
        self.said["prune_before"] = "20261101"
        self.pages = [self.page([self.entry(self.folder, 1)])]
        self.notifier.file("duckpond-41", self.said)
        items = [(self.folder + "/a.png", 6, CHUNKS[:2]), (self.folder + "/phone/b.png", 2, CHUNKS[2:])]
        self.assertEqual(self.sent("files"), [server.files_commit_many(SNAPSHOT, f"pods: duckpond-41 run r1 wrote {self.folder}", items)])

    def test_a_first_archive_has_no_siblings(self):
        self.exists = False
        self.notifier.file("duckpond-41", self.said)
        self.assertEqual(len(self.sent("files")), 1)
        self.assertEqual(self.listed, [server.files_stat(self.parent, SNAPSHOT)])

    def test_a_stale_commit_reads_the_new_siblings_before_committing_again(self):
        old = self.parent + "/20261008-old"
        moved = "sha256:" + "cd" * 32
        self.signer.head = [ran(server.u8(0) + server.u8(1) + server.blob_id(SNAPSHOT)), ran(server.u8(0) + server.u8(1) + server.blob_id(moved))]
        self.signer.submitted = [ran(POOLED + b"landed 960 rejected stale: the head moved\n"), ran(POOLED + b"landed 961 applied 00ff\n")]
        self.pages = [self.page([self.entry(old, 1)]), self.page([])]
        self.notifier.file("duckpond-41", self.said)
        first, second = self.sent("files")
        self.assertTrue(first.endswith(server.u8(2) + server.borsh_text(old)))
        self.assertNotIn(old.encode(), second)
        self.assertEqual(self.listed[-2:], [server.files_stat(self.parent, moved), server.files_list(self.parent, moved, None)])

    def test_a_listing_failure_writes_nothing(self):
        self.pages = [ran(code=1, err="query refused: invalid cursor")]
        self.notifier.file("duckpond-41", self.said)
        self.assertEqual(self.sent("files"), [])
        self.assertIn("siblings were not read", self.notifier.told("duckpond-41", "r1 ended: exit 1"))


class Posts(unittest.TestCase):
    def setUp(self):
        self.kube = Kube()
        self.signer = Signer()
        self.notifier = server.Notifier(self.kube, self.signer, 1)
        self.paused = []
        self.notifier.pause = self.paused.append

    def test_a_room_query_the_node_does_not_answer_is_asked_again_until_it_answers(self):
        self.signer = Unanswering(3)
        self.notifier = server.Notifier(self.kube, self.signer, 1)
        self.notifier.pause = self.paused.append
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [("chat", POSTED)])
        self.assertEqual(self.paused, [1, 2, 4])
        retried = [call.kwargs["attempts"] for call in said.call_args_list if call.args == ("pods_retried",)]
        self.assertEqual(retried, [1, 2])
        self.assertNotIn("pods_unposted", [call.args[0] for call in said.call_args_list])
        self.assertIn("annotate", self.kube.verbs())

    def test_a_submit_the_node_does_not_answer_is_sent_again_and_a_repeat_counts_as_posted(self):
        self.signer.submitted = [ran(code=1, err="ducktape: the node failed (502): bad gateway"), ran(POOLED + b"landed 961 rejected already_exists: message pods-duckpond-41-ended is posted\n")]
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [("chat", POSTED)] * 2)
        self.assertEqual(self.paused, [1])
        self.assertIn("annotate", self.kube.verbs())

    def test_a_room_query_chat_answers_with_a_failure_is_not_asked_again(self):
        self.signer = Unanswering(1, why="ducktape: chat could not read its query")
        self.notifier = server.Notifier(self.kube, self.signer, 1)
        self.notifier.pause = self.paused.append
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual((self.signer.submits, self.paused), ([], []))

    def test_the_post_is_chats_notify_of_a_notice_for_the_requester(self):
        ended = server.ended_said(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(ended, {"group": "duckpond-41", "subject": "pod duckpond-41", "status": "passed", "detail": "ended: exit code 0 (Completed)", "concerns": [17]})
        self.assertEqual(server.notice_message("builds", "pods-duckpond-41-ended", None, ended), POSTED)
        threaded = server.notice_message("builds", "pods-duckpond-41-ended", 5, ended)
        self.assertEqual(threaded[:37], POSTED[:37])
        self.assertEqual(threaded[37:46], b"\x01" + (5).to_bytes(8, "little"))
        self.assertEqual(threaded[46:], POSTED[38:])
        failed = server.ended_said(pod_json(state=FAILED, phase="Failed", notes=NOTIFY))
        self.assertEqual(failed["status"], "failed")

    def deleted(self, mark, deleting=True, **given):
        pod = pod_json(**{"state": SIGTERMED, "phase": "Failed", "init": FETCHED, **given, "notes": {**NOTIFY, **({DELETED_MARK: f"{mark} {ASKED_AT}"} if mark else {})}})
        if deleting:
            pod["metadata"]["deletionTimestamp"] = "2026-10-22T09:31:00Z"
            pod["metadata"]["deletionGracePeriodSeconds"] = 30
        return pod

    def test_a_pod_the_kubelet_deletes_again_once_its_containers_stopped_still_reads_deleted_by_its_requester(self):
        pod = self.deleted("uid-duckpond-41")
        pod["metadata"]["deletionTimestamp"] = "2026-10-22T09:30:32Z"
        pod["metadata"]["deletionGracePeriodSeconds"] = 0
        self.assertEqual(server.ended_said(pod)["status"], "note")
        self.assertEqual(server.ended_event(pod)["line"], "pod duckpond-41 ended: deleted by its requester")

    def test_a_run_that_ended_on_its_own_before_its_requesters_delete_still_reads_its_exit(self):
        for code, reason, status in ((1, "Error", "failed"), (0, "Completed", "passed")):
            with self.subTest(code=code):
                pod = self.deleted("uid-duckpond-41", state={"terminated": {"exitCode": code, "reason": reason, "startedAt": "s", "finishedAt": "2026-10-22T09:30:29Z"}})
                said = server.ended_said(pod)
                self.assertEqual((said["status"], said["detail"]), (status, f"ended: exit code {code} ({reason})"))
                self.assertEqual(server.ended_event(pod)["line"], f"pod duckpond-41 ended: exit code {code} ({reason})")
        at_the_delete = self.deleted("uid-duckpond-41", state={"terminated": {**SIGTERMED["terminated"], "finishedAt": "2026-10-22T09:30:30Z"}})
        self.assertEqual(server.ended_said(at_the_delete)["status"], "note")

    def test_a_pod_its_requesters_delete_ended_is_noted_deleted_by_its_requester(self):
        pod = self.deleted("uid-duckpond-41")
        said = {"group": "duckpond-41", "subject": "pod duckpond-41", "status": "note", "detail": "ended: deleted by its requester", "concerns": [17]}
        self.assertEqual(server.ended_said(pod), said)
        self.assertEqual(server.ended_line(pod), "pod duckpond-41 ended: deleted by its requester")
        fetching = self.deleted("uid-duckpond-41", state=None, init=SIGTERMED)
        self.assertEqual(server.ended_line(fetching), "pod duckpond-41 ended: deleted by its requester")
        self.assertIsNone(server.ended_line(self.deleted("uid-duckpond-41", state=RUNNING, phase="Running")))
        self.notifier.seen(pod)
        self.assertEqual(self.signer.submits, [("chat", server.notice_message("builds", "pods-duckpond-41-ended", None, said))])

    def test_a_pod_no_delete_of_its_requesters_ended_still_reads_its_exit(self):
        timeless = self.deleted(None)
        timeless["metadata"]["annotations"][DELETED_MARK] = "uid-duckpond-41"
        cases = {"another pod's mark": self.deleted("uid-an-older-pod"), "no mark": self.deleted(None), "a mark with no delete": self.deleted("uid-duckpond-41", deleting=False), "a mark with no ask time": timeless}
        for case, pod in cases.items():
            with self.subTest(case=case):
                said = server.ended_said(pod)
                self.assertEqual((said["status"], said["detail"]), ("failed", "ended: exit code 143 (Error)"))

    def test_a_post_without_a_requester_concerns_no_one(self):
        posted = server.notice_message("builds", "m", None, server.line_said("p", "ended", None))
        self.assertTrue(posted.endswith(b"\x00\x00\x00\x00"))

    def test_a_pods_notify_thread_reaches_its_notice(self):
        notes = {**NOTIFY, server.NOTIFY_THREAD: "5"}
        pod = pod_json(state=ENDED, phase="Succeeded", notes=notes)
        self.notifier.seen(pod)
        posted = server.notice_message("builds", "pods-duckpond-41-ended", 5, server.ended_said(pod))
        self.assertEqual(self.signer.submits, [("chat", posted)])

    def test_a_notice_chat_rejects_is_logged_unposted_with_its_reason_and_not_marked(self):
        self.signer.submitted = ran(POOLED + b"landed 961 rejected invalid_input: there is no account 17\n")
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [("chat", POSTED)])
        said.assert_any_call("pods_unposted", message="pods-duckpond-41-ended", reason="invalid_input", detail=unittest.mock.ANY)
        self.assertNotIn("annotate", self.kube.verbs())

    def test_a_lines_group_is_the_run_it_names_else_the_pod(self):
        cases = {
            "20261019-1 ended: exit 0, GATES ok": "duckpond-41:20261019-1",
            "run 20261019-1 lost with pod duckpond-41: it ended before the run did": "duckpond-41:20261019-1",
            "relay this": "duckpond-41",
        }
        self.assertEqual({text: server.line_said("duckpond-41", text, 17)["group"] for text in cases}, cases)

    def test_a_notify_for_line_concerns_the_caller_it_names(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.kube.logs_of["duckpond-41"] = b"ducktape-notify-for 33 j-093 20261019-1 ended: exit 0, GATES ok\nducktape-notify-for x j-093 20261019-2 ended: exit 0, GATES ok\n"
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        text = "20261019-1 ended: exit 0, GATES ok"
        posted = server.notice_message("j-093", server.line_id("duckpond-41", "j-093", text), None, server.line_said("duckpond-41", text, 33))
        self.assertEqual(self.signer.submits, [("chat", posted)])

    def test_a_notify_for_number_no_u64_holds_is_no_notify_for_line_and_the_next_line_still_posts(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        past = str(2**64)
        long = "9" * 5000
        lines = [f"ducktape-notify-for {past} j-093 r0 ended: exit 0", f"ducktape-notify-for {long} j-093 r0 ended: exit 0", "ducktape-notify-for 33 j-093 20261019-1 ended: exit 0, GATES ok"]
        self.kube.logs_of["duckpond-41"] = "".join(f"{line}\n" for line in lines).encode()
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        text = "20261019-1 ended: exit 0, GATES ok"
        posted = server.notice_message("j-093", server.line_id("duckpond-41", "j-093", text), None, server.line_said("duckpond-41", text, 33))
        self.assertEqual(self.signer.submits, [("chat", posted)])

    def test_the_catch_up_walk_finds_a_held_notice_by_the_id_each_posts_it_under(self):
        text = "r1 ended: exit 0, GATES ok"
        message_id = server.line_id("desktop", "j-1", text)
        self.assertEqual(message_id, server.notify_id("desktop", "j-1", f"desktop \u00b7 {text}"))
        self.signer.messages.add(message_id)
        self.assertEqual(self.notifier.passed("desktop", [("j-1", text, None)]), (1, 1, None))
        self.assertEqual(self.notifier.posted, {"desktop": {message_id}})
        self.assertEqual(self.notifier.each("desktop", None, [("j-1", text, None)], unvouched=True), 0)
        self.assertEqual(self.signer.submits, [])

    def test_a_lines_status_is_its_runs_exit_and_a_lost_run_or_unwritten_shots_fail(self):
        cases = {
            "20261019-1 ended: exit 0, GATES ok": "passed",
            "20261019-1 ended: exit 1, GATES FAIL: test": "failed",
            "20261019-1 ended: exit 143, stopped by build.sh stop": "failed",
            "20261019-1 ended: sh exit 0 (core c293b001070e, slot 2)": "passed",
            "20261019-1 ended: sh exit 1 (core c293b001070e, slot 2)": "failed",
            "20261019-1 ended: shot exit 0 (programs 6064e86c9aa5, slot shot)": "passed",
            "20261019-1 ended: shot exit 1 (programs 6064e86c9aa5, slot shot)": "failed",
            "20261019-1 ended: shot exit 0 (programs 6064e86c9aa5, slot shot) \u00b7 shots written: 2 files under /shared/j/shots at block 960": "passed",
            "20261019-1 ended: shot exit 0 (programs 6064e86c9aa5, slot shot) \u00b7 shots not written: unauthorized: /shared/j is written by its write list only": "failed",
            "run 20261019-1 lost with pod duckpond-41: it ended before the run did": "failed",
            "relay this": "note",
        }
        self.assertEqual({text: server.line_status(text) for text in cases}, cases)

    def test_an_ended_pod_is_posted_once_and_marked(self):
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [("chat", POSTED)])
        read = ("logs", "duckpond-41", ["--container", "run"])
        self.assertEqual(self.kube.calls, [read, ("annotate", "duckpond-41", "ducktape.industries/notified", "ended")])
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes={**NOTIFY, "ducktape.industries/notified": "ended"}))
        self.assertEqual(len(self.signer.submits), 1)

    def test_a_pod_the_scheduler_cannot_place_posts_its_reason_once_and_still_posts_its_end(self):
        self.notifier.seen(unschedulable(pod_json(notes=NOTIFY)))
        said = {"group": "duckpond-41", "subject": "pod duckpond-41", "status": "note", "detail": f"pending, waiting to be placed: {UNSCHEDULABLE}", "concerns": [17]}
        self.assertEqual(self.signer.submits, [("chat", server.notice_message("builds", "pods-duckpond-41-pending", None, said))])
        self.assertEqual(self.kube.calls, [("annotate", "duckpond-41", "ducktape.industries/notified-pending", "posted")])
        marked = {**NOTIFY, "ducktape.industries/notified-pending": "posted"}
        self.notifier.seen(unschedulable(pod_json(notes=marked)))
        self.assertEqual(len(self.signer.submits), 1)
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=marked))
        self.assertEqual(self.signer.submits[1:], [("chat", POSTED)])

    def test_a_pending_pod_the_scheduler_has_not_refused_or_with_no_room_to_post_in_posts_nothing(self):
        self.notifier.seen(pod_json(notes=NOTIFY))
        self.notifier.seen(unschedulable(pod_json(notes=NOTIFY), reason="SchedulingGated"))
        self.notifier.seen(unschedulable(pod_json()))
        self.assertEqual((self.signer.submits, self.kube.calls), ([], []))

    def test_a_pending_notice_chat_refuses_is_not_marked_so_the_next_event_posts_it_again(self):
        self.signer.submitted = ran(POOLED + b"landed 961 rejected invalid_input: there is no account 17\n")
        self.notifier.seen(unschedulable(pod_json(notes=NOTIFY)))
        self.assertEqual((len(self.signer.submits), self.kube.calls), (1, []))
        self.notifier.seen(unschedulable(pod_json(notes=NOTIFY)))
        self.assertEqual(len(self.signer.submits), 2)

    def test_a_pod_that_ended_unplaced_posts_its_end_and_no_pending_notice(self):
        pod = unschedulable(pod_json(phase="Failed", notes=NOTIFY))
        self.notifier.seen(pod)
        self.assertEqual(self.signer.submits, [("chat", server.notice_message("builds", "pods-duckpond-41-ended", None, server.ended_said(pod)))])

    def test_a_pod_still_running_or_asking_for_no_post_is_not_posted(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.notifier.seen(pod_json(state={"running": {}}, phase="Running", notes=NOTIFY))
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded"))
        self.assertEqual((self.signer.submits, self.kube.calls), ([], [("logs", "duckpond-41", ["--container", "run"])]))

    def test_a_notified_pods_notify_line_is_posted_led_by_the_pod(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        self.kube.logs_of["duckpond-41"] = b"building\nducktape-notify j-093 20261019-1 ended: exit 0, GATES ok\n"
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        line = "duckpond-41 \u00b7 20261019-1 ended: exit 0, GATES ok"
        posted = notified("j-093", server.notify_id("duckpond-41", "j-093", line), 17, line, None)
        self.assertEqual(self.signer.submits, [("chat", posted)])
        self.assertEqual((self.kube.follows, self.kube.calls, self.notifier.following), (["duckpond-41"], [("logs", "duckpond-41", ["--container", "run"])], set()))

    def test_a_followed_pod_asks_chat_only_back_to_the_newest_line_it_holds(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        logged = b"[r1] a line\nducktape-notify j-1 r1 ended: exit 0, GATES ok\nducktape-notify j-2 r2 ended: exit 1, GATES FAIL: test\nducktape-notify j-1 r3 ended: exit 0, GATES ok\n"
        lines = [("j-1", "duckpond-41 \u00b7 r1 ended: exit 0, GATES ok"), ("j-2", "duckpond-41 \u00b7 r2 ended: exit 1, GATES FAIL: test"), ("j-1", "duckpond-41 \u00b7 r3 ended: exit 0, GATES ok")]
        ids = [server.notify_id("duckpond-41", channel, line) for channel, line in lines]
        self.signer.messages.update(ids[:2])
        self.kube.answer("logs", ran(logged))
        self.kube.logs_of["duckpond-41"] = logged
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        self.assertEqual([request for request in self.signer.asked if request in map(server.chat_message, ids)], [server.chat_message(ids[2]), server.chat_message(ids[1]), server.chat_message(ids[2])])
        self.assertEqual(self.signer.submits, [("chat", notified("j-1", ids[2], 17, lines[2][1], None))])
        said.assert_any_call("pods_caught_up", pod="duckpond-41", already_posted=2, asked=2)

    def test_a_followed_pod_whose_log_cannot_be_read_asks_each_line_as_it_comes(self):
        self.notifier.spawn = lambda target, *args: target(*args)
        logged = b"ducktape-notify j-1 r1 ended: exit 0, GATES ok\nducktape-notify j-1 r2 ended: exit 0, GATES ok\n"
        lines = [("j-1", f"duckpond-41 \u00b7 r{n} ended: exit 0, GATES ok") for n in (1, 2)]
        ids = [server.notify_id("duckpond-41", channel, line) for channel, line in lines]
        self.signer.messages.add(ids[0])
        self.kube.answer("logs", ran(code=1, err="Error from server: the container is starting"))
        self.kube.logs_of["duckpond-41"] = logged
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))
        said.assert_any_call("pods_uncaught", pod="duckpond-41", detail=json.dumps("Error from server: the container is starting"))
        self.assertEqual([request for request in self.signer.asked if request in map(server.chat_message, ids)], [server.chat_message(id) for id in ids])
        self.assertEqual(self.signer.submits, [("chat", notified("j-1", ids[1], 17, lines[1][1], None))])

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
        posted = notified("j-093", server.notify_id("duckpond-41", "j-093", line), 17, line, None)
        self.assertEqual(self.signer.submits, [("chat", posted), ("chat", POSTED)])
        self.assertEqual(self.notifier.posted, {})

    def test_a_run_a_dead_pod_held_is_posted_lost_in_its_channel_and_one_that_ended_is_not(self):
        self.kube.answer("logs", ran(
            b"ducktape-run armed j-101 20261019-1 app b2ae apply /home/tech/work/j-101/snap.sh\n"
            b"ducktape-run armed j-080 20261019-2 tools 3f52 sh make check\n"
            b"ducktape-run done 20261019-2\n"
            b"ducktape-notify j-080 20261019-2 ended: exit 0, GATES ok\n"
            b"[20261019-1] fetching app\n"
        ))
        self.notifier.seen(pod_json(phase="Failed", reason="NodeShutdown", notes=NOTIFY))
        ended = "duckpond-41 \u00b7 20261019-2 ended: exit 0, GATES ok"
        lost = "duckpond-41 \u00b7 run 20261019-1 lost with pod duckpond-41: it ended before the run did, NodeShutdown, so nothing of it runs now; call it again: app b2ae apply /home/tech/work/j-101/snap.sh"
        posts = [
            ("chat", notified("j-080", server.notify_id("duckpond-41", "j-080", ended), 17, ended, None)),
            ("chat", notified("j-101", lost_id("j-101", "20261019-1"), 17, lost, None)),
        ]
        self.assertEqual(self.signer.submits[:2], posts)
        self.assertEqual(len(self.signer.submits), 3)

    def test_a_pod_whose_runs_all_ended_posts_none_lost(self):
        self.kube.answer("logs", ran(b"ducktape-run armed j-101 20261019-1 app b2ae gates\nducktape-run done 20261019-1\n"))
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.signer.submits, [("chat", POSTED)])

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
        with unittest.mock.patch.object(server, "say") as said:
            self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        said.assert_any_call("pods_posted", message="pods-duckpond-41-ended", outcome="rejected", reason="already_exists")
        self.assertEqual(self.kube.verbs(), ["logs", "logs", "annotate"])

    def test_a_post_no_block_carried_is_made_again_until_one_does(self):
        self.signer.submitted = [ran(POOLED + b"unlanded\n"), ran(POOLED + b"landed 960 applied 00ff\n")]
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual((self.kube.verbs(), self.paused), (["logs", "annotate"], [1]))

    def test_a_post_that_was_not_sent_is_not_marked(self):
        self.signer.submitted = ran(code=1, err="node unreachable")
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual(self.kube.verbs(), ["logs"])

    def test_without_a_key_nothing_is_posted_or_marked(self):
        self.signer.keyed = False
        self.notifier.seen(pod_json(state=ENDED, phase="Succeeded", notes=NOTIFY))
        self.assertEqual((self.signer.submits, self.kube.verbs()), ([], ["logs"]))


ARMED = (
    b"ducktape-run armed j-1 r1 app b2ae gates\n"
    b"ducktape-run armed j-2 r2 core c0ff sh make check\n"
    b"ducktape-run armed j-3 r3 programs 1a2b quick -p chat\n"
    b"ducktape-run done r3\n"
)
KILLED = {"terminated": {"exitCode": 137, "reason": "OOMKilled", "startedAt": "s", "finishedAt": "f"}}


class Lost(unittest.TestCase):
    def setUp(self):
        self.kube = Kube()
        self.signer = Signer()
        self.notifier = self.fresh()

    def fresh(self):
        notifier = server.Notifier(self.kube, self.signer, 1)
        notifier.pause = lambda pause: None
        notifier.spawn = lambda target, *args: target(*args)
        return notifier

    def followed_with_its_armed_lines_rotated_away(self):
        self.kube.answer("logs", ran(b""), ran(b"[r2] building\n"))
        self.kube.logs_of["duckpond-41"] = ARMED
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))

    def lost(self, why):
        posts = []
        for channel, run, call in (("j-1", "r1", "app b2ae gates"), ("j-2", "r2", "core c0ff sh make check")):
            text = f"run {run} lost with pod duckpond-41: {why}, so nothing of it runs now; call it again: {call}"
            posts.append(("chat", server.notice_message(channel, lost_id(channel, run), None, server.line_said("duckpond-41", text, 17))))
        return posts

    def test_a_queued_and_a_running_run_are_each_posted_lost_once_when_their_pod_dies_after_its_log_rotated(self):
        self.followed_with_its_armed_lines_rotated_away()
        killed = pod_json(state=KILLED, phase="Failed", notes=NOTIFY)
        self.notifier.seen(killed)
        ended = server.notice_message("builds", "pods-duckpond-41-ended", None, server.ended_said(killed))
        self.assertEqual(self.signer.submits, self.lost("it ended before the run did, exit code 137 (OOMKilled)") + [("chat", ended)])
        self.notifier.seen(pod_json(state=KILLED, phase="Failed", notes={**NOTIFY, server.NOTIFIED: "ended"}))
        self.notifier.event({"type": "DELETED", "object": killed})
        self.assertEqual(len(self.signer.submits), 3)

    def test_a_queued_and_a_running_run_are_each_posted_lost_once_when_their_pod_is_deleted(self):
        self.followed_with_its_armed_lines_rotated_away()
        running = pod_json(state=RUNNING, phase="Running", notes=NOTIFY)
        for _ in range(2):
            self.notifier.event({"type": "DELETED", "object": running})
        self.assertEqual(self.signer.submits, self.lost("it was deleted before the run ended"))

    def test_a_deleting_pods_log_is_read_once_more_so_runs_carried_away_before_the_delete_are_not_lost(self):
        self.followed_with_its_armed_lines_rotated_away()
        self.kube.answer("logs", ran(b"ducktape-run done r1\nducktape-run done r2\n"))
        deleting = pod_json(state=RUNNING, phase="Running", notes=NOTIFY)
        deleting["metadata"]["deletionTimestamp"] = "2026-10-21T07:30:00Z"
        self.notifier.following.add("duckpond-41")
        self.notifier.event({"type": "MODIFIED", "object": deleting})
        self.notifier.event({"type": "DELETED", "object": deleting})
        self.assertEqual(self.signer.submits, [])

    def test_runs_kept_by_a_pods_service_that_restarted_after_their_pod_was_deleted_are_posted_lost_once(self):
        self.followed_with_its_armed_lines_rotated_away()
        live = pod_json(name="duckpond-42", state=RUNNING, phase="Running", notes=NOTIFY)
        self.kube.logs_of["duckpond-42"] = b"ducktape-run armed j-4 r4 app b2ae gates\n"
        self.notifier.seen(live)
        self.assertEqual(self.signer.submits, [])
        self.kube.answer("list", ran({"items": [live]}))
        for _ in range(2):
            with self.assertRaises(Unwatched):
                self.fresh().watch()
        self.assertEqual(self.signer.submits, self.lost("it was gone when the pods service looked"))


ARGV = {
    "r1": ["NOTIFY=j-1", "JOB=j-1", "app", "b2ae", "gates"],
    "r2": ["NOTIFY=j-2", "JOB=j-2", "core", "c0ff", "sh", "make check"],
}


def argv_line(run, words):
    return f"ducktape-run argv {run} {server.base64.b64encode(chr(0).join(words).encode() + bytes(1)).decode()}\n".encode()


class Recalled(unittest.TestCase):
    def setUp(self):
        self.kube = Kube()
        self.signer = Signer()
        self.recalls = []
        self.answers = ["desktop:d1", "desktop:d2"]
        self.notifier = self.fresh()

    def fresh(self):
        notifier = Lost.fresh(self)
        notifier.recall = self.recall
        return notifier

    def recall(self, words):
        self.recalls.append(words)
        answer = self.answers.pop(0)
        if isinstance(answer, server.Refused):
            raise answer
        return answer

    def armed(self, argv=ARGV):
        self.kube.answer("logs", ran(b""), ran(b"[r2] building\n"))
        self.kube.logs_of["duckpond-41"] = ARMED + b"".join(argv_line(run, words) for run, words in argv.items())
        self.notifier.seen(pod_json(state=RUNNING, phase="Running", notes=NOTIFY))

    def said(self, channel, run, text, message=None):
        return ("chat", server.notice_message(channel, message or lost_id(channel, run), None, server.line_said("duckpond-41", text, 17)))

    def moved(self, channel, run, new, why, message=None):
        return self.said(channel, run, f"run {run} is now run {new}: pod duckpond-41 went before it ended ({why}), so the pods service called it again once: wait on that one", message)

    def test_a_run_whose_pod_died_is_called_again_once_with_its_words_and_posted_as_moved_never_as_lost(self):
        self.armed()
        self.notifier.event({"type": "DELETED", "object": pod_json(state=RUNNING, phase="Running", notes=NOTIFY)})
        why = "it was deleted before the run ended"
        self.assertEqual(self.recalls, [ARGV["r1"], ARGV["r2"]])
        self.assertEqual(self.signer.submits, [self.moved("j-1", "r1", "desktop:d1", why), self.moved("j-2", "r2", "desktop:d2", why)])
        self.assertEqual(server.line_status("run r1 is now run desktop:d1: pod duckpond-41 went before it ended (x), so the pods service called it again once: wait on that one"), "note")
        self.assertEqual(self.kube.kept, {})

    def test_a_run_already_called_again_once_is_posted_lost(self):
        self.armed({"r1": ["NOTIFY=j-1", "JOB=j-1", "AGAIN=1", "app", "b2ae", "gates"], "r2": ARGV["r2"]})
        self.notifier.event({"type": "DELETED", "object": pod_json(state=RUNNING, phase="Running", notes=NOTIFY)})
        why = "it was deleted before the run ended"
        self.assertEqual(self.recalls, [ARGV["r2"]])
        lost = self.said("j-1", "r1", f"run r1 lost with pod duckpond-41: {why}, so nothing of it runs now; call it again: app b2ae gates")
        self.assertEqual(self.signer.submits, [lost, self.moved("j-2", "r2", "desktop:d1", why)])

    def test_a_run_no_runner_takes_waits_kept_and_the_next_builds_call_calls_it_again(self):
        self.answers = [server.Refused(503, "the desktop is unreachable: lan: refused"), "desktop:d2"]
        self.armed()
        self.notifier.event({"type": "DELETED", "object": pod_json(state=RUNNING, phase="Running", notes=NOTIFY)})
        why = "it was deleted before the run ended"
        waits = self.said("j-1", "r1", f"run r1 waits to be called again: pod duckpond-41 went before it ended ({why}), and no build machine took it (the desktop is unreachable: lan: refused); the next builds call calls it again")
        self.assertEqual(self.signer.submits, [waits, self.moved("j-2", "r2", "desktop:d2", why)])
        self.assertEqual(list(self.kube.kept), [server.pending_key("duckpond-41", "r1")])
        later = self.fresh()
        self.kube.answer("list", ran({"items": []}))
        with self.assertRaises(Unwatched):
            later.watch()
        self.assertEqual(list(later.pending), [server.pending_key("duckpond-41", "r1")])
        later.call_pending(lambda words: (self.recalls.append(words), "duckpond-43:w1")[1])
        self.assertEqual(self.recalls[-1], ARGV["r1"])
        self.assertEqual(self.signer.submits[-1], self.moved("j-1", "r1", "duckpond-43:w1", why, server.recalled_id("duckpond-41", "j-1", "r1")))
        self.assertEqual((self.kube.kept, later.pending), ({}, {}))

    def test_a_pending_run_is_called_again_by_one_builds_call_while_another_runs_beside_it(self):
        self.answers = [server.Refused(503, "the desktop is unreachable"), "desktop:d2"]
        self.armed()
        self.notifier.event({"type": "DELETED", "object": pod_json(state=RUNNING, phase="Running", notes=NOTIFY)})
        beside = []

        def recall(words):
            self.notifier.call_pending(lambda again: (beside.append(again), "duckpond-43:w2")[1])
            return "duckpond-43:w1"

        self.notifier.call_pending(recall)
        self.assertEqual((beside, self.notifier.pending, self.kube.kept), ([], {}, {}))

    def test_a_pending_run_a_builds_call_could_not_place_stays_pending_for_the_next(self):
        self.answers = [server.Refused(503, "the desktop is unreachable"), "desktop:d2"]
        self.armed()
        self.notifier.event({"type": "DELETED", "object": pod_json(state=RUNNING, phase="Running", notes=NOTIFY)})
        key = server.pending_key("duckpond-41", "r1")

        def refused(words):
            raise server.Refused(502, "/shared/tools/warm-pod names no warm pod")

        self.notifier.call_pending(refused)
        self.assertEqual((list(self.notifier.pending), list(self.kube.kept)), ([key], [key]))
        self.notifier.call_pending(lambda words: "duckpond-43:w1")
        self.assertEqual((self.notifier.pending, self.kube.kept), ({}, {}))

    def test_a_run_a_stopped_service_was_calling_again_is_never_called_a_second_time(self):
        record = {"pod": "duckpond-41", "run": "r1", "channel": "j-1", "call": "app b2ae gates", "requester": 17, "argv": server.base64.b64encode(chr(0).join(ARGV["r1"]).encode()).decode(), "recalling": True}
        self.kube.kept[server.kept_key("duckpond-41", "r1")] = json.dumps(record)
        self.kube.answer("list", ran({"items": []}))
        with self.assertRaises(Unwatched):
            self.fresh().watch()
        self.assertEqual(self.recalls, [])
        text = "run r1 may have been called again by a pods service that stopped before it said so (pod duckpond-41 went before it ended: it was gone when the pods service looked): read build.sh busy on each build machine before calling it again"
        self.assertEqual(self.signer.submits, [self.said("j-1", "r1", text)])


class KeptRuns(unittest.TestCase):
    def test_the_runs_configmap_is_made_when_missing_and_changed_by_merge_patches_in_the_services_namespace(self):
        kube = server.Kube(["kubectl"], "agents")
        answers = [server.Ran(1, b"", 'Error from server (NotFound): configmaps "runs" not found'), server.Ran(0, b'{"metadata": {"name": "runs"}}', "")]
        calls = []

        def ran_kubectl(argv, stdin=None, env=None):
            calls.append(argv)
            return answers.pop(0) if answers else server.Ran(0, b"", "")

        with unittest.mock.patch.object(server, "run", ran_kubectl):
            self.assertEqual(kube.kept_load(), {})
            kube.kept_change({"k": "v", "gone": None})
        patch = json.dumps({"data": {"k": "v", "gone": None}})
        self.assertEqual(calls, [
            ["kubectl", "--namespace", "pods", "get", "configmap", "runs", "--output", "json"],
            ["kubectl", "--namespace", "pods", "create", "configmap", "runs", "--output", "json"],
            ["kubectl", "--namespace", "pods", "patch", "configmap", "runs", "--type", "merge", "--patch", patch],
        ])


LANE = "a96fa67643795cf0b5359e416e31a55232d0e79a"
LANE_ROOM = "forge:programs:561"
DEV = b"refs/heads/dev"
LIVE_REQUIRED = bytes.fromhex(
    "12b8cd050000000000b8cd050000000000010000000e000000726566732f68656164732f646576003c00000000000000020000000900000035362f7265766965771000000032332f6275696c642e7368236c616e6500"
)
LIVE_QUEUE = bytes.fromhex(
    "13b9cd050000000000b9cd050000000000010000003102000000000000280000003664633235343263353365626363383437623434373363623032396137346532613434623535633600110000000000000081ad0500000000000155000000987f00000000000021000000150800000070726f6772616d730e000000726566732f68656164732f646576000024000000652f70726f6772616d730000726566732f68656164732f64657600000000000000000091"
)
LIVE_PREVIEW = bytes.fromhex(
    "10bacd05000000000001310200000000000028000000323539366235616665343765633237656266393862336137666539336636346532653461356632332800000036646332353432633533656263633834376234343733636230323961373465326134346235356336280000006139366661363736343337393563663062353335396534313665333161353532333264306537396158c4050000000000"
)
LIVE_STATUSES = bytes.fromhex(
    "11bacd050000000000bacd05000000000002000000003c000000000000001000000032332f6275696c642e7368236c616e65000058c4050000000000003c000000000000000900000035362f72657669657701012b0000006475636b3a2f2f6475636b706f6e642d37323562636164312f666f7267652f70726f6772616d732f35363158c405000000000000"
)
LIVE_REPOS = bytes.fromhex(
    "004fd50500000000004fd5050000000000020000000800000070726f6772616d73010012000000000000000f000000726566732f68656164732f6d61696e0000bf030000000000003ed505000000000004000000636f7265010012000000000000000f000000726566732f68656164732f6d61696e00001c02000000000000dfd4050000000000012300000086800000000000000300000000000010000000612ffffffffffffa2b20636f72650000"
)
ASK_REQUIRED = server.forge_required("programs", None)
ASK_FRONT = server.forge_queue_front("programs", DEV)
ASK_PREVIEW = server.forge_preview("programs", 561)
ASK_STATUSES = server.forge_statuses("programs", LANE, None)
MOVED_PREVIEW = "7e" * 20


def principal(account):
    return server.u8(0) + server.u64(account)


def forge_page(tag, items, after=None):
    return server.u8(tag) + server.u64(1) + server.u64(1) + server.borsh_vec(items) + server.borsh_option(after, server.borsh_bytes)


def required_reply(*contexts, after=None):
    item = server.borsh_bytes(DEV) + principal(60) + server.borsh_vec([server.borsh_text(context) for context in contexts])
    return forge_page(18, [item], after)


def queue_reply(*changes):
    return forge_page(19, [server.u64(n) + server.borsh_text("4e" * 20) + principal(17) + server.u64(1) for n in changes])


def preview_reply(n, merge):
    info = server.u64(n) + server.borsh_text("1e" * 20) + server.borsh_text("2e" * 20) + server.borsh_text(merge) + server.u64(1)
    return server.u8(16) + server.u64(1) + server.borsh_option(info, bytes)


def repos_reply(*names):
    return forge_page(0, [server.borsh_text(name) + server.u8(1) + principal(21) + server.borsh_bytes(b"refs/heads/main") + server.u8(0) + server.u8(0) + server.u64(1) + server.u64(1) for name in names])


def check_status(context, state, reporter=60):
    return principal(reporter) + server.borsh_text(context) + server.u8(server.CHECK_STATES.index(state)) + server.borsh_option(None, server.borsh_text) + server.u64(1)


def statuses_reply(*items, after=None):
    return forge_page(17, list(items), after)


def forge_key(table, repo):
    return (table + repo.encode() + b"\x00\x00" + b"rest").hex()


def required_repo(request):
    return server.Reader(request[1:]).bytes().decode()


def lane_of(command):
    return command[-3:-1]


class Forging:
    def __init__(self, answers, account=23):
        self.answers = answers
        self.account = account
        self.queries = []
        self.follows = []
        self.streams = []

    def key(self):
        return b"k" * 32

    def query(self, program, request):
        self.queries.append((program, request))
        if program == "identity":
            return ran(server.u8(0) + server.borsh_option(self.account, server.u64))
        if program == "forge-reach":
            return ran(server.u8(2) + server.u64(1) + server.borsh_vec([]) + server.borsh_option(None, server.borsh_bytes))
        return ran(self.answers[request])

    def follow(self, program):
        self.follows.append(program)
        return Log(self.streams.pop(0))


class Reruns(unittest.TestCase):
    def setUp(self):
        self.kube = Kube()
        self.signer = Forging({ASK_REQUIRED: LIVE_REQUIRED, ASK_TRAIN: server.u8(25) + server.u64(1) + b"\x00\x00", ASK_FRONT: LIVE_QUEUE, ASK_PREVIEW: LIVE_PREVIEW, ASK_STATUSES: LIVE_STATUSES})
        self.launched = []
        self.answers = [ran(b"run desktop:r1\nread it with: build.sh status r1\n")]
        self.calls = []
        self.call_answer = ran(b"bumped\n")
        self.posts = []
        self.spawned = []
        self.pauses = []
        self.rerunner = self.fresh()

    def fresh(self):
        rerunner = server.Rerunner(self.signer, self.kube, self.launch, 1, self.post)
        rerunner.reporter = 23
        rerunner.spawn = lambda target, *args: self.spawned.append((target, args))
        rerunner.pause = self.pauses.append
        rerunner.called = self.call
        return rerunner

    def call(self, command):
        self.calls.append(command)
        if isinstance(self.call_answer, server.Refused):
            raise self.call_answer
        return self.call_answer

    def launch(self, command):
        self.launched.append((command, dict(self.kube.kept)))
        answer = self.answers.pop(0) if len(self.answers) > 1 else self.answers[0]
        if isinstance(answer, server.Refused):
            raise answer
        return answer

    def post(self, channel, message_id, said):
        self.posts.append((channel, message_id, said["detail"]))
        return True

    def record(self, **more):
        return json.dumps({"repo": "programs", "n": 561, "commit": LANE, **more})

    def asked(self):
        return [request for program, request in self.signer.queries if program == "forge"]

    def test_the_live_forge_replies_read_as_the_queue_front_and_its_previews_statuses(self):
        self.assertEqual(server.required_of(LIVE_REQUIRED), ([{"branch": DEV, "reporter": ("account", 60), "contexts": ["56/review", "23/build.sh#lane"]}], None))
        self.assertEqual(server.queue_front_of(LIVE_QUEUE), 561)
        self.assertEqual(server.preview_of(LIVE_PREVIEW), LANE)
        statuses, after = server.statuses_of(LIVE_STATUSES)
        read = [(found["reporter"], found["context"], found["state"]) for found in statuses]
        self.assertEqual((read, after), ([(("account", 60), "23/build.sh#lane", "pending"), (("account", 60), "56/review", "passed")], None))
        names, after = server.repos_of(LIVE_REPOS)
        self.assertEqual((names, after is not None), (["programs", "core"], True))

    def test_its_forge_queries_are_the_bytes_forge_reads(self):
        programs = server.borsh_text("programs")
        self.assertEqual(ASK_REQUIRED, b"\x14" + programs + b"\x00\x00")
        self.assertEqual(ASK_FRONT, b"\x15" + programs + server.borsh_bytes(DEV) + b"\x00\x01" + server.u64(1))
        self.assertEqual(ASK_PREVIEW, b"\x12" + programs + server.u64(561))
        self.assertEqual(ASK_STATUSES, b"\x13" + programs + server.borsh_text(LANE) + b"\x00\x00")

    def test_the_queue_fronts_pending_lane_is_kept_first_then_run_once_through_builds_in_its_change_room(self):
        self.rerunner.settle("programs")
        self.rerunner.settle("programs")
        key = server.rerun_key("programs", LANE)
        command = ["sh", server.BUILD_SH, f"JOB={LANE_ROOM}", f"NOTIFY={LANE_ROOM}", "programs", LANE, "lane"]
        self.assertEqual(self.launched, [(command, {key: self.record()})])
        self.assertEqual(self.kube.kept, {key: self.record(run="desktop:r1")})
        self.assertEqual((self.posts, self.spawned), ([], []))

    def test_a_started_front_lane_is_bumped_to_the_front_of_its_machines_check_lane(self):
        self.rerunner.settle("programs")
        self.rerunner.settle("programs")
        self.assertEqual(self.calls, [["sh", server.BUILD_SH, "bump", "desktop:r1"]])

    def test_a_lane_that_did_not_start_is_not_bumped(self):
        self.answers = [server.Refused(503, "no build runner is free")]
        self.rerunner.settle("programs")
        self.assertEqual(self.calls, [])

    def test_a_refused_bump_leaves_the_lane_kept_and_running(self):
        self.call_answer = server.Refused(502, "pod desktop was not reached")
        self.rerunner.settle("programs")
        self.assertEqual(self.kube.kept, {server.rerun_key("programs", LANE): self.record(run="desktop:r1")})
        self.assertEqual((len(self.calls), self.spawned), (1, []))

    def test_a_retried_lane_that_starts_is_bumped(self):
        self.answers = [server.Refused(503, "no build runner is free"), ran(b"run desktop:r2\n")]
        self.rerunner.settle("programs")
        target, args = self.spawned[0]
        target(*args)
        self.assertEqual(self.calls, [["sh", server.BUILD_SH, "bump", "desktop:r2"]])

    def test_a_kept_lane_whose_preview_left_the_front_is_stopped_and_one_that_never_started_is_not(self):
        ran_key = server.rerun_key("programs", MOVED_PREVIEW)
        never_key = server.rerun_key("programs", "8e" * 20)
        app_key = server.rerun_key("app", "9e" * 20)
        self.kube.kept = {
            ran_key: json.dumps({"repo": "programs", "n": 559, "commit": MOVED_PREVIEW, "run": "duckpond-warm:r0"}),
            never_key: json.dumps({"repo": "programs", "n": 577, "commit": "8e" * 20, "unstarted": "no build runner is free"}),
            app_key: json.dumps({"repo": "app", "n": 3, "commit": "9e" * 20, "run": "desktop:r7"}),
        }
        self.rerunner.settle("programs")
        self.assertEqual(self.calls, [["sh", server.BUILD_SH, "stop", "duckpond-warm:r0"], ["sh", server.BUILD_SH, "bump", "desktop:r1"]])
        self.assertEqual(sorted(self.kube.kept), sorted([app_key, server.rerun_key("programs", LANE)]))

    def test_a_refused_stop_still_drops_the_key(self):
        key = server.rerun_key("programs", MOVED_PREVIEW)
        self.kube.kept = {key: json.dumps({"repo": "programs", "n": 559, "commit": MOVED_PREVIEW, "run": "desktop:r0"})}
        self.call_answer = server.Refused(502, "pod desktop was not reached")
        self.rerunner.settle("programs")
        self.assertNotIn(key, self.kube.kept)

    def test_a_front_no_lane_ever_reported_on_gets_its_lane(self):
        self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("56/review", "passed"))
        self.rerunner.settle("programs")
        self.assertEqual([lane_of(command) for command, _ in self.launched], [["programs", LANE]])

    def test_only_the_queues_front_is_read_and_laned(self):
        self.signer.answers[ASK_FRONT] = queue_reply(561, 577)
        self.rerunner.settle("programs")
        self.assertEqual(self.asked(), [ASK_REQUIRED, ASK_TRAIN, ASK_FRONT, ASK_PREVIEW, ASK_STATUSES])
        self.assertEqual([lane_of(command) for command, _ in self.launched], [["programs", LANE]])

    def test_a_front_whose_review_is_pending_waits_for_it_then_gets_its_lane(self):
        self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("23/build.sh#lane", "pending"), check_status("56/review", "pending"))
        self.rerunner.settle("programs")
        self.assertEqual(self.launched, [])
        self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("23/build.sh#lane", "pending"), check_status("56/review", "passed"))
        self.rerunner.settle("programs")
        self.assertEqual([lane_of(command) for command, _ in self.launched], [["programs", LANE]])

    def test_a_front_whose_lane_has_a_result_starts_nothing(self):
        for state in ("passed", "failed", "errored"):
            self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("23/build.sh#lane", state), check_status("56/review", "passed"))
            self.rerunner.settle("programs")
        self.assertEqual(self.launched, [])

    def test_only_the_required_reporters_statuses_count(self):
        self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("23/build.sh#lane", "passed", reporter=23), check_status("56/review", "passed"))
        self.rerunner.settle("programs")
        self.assertEqual([lane_of(command) for command, _ in self.launched], [["programs", LANE]])
        self.kube.kept = {}
        self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("56/review", "passed", reporter=56))
        self.fresh().settle("programs")
        self.assertEqual(len(self.launched), 1)

    def test_a_repo_that_asks_no_lane_of_the_services_account_is_read_no_further_nor_its_kept_runs(self):
        self.signer.answers[ASK_REQUIRED] = required_reply("56/review", "24/build.sh#lane")
        self.kube.kept_load = lambda: self.fail("the ConfigMap was read for a repo that asks no lane")
        self.rerunner.settle("programs")
        self.assertEqual((self.asked(), self.launched), ([ASK_REQUIRED], []))

    def test_an_empty_queue_or_a_front_with_no_preview_starts_nothing(self):
        self.signer.answers[ASK_FRONT] = queue_reply()
        self.rerunner.settle("programs")
        self.signer.answers[ASK_FRONT] = LIVE_QUEUE
        self.signer.answers[ASK_PREVIEW] = server.u8(16) + server.u64(1) + b"\x00"
        self.rerunner.settle("programs")
        self.assertEqual(self.launched, [])

    def test_required_and_statuses_are_read_page_by_page(self):
        more_required = server.forge_required("programs", b"c")
        more_statuses = server.forge_statuses("programs", LANE, b"d")
        self.signer.answers[ASK_REQUIRED] = required_reply("56/review", after=b"c")
        self.signer.answers[more_required] = LIVE_REQUIRED
        self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("56/review", "passed"), after=b"d")
        self.signer.answers[more_statuses] = statuses_reply(check_status("23/build.sh#lane", "passed"))
        self.rerunner.settle("programs")
        self.assertEqual(self.asked(), [ASK_REQUIRED, more_required, ASK_TRAIN, ASK_FRONT, ASK_PREVIEW, ASK_STATUSES, more_statuses])
        self.assertEqual(self.launched, [])

    def test_a_restarted_service_starts_no_second_lane_for_a_front_it_kept(self):
        self.kube.kept[server.rerun_key("programs", LANE)] = self.record(run="desktop:r1")
        self.fresh().settle("programs")
        self.assertEqual((self.launched, self.spawned), ([], []))

    def test_a_kept_preview_no_longer_its_queues_front_is_dropped_and_other_repos_and_runs_stay(self):
        former = server.rerun_key("programs", MOVED_PREVIEW)
        app = server.rerun_key("app", "8e" * 20)
        other = server.kept_key("duckpond-41", "r1")
        app_record = json.dumps({"repo": "app", "n": 3, "commit": "8e" * 20})
        self.kube.kept = {former: json.dumps({"repo": "programs", "n": 559, "commit": MOVED_PREVIEW, "run": "desktop:r0"}), app: app_record, other: "{}"}
        self.rerunner.settle("programs")
        self.assertEqual(self.kube.kept, {app: app_record, other: "{}", server.rerun_key("programs", LANE): self.record(run="desktop:r1")})

    def test_a_key_the_configmap_would_not_take_starts_no_lane(self):
        self.kube.kept_change = lambda data: ran(code=1, err="Error from server (Forbidden): configmaps is forbidden")
        self.rerunner.settle("programs")
        self.assertEqual(self.launched, [])

    def test_a_refused_lane_is_kept_with_its_reason_its_room_told_and_left_to_its_retry(self):
        self.answers = [server.Refused(503, "no build runner is free")]
        self.rerunner.settle("programs")
        self.rerunner.settle("programs")
        self.assertEqual(len(self.launched), 1)
        self.assertEqual(self.kube.kept, {server.rerun_key("programs", LANE): self.record(unstarted="no build runner is free")})
        self.assertEqual([(channel, detail) for channel, _, detail in self.posts], [(LANE_ROOM, f"the lane on Preview {LANE} did not start: no build runner is free. The pods service asks again from one block, doubling, while that Preview is its queue's lane target.")])
        self.assertEqual([(target, args) for target, args in self.spawned], [(self.rerunner.retry, ("programs", server.rerun_key("programs", LANE)))])

    def test_a_lane_build_sh_did_not_start_is_kept_with_its_exit(self):
        self.answers = [ran(code=2, err="build.sh: usage")]
        self.rerunner.settle("programs")
        self.assertEqual(self.kube.kept, {server.rerun_key("programs", LANE): self.record(unstarted="build.sh answered exit 2 with no run")})
        self.assertEqual(len(self.spawned), 1)

    def test_an_unstarted_lane_is_tried_again_from_one_block_doubling_and_its_room_told_once_per_reason(self):
        busy = server.Refused(503, "no build runner is free")
        self.answers = [busy, busy, server.Refused(502, "the warm pod's name was not read"), ran(b"run desktop:r2\n")]
        self.rerunner.settle("programs")
        target, args = self.spawned[0]
        target(*args)
        self.assertEqual((len(self.launched), self.pauses), (4, [1, 2, 4]))
        self.assertEqual(self.kube.kept, {server.rerun_key("programs", LANE): self.record(run="desktop:r2")})
        self.assertEqual(len({message for _, message, _ in self.posts}), 2)
        self.assertEqual((len(self.spawned), self.rerunner.retrying), (1, set()))

    def test_a_retry_stops_as_soon_as_the_front_moves(self):
        self.answers = [server.Refused(503, "no build runner is free")]
        self.rerunner.settle("programs")
        self.signer.answers[ASK_FRONT] = queue_reply(577)
        self.signer.answers[server.forge_preview("programs", 577)] = preview_reply(577, MOVED_PREVIEW)
        self.signer.answers[server.forge_statuses("programs", MOVED_PREVIEW, None)] = statuses_reply(check_status("56/review", "pending"))
        target, args = self.spawned[0]
        target(*args)
        self.assertEqual((len(self.launched), self.pauses, self.rerunner.retrying), (1, [1], set()))

    def test_a_retry_stops_once_the_fronts_lane_is_no_longer_wanted(self):
        self.answers = [server.Refused(503, "no build runner is free"), ran(b"run desktop:r2\n")]
        self.rerunner.settle("programs")
        self.signer.answers[ASK_STATUSES] = statuses_reply(check_status("23/build.sh#lane", "pending"), check_status("56/review", "pending"))
        target, args = self.spawned[0]
        target(*args)
        self.assertEqual((len(self.launched), self.pauses, self.rerunner.retrying), (1, [1], set()))

    def test_a_retry_stops_once_its_key_holds_a_run(self):
        self.answers = [server.Refused(503, "no build runner is free"), ran(b"run desktop:r2\n")]
        self.rerunner.settle("programs")
        key = server.rerun_key("programs", LANE)
        self.kube.kept[key] = self.record(run="desktop:r9")
        target, args = self.spawned[0]
        target(*args)
        self.assertEqual((len(self.launched), self.pauses, self.kube.kept[key]), (1, [1], self.record(run="desktop:r9")))

    def test_a_restarted_service_tries_an_unstarted_front_again(self):
        self.kube.kept[server.rerun_key("programs", LANE)] = self.record(unstarted="no build runner is free")
        rerunner = self.fresh()
        rerunner.settle("programs")
        self.assertEqual((self.launched, [args for _, args in self.spawned]), ([], [("programs", server.rerun_key("programs", LANE))]))
        rerunner.settle("programs")
        self.assertEqual(len(self.spawned), 1)

    def test_it_follows_forge_and_settles_every_queued_repo_as_it_opens_and_a_repo_once_per_block_that_writes_its_queue_statuses_previews_or_requirements(self):
        for repo in ("app", "core"):
            self.signer.answers[server.forge_required(repo, None)] = required_reply("56/review")
        self.signer.answers[server.forge_repos(None)] = repos_reply("app", "programs")
        lines = [
            f"10\t{forge_key(b'r/', 'programs')}\t00\n",
            f"11\t{forge_key(b'e/', 'programs')}\t01\n",
            f"12\t{forge_key(b's/', 'programs')}\t-\n",
            f"13\t{forge_key(b'm/', 'core')}\t01\n",
            f"13\t{forge_key(b'm/', 'core')}\t02\n",
            f"14\t{forge_key(b'q/', 'programs')}\t01\n",
            "15\tzz\t01\n",
        ]
        self.signer.streams = ["".join(lines).encode(), b""]

        class Stop(BaseException):
            pass

        def pause(seconds):
            self.pauses.append(seconds)
            if len(self.pauses) == 2:
                raise Stop()

        rerunner = server.Rerunner(self.signer, self.kube, self.launch, 1, self.post)
        rerunner.pause = pause
        rerunner.called = self.call
        with self.assertRaises(Stop):
            rerunner.watch()
        settled = [required_repo(request) for request in self.asked() if request[:1] == b"\x14"]
        self.assertEqual(settled, ["app", "programs", "programs", "programs", "core", "programs", "app", "programs"])
        self.assertEqual((self.signer.follows, len(self.launched)), (["forge", "forge"], 1))
        self.assertEqual([request for request in self.asked() if request[:1] == b"\x00"], [server.forge_repos(None)] * 2)

    def test_every_repository_forge_lists_is_settled_a_page_at_a_time_as_the_stream_opens(self):
        _, after = server.repos_of(LIVE_REPOS)
        self.signer.answers[server.forge_repos(None)] = LIVE_REPOS
        self.signer.answers[server.forge_repos(after)] = repos_reply("app")
        for repo in ("app", "core"):
            self.signer.answers[server.forge_required(repo, None)] = required_reply("56/review")
        self.rerunner.reopened()
        settled = [required_repo(request) for request in self.asked() if request[:1] == b"\x14"]
        self.assertEqual((settled, [lane_of(command) for command, _ in self.launched]), (["programs", "core", "app"], [["programs", LANE]]))

    def test_a_key_no_account_acts_as_follows_nothing(self):
        self.signer.account = None
        server.Rerunner(self.signer, self.kube, self.launch, 1, self.post).watch()
        self.assertEqual((self.signer.follows, self.launched), ([], []))

    def test_the_service_key_line_names_its_public_key(self):
        body = bytes(range(16 + 12 + 24)) + b"P" * 32 + bytes(48)
        line = server.USER_KEY + server.base64.urlsafe_b64encode(body).decode().rstrip("=") + "\n"
        self.assertEqual(server.public_key(line), b"P" * 32)
        with self.assertRaises(server.Refused):
            server.public_key("ducktape-user-key-v1:AAAA\n")

    def test_the_runs_the_notifier_resumes_leave_the_rerun_keys_alone(self):
        key = server.rerun_key("programs", LANE)
        self.kube.kept[key] = self.record(run="desktop:r1")
        self.kube.answer("list", ran({"items": []}))
        notifier = server.Notifier(self.kube, Signer(), 1)
        notifier.resume()
        self.assertEqual((notifier.open, notifier.pending, self.kube.kept), ({}, {}, {key: self.record(run="desktop:r1")}))

TRAIN_REPLY_TAG = 25
ASK_TRAIN = server.u8(28) + server.borsh_text("programs") + server.borsh_bytes(DEV)


def train_reply(members=(), tip=("23/build.sh#lane",), formed=True, alone=False):
    train = server.u64(4) + server.borsh_vec([server.borsh_text(context) for context in tip])
    row = server.borsh_vec([server.u64(n) + server.borsh_text(commit) for n, commit in members]) + server.u8(int(alone)) + server.u64(2)
    return server.u8(TRAIN_REPLY_TAG) + server.u64(3) + server.borsh_option(train, bytes) + server.borsh_option(row if formed else None, bytes)


class Trains(unittest.TestCase):
    fresh = Reruns.fresh
    call = Reruns.call
    launch = Reruns.launch
    post = Reruns.post
    asked = Reruns.asked

    def setUp(self):
        Reruns.setUp(self)
        self.signer.answers[ASK_TRAIN] = train_reply([(561, LANE), (577, MOVED_PREVIEW)])
        self.signer.answers[server.forge_statuses("programs", MOVED_PREVIEW, None)] = LIVE_STATUSES

    def test_one_tip_lane_is_started_and_kept_across_settles_and_restart(self):
        self.rerunner.settle("programs")
        self.rerunner.settle("programs")
        self.fresh().settle("programs")
        command = ["sh", server.BUILD_SH, "JOB=forge:programs:577", "NOTIFY=forge:programs:577", "programs", MOVED_PREVIEW, "lane"]
        self.assertEqual([command for command, _ in self.launched], [command])
        self.assertNotIn(ASK_FRONT, self.asked())
        self.assertNotIn(ASK_PREVIEW, self.asked())
        self.assertNotIn(ASK_STATUSES, self.asked())

    def test_no_formed_members_means_no_lane_and_no_queue_fallback(self):
        for formed in (False, True):
            self.signer.answers[ASK_TRAIN] = train_reply(formed=formed)
            self.rerunner.settle("programs")
        self.assertEqual(self.launched, [])
        self.assertNotIn(ASK_FRONT, self.asked())

    def test_a_red_train_stops_its_tip_and_lanes_the_front_alone(self):
        self.rerunner.settle("programs")
        self.signer.answers[ASK_TRAIN] = train_reply([(561, LANE)], alone=True)
        self.rerunner.settle("programs")
        self.assertEqual([lane_of(command) for command, _ in self.launched], [["programs", MOVED_PREVIEW], ["programs", LANE]])
        self.assertIn(["sh", server.BUILD_SH, "stop", "desktop:r1"], self.calls)
        self.assertEqual(list(self.kube.kept), [server.rerun_key("programs", LANE)])

    def test_an_unstarted_tip_is_not_retried_after_the_tip_moves(self):
        self.answers = [server.Refused(503, "no build runner is free"), ran(b"run desktop:r2\n")]
        self.rerunner.settle("programs")
        self.signer.answers[ASK_TRAIN] = train_reply([(561, LANE)], alone=True)
        target, args = self.spawned[0]
        target(*args)
        self.assertEqual([lane_of(command) for command, _ in self.launched], [["programs", MOVED_PREVIEW]])
        self.assertEqual(self.rerunner.retrying, set())

    def test_a_lane_not_named_as_a_tip_context_keeps_the_queue_front_path(self):
        self.signer.answers[ASK_TRAIN] = train_reply([(561, LANE), (577, MOVED_PREVIEW)], tip=("56/review",))
        self.rerunner.settle("programs")
        self.assertEqual([lane_of(command) for command, _ in self.launched], [["programs", LANE]])

    def test_a_tip_with_a_result_or_pending_review_starts_nothing(self):
        ask = server.forge_statuses("programs", MOVED_PREVIEW, None)
        for lane, review in (("passed", "passed"), ("failed", "passed"), ("errored", "passed"), ("pending", "pending")):
            self.signer.answers[ask] = statuses_reply(check_status("23/build.sh#lane", lane), check_status("56/review", review))
            self.rerunner.settle("programs")
        self.assertEqual(self.launched, [])

    def test_train_configuration_and_formation_writes_trigger_settle(self):
        for table in (b"y/", b"z/"):
            with self.subTest(table=table):
                self.assertEqual(server.front_touched(f"42\t{forge_key(table, 'programs')}\t01\n".encode()), ("42", "programs"))


class TrainWire(unittest.TestCase):
    def test_train_query_and_reply_match_forges_contract(self):
        self.assertEqual(server.forge_train("programs", DEV), ASK_TRAIN)
        train, members = server.train_of(train_reply([(561, LANE), (577, MOVED_PREVIEW)]))
        self.assertEqual(train, {"length": 4, "tip": ["23/build.sh#lane"]})
        self.assertEqual(members, [{"n": 561, "merge": LANE}, {"n": 577, "merge": MOVED_PREVIEW}])
        self.assertEqual(server.train_of(server.u8(TRAIN_REPLY_TAG) + server.u64(1) + b"\x00\x00"), (None, []))
        self.assertEqual(server.train_of(train_reply(formed=False))[1], [])
        with self.assertRaises(server.Refused):
            server.train_of(LIVE_REQUIRED)


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
        self.fetcher = server.Fetcher(["ducktape"], "https://node.example", "duck://duckpond/forge", self.home, runner=self.recorded, block_ms=1000)
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
        self.fetcher = server.Fetcher(["ducktape"], "https://node.example", "duck://duckpond/forge", self.base / "home", block_ms=1000)

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


UP = {"runner": "desktop", "up": True, "state": "running", "started": "2026-10-20 12:58:31.072128879 +0900 KST", "busy": [], "busy_exit": 0, "heavy_runs": 1}
RUNNER_STARTED = datetime.datetime(2026, 10, 20, 3, 58, 31, tzinfo=datetime.timezone.utc).timestamp() + 72128879 / 1e9
BUILD = server.BUILD_SH
NONE_AHEAD = ran(b"ahead heavy 0\n")


class Gate:
    def __init__(self):
        self.calls = []
        self.answers = {"ping": ran(UP)}
        self.down = set()
        self.cut = set()
        self.cut_words = set()
        self.cut_said = b""
        self.said = {}
        self.processes = []
        self.followers = queue.Queue()
        self.spawned = queue.Queue()
        self.envs = []
        self.sequence = {}
        self.by_host = {}

    def run(self, argv, env=None):
        host = argv[-2].split("@", 1)[1]
        words = json.loads(argv[-1])
        self.calls.append((host, words, argv))
        self.envs.append(env)
        key = " ".join(words[:2])
        if self.sequence.get(key):
            return self.sequence[key].pop(0)
        answers = {**self.answers, **self.by_host.get(host, {})}
        if host in self.down:
            return ran(code=255, err=f"ssh: connect to host {host} port 22: No route to host")
        if words[:2] == ["exec", "ahead"] and host not in self.cut and host not in self.said:
            return answers.get("ahead", NONE_AHEAD)
        if len(words) > 1 and words[0] == "exec" and f"exec {words[1]}" in answers and host not in self.cut and host not in self.said:
            return answers[f"exec {words[1]}"]
        if host in self.cut or (words[1:2] and words[0] == "exec" and set(words) & self.cut_words):
            return ran(self.cut_said, code=255, err=f"Connection to {host} closed by remote host.")
        if host in self.said:
            return ran(code=255, err=self.said[host])
        return answers.get(words[0], ran(b""))

    def spawn(self, argv, env=None):
        words = json.loads(argv[-1])
        self.calls.append((argv[-2].split("@", 1)[1], words, argv))
        if any(word.startswith("--since=") for word in words):
            self.spawned.put(words)
            return self.followers.get()
        return self.processes.pop(0)

    def words(self):
        return [words for _, words, _ in self.calls]


class Followed:
    def __init__(self, code, said, out=b"[run] a line\n"):
        self.stdout = io.BytesIO(out)
        self.stderr = io.BytesIO(said)
        self.code = code
        self.killed = False

    def wait(self):
        return self.code

    def poll(self):
        return self.code

    def kill(self):
        self.killed = True


class DesktopRunner(Streamed):
    def setUp(self):
        held = tempfile.TemporaryDirectory()
        self.addCleanup(held.cleanup)
        self.keys = pathlib.Path(held.name) / "keys"
        self.keys.mkdir()
        for name, text in {"id_ed25519": "private", "known_hosts": "desk ssh-ed25519 AAAA", "hosts": "lan\ntail\nwifi\n", "user": "orthory\n"}.items():
            (self.keys / name).write_text(text)
        self.gate = Gate()
        desktop = server.Runner("desktop", self.keys, pathlib.Path(held.name) / "home", runner=self.gate.run, spawn=self.gate.spawn)
        self.serve(runners=[desktop])
        self.service.rest = lambda name, pause: self.paused.append(pause)
        self.service.wall = lambda: RUNNER_STARTED

    def test_a_call_fleet_relays_reaches_no_build_runner(self):
        status, answer = self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "busy"]}, FLEETED)
        self.assertEqual(status, 403, answer)
        self.assertIn("build runner", answer["error"])
        self.assertEqual(self.kube.calls, [])

    def test_a_build_runner_answers_whole_whatever_its_inline(self):
        self.gate.answers["exec"] = ran(b"run r1\n")
        status, answer = self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "busy"], "inline": 0})
        self.assertEqual((status, answer["stdout"]), (200, "run r1\n"))
        self.assertNotIn("keep", self.kube.verbs())

    def notified(self, line):
        self.gate.answers["exec"] = ran(b"run r1\n")
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "JOB=j-1", "NOTIFY=j-1", "programs", "dev", "gates"]})
        words = self.gate.spawned.get(timeout=10)
        self.assertEqual(words[0], "logs")
        self.assertTrue(words[1].startswith("--since=") and words[1].endswith("s"))
        self.gate.followers.put(Followed(255, b""))
        self.gate.followers.put(Followed(0, b"", line))
        self.gate.spawned.get(timeout=10)
        self.gate.spawned.get(timeout=10)

    def test_a_build_call_goes_with_the_caller_egress_names_and_names_none_itself(self):
        self.signer.sender = ran(server.u8(server.EGRESS_SENDER) + server.u8(1) + server.u64(21))
        self.gate.answers["exec"] = ran(b"run r1\n")
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "JOB=j-1", "programs", "dev", "gates"]}, headers=asked(51))
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", "-c", f"JOB=j-1 sh {BUILD} programs dev lint"]}, headers=asked(52))
        self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["sh", BUILD, "programs", "dev", "lint"]}, headers=asked(53))
        self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["cat", "/out/a"]}, headers=asked(54))
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "status", "r1"]}, headers=asked(55))
        execs = [words for words in self.gate.words() if words[0] == "exec"]
        self.assertEqual(
            execs,
            [
                ["exec", "CALLER=21", "JOB=j-1", "programs", "dev", "gates"],
                ["exec", "CALLER=21", "JOB=j-1", "programs", "dev", "lint"],
                ["exec", "CALLER=21", "status", "r1"],
            ],
        )
        pods = [call[3] for call in self.kube.calls if call[0] == "exec"]
        self.assertEqual(pods, [["sh", BUILD, "CALLER=21", "PINNED=1", "programs", "dev", "lint"], ["cat", "/out/a"]])
        self.assertEqual(self.signer.senders, [server.egress_sender(request) for request in (51, 52, 53, 55)])
        self.refused("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "CALLER=9", "programs", "dev", "gates"]}, 400, ["leave CALLER=<n> out"])

    def test_a_build_call_to_a_pod_by_name_is_pinned_there_and_one_through_builds_or_to_a_runner_is_not(self):
        self.gate.answers["exec"] = ran(b"run r1\n")
        self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["sh", BUILD, "JOB=j-1", "programs", "dev", "lint"]}, headers=asked(71))
        self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["sh", BUILD, "PINNED=1", "programs", "dev", "gates"]}, headers=asked(72))
        self.call("POST", "/v1/pods/duckpond-41/exec", {"command": ["cat", "/out/a"]}, headers=asked(73))
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "programs", "dev", "gates"]}, headers=asked(74))
        pods = [call[3] for call in self.kube.calls if call[0] == "exec"]
        self.assertEqual(
            pods,
            [
                ["sh", BUILD, "PINNED=1", "JOB=j-1", "programs", "dev", "lint"],
                ["sh", BUILD, "PINNED=1", "programs", "dev", "gates"],
                ["cat", "/out/a"],
            ],
        )
        execs = [words for words in self.gate.words() if words[0] == "exec"]
        self.assertEqual(execs, [["exec", "programs", "dev", "gates"]])

    def test_a_pod_by_name_runs_its_own_build_sh_as_it_came(self):
        own = (["sh", "build.sh", "x"], ["build.sh", "x"], ["env", "K=V", "build.sh", "x"], ["K=V", "build.sh", "x"], [BUILD, "x"])
        for request, command in enumerate(own, start=90):
            self.call("POST", "/v1/pods/duckpond-41/exec", {"command": command}, headers=asked(request))
        pods = [call[3] for call in self.kube.calls if call[0] == "exec"]
        self.assertEqual(pods, list(own))

    def test_a_build_call_egress_names_no_sender_for_goes_as_it_came_and_says_so_once(self):
        self.signer.sender = ran(code=1, err="egress: no such query")
        self.gate.answers["exec"] = ran(b"run r1\n")
        with unittest.mock.patch.object(server, "say") as said:
            for request in (61, 62):
                self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "programs", "dev", "gates"]}, headers=asked(request))
            self.signer.sender = ran(server.u8(server.EGRESS_SENDER) + server.u8(0))
            self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "programs", "dev", "lint"]}, headers=asked(63))
        execs = [words for words in self.gate.words() if words[0] == "exec"]
        self.assertEqual(execs, [["exec", "programs", "dev", "gates"]] * 2 + [["exec", "programs", "dev", "lint"]])
        self.assertEqual([call for call in said.call_args_list if call.args == ("pods_uncalled",)], [unittest.mock.call("pods_uncalled", detail=unittest.mock.ANY)])
        said.assert_any_call("pods_called", missed=2)

    def test_with_no_warm_pod_named_a_notify_run_s_end_line_concerns_no_one(self):
        self.notified(b"[r1] GATES ok\nducktape-notify j-1 r1 ended: exit 0, GATES ok\n")
        line = "desktop · r1 ended: exit 0, GATES ok"
        posted = notified("j-1", server.notify_id("desktop", "j-1", line), None, line, None)
        self.assertEqual(self.signer.submits, [("chat", posted)])
        self.assertEqual(self.paused, [1, 1])

    def joins(self, clock, streams):
        self.service.clock = clock
        self.gate.answers["exec"] = ran(b"run r1\n")
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "NOTIFY=j-1", "programs", "dev", "gates"]})
        asked = [self.gate.spawned.get(timeout=10)]
        for out in streams:
            self.gate.followers.put(Followed(255, b"", out))
            asked.append(self.gate.spawned.get(timeout=10))
        return [words[1] for words in asked]

    def test_a_run_that_ended_while_the_service_was_down_still_posts_its_end(self):
        self.service.clock = lambda: 1000.0
        self.service.wall = lambda: RUNNER_STARTED + 599.5
        self.gate.answers["exec"] = ran(b"run r2\n")
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "NOTIFY=j-1", "programs", "dev", "gates"]})
        words = self.gate.spawned.get(timeout=10)
        self.assertEqual(words[1], "--since=600s")
        self.gate.followers.put(Followed(0, b"", b"ducktape-notify j-1 r1 ended: exit 0, GATES ok\n"))
        self.gate.spawned.get(timeout=10)
        line = "desktop · r1 ended: exit 0, GATES ok"
        self.assertIn(("chat", notified("j-1", server.notify_id("desktop", "j-1", line), None, line, None)), self.signer.submits)

    def test_a_desktop_whose_ping_names_no_start_is_followed_from_now_and_asked_again(self):
        self.gate.answers["ping"] = ran({name: value for name, value in UP.items() if name != "started"})
        self.service.clock = lambda: 1000.0
        self.service.wall = lambda: RUNNER_STARTED + 599.5
        with unittest.mock.patch.object(server, "say") as said:
            self.service.follow("desktop")
            self.assertEqual(self.gate.spawned.get(timeout=10)[1], "--since=0s")
            self.gate.answers["ping"] = ran(UP)
            self.gate.followers.put(Followed(255, b""))
            self.assertEqual(self.gate.spawned.get(timeout=10)[1], "--since=600s")
        said.assert_any_call("pods_unresumed", pod="desktop", detail=json.dumps("the desktop's ping names no start time"))

    def test_a_start_time_not_in_podmans_shape_is_named_and_followed_from_now(self):
        self.gate.answers["ping"] = ran({**UP, "started": "2026-10-20T03:58:31Z"})
        self.service.clock = lambda: 1000.0
        self.service.wall = lambda: RUNNER_STARTED + 599.5
        with unittest.mock.patch.object(server, "say") as said:
            self.service.follow("desktop")
            self.assertEqual(self.gate.spawned.get(timeout=10)[1], "--since=0s")
        said.assert_any_call("pods_unresumed", pod="desktop", detail=json.dumps("the desktop's ping names its start as '2026-10-20T03:58:31Z', not as podman's StartedAt (date, time, offset, zone)"))

    def test_a_line_chat_already_holds_is_not_offered_again(self):
        line = "desktop · r1 ended: exit 0, GATES ok"
        held = server.notify_id("desktop", "j-1", line)
        self.signer.messages.add(held)
        with unittest.mock.patch.object(server, "say") as said:
            self.notified(b"ducktape-notify j-1 r1 ended: exit 0, GATES ok\nducktape-notify j-1 r2 ended: exit 0, GATES ok\n")
        self.assertEqual(len(self.signer.submits), 1)
        said.assert_any_call("pods_unfollowed", pod="desktop", code=0, already_posted=1)
        self.assertNotIn(held, repr(said.call_args_list))

    def test_a_pods_start_asks_chat_only_back_to_the_newest_line_it_holds(self):
        logged = b"[r1] a line\nducktape-notify j-1 r1 ended: exit 0, GATES ok\nducktape-notify j-2 r2 ended: exit 1, GATES FAIL: test\nducktape-notify j-1 r3 ended: exit 0, GATES ok\n"
        lines = [("j-1", "desktop · r1 ended: exit 0, GATES ok"), ("j-2", "desktop · r2 ended: exit 1, GATES FAIL: test"), ("j-1", "desktop · r3 ended: exit 0, GATES ok")]
        ids = [server.notify_id("desktop", channel, line) for channel, line in lines]
        self.signer.messages.update(ids[:2])
        self.gate.answers["logs"] = ran(logged)
        self.service.clock = lambda: 1000.0
        self.service.wall = lambda: RUNNER_STARTED + 599.5
        with unittest.mock.patch.object(server, "say") as said:
            self.service.follow("desktop")
            self.assertEqual(self.gate.spawned.get(timeout=10)[1], "--since=600s")
            self.gate.followers.put(Followed(0, b"", logged))
            self.gate.spawned.get(timeout=10)
        self.assertIn(["logs", "--since=600s"], self.gate.words())
        self.assertNotIn(server.chat_message(ids[0]), self.signer.asked)
        self.assertEqual([request for request in self.signer.asked if request in map(server.chat_message, ids)], [server.chat_message(ids[2]), server.chat_message(ids[1]), server.chat_message(ids[2])])
        self.assertEqual(self.signer.submits, [("chat", notified("j-1", ids[2], None, lines[2][1], None))])
        said.assert_any_call("pods_caught_up", pod="desktop", already_posted=2, asked=2)

    def test_a_chat_ask_that_fails_mid_walk_marks_nothing_and_each_line_is_asked_as_it_comes(self):
        self.walk_failing("ducktape: chat could not read its query")

    def test_a_chat_ask_the_node_does_not_answer_mid_walk_marks_nothing_and_each_line_is_asked_as_it_comes(self):
        self.walk_failing("ducktape: the node failed (502): bad gateway")

    def walk_failing(self, error):
        logged = b"ducktape-notify j-1 r1 ended: exit 0, GATES ok\nducktape-notify j-1 r2 ended: exit 0, GATES ok\nducktape-notify j-1 r3 ended: exit 0, GATES ok\n"
        lines = [("j-1", f"desktop · r{n} ended: exit 0, GATES ok") for n in (1, 2, 3)]
        ids = [server.notify_id("desktop", channel, line) for channel, line in lines]
        self.signer.messages.update(ids[:2])
        asking, failed = self.signer.query, []

        def query(program, request):
            if program == "chat" and request[:1] == server.u8(2) and not failed:
                failed.append(request)
                return ran(code=1, err=error)
            return asking(program, request)

        self.signer.query = query
        self.gate.answers["logs"] = ran(logged)
        self.service.clock = lambda: 1000.0
        self.service.wall = lambda: RUNNER_STARTED + 599.5
        with unittest.mock.patch.object(server, "say") as said:
            self.service.follow("desktop")
            self.gate.spawned.get(timeout=10)
            self.gate.followers.put(Followed(0, b"", logged))
            self.gate.spawned.get(timeout=10)
        said.assert_any_call("pods_uncaught", pod="desktop", asked=1, detail=json.dumps(error))
        self.assertNotIn("pods_caught_up", [call.args[0] for call in said.call_args_list])
        self.assertEqual([request for request in self.signer.asked if request in map(server.chat_message, ids)], [server.chat_message(id) for id in ids])
        self.assertEqual(self.signer.submits, [("chat", notified("j-1", ids[2], None, lines[2][1], None))])

    def test_a_backlog_the_gate_does_not_serve_leaves_each_line_to_its_own_check(self):
        self.gate.answers["logs"] = ran(code=64, err="gate: refused bad_log_option: --since")
        self.service.clock = lambda: 1000.0
        self.service.wall = lambda: RUNNER_STARTED + 599.5
        with unittest.mock.patch.object(server, "say") as said:
            self.service.follow("desktop")
            self.assertEqual(self.gate.spawned.get(timeout=10)[1], "--since=600s")
        said.assert_any_call("pods_uncaught", pod="desktop", detail=json.dumps("the desktop's logs were not read: gate: refused bad_log_option: --since"))

    def test_podmans_start_times_read_with_or_without_a_fraction(self):
        self.assertEqual(server.go_time(UP["started"]), RUNNER_STARTED)
        self.assertEqual(server.go_time("2026-10-20 03:58:31 +0000 UTC"), datetime.datetime(2026, 10, 20, 3, 58, 31, tzinfo=datetime.timezone.utc).timestamp())
        self.assertEqual(server.go_time("2026-10-19 19:58:31.5 -0800"), datetime.datetime(2026, 10, 20, 3, 58, 31, tzinfo=datetime.timezone.utc).timestamp() + 0.5)
        self.assertIsNone(server.go_time("2026-10-20T03:58:31Z"))

    def test_a_join_that_delivers_nothing_keeps_the_window_of_the_last_stream_that_did(self):
        windows = self.joins(iter([0, 10, 400, 420]).__next__, [b"[r1] a line\n", b""])
        self.assertEqual(windows, ["--since=10s", "--since=390s", "--since=410s"])
        self.assertEqual(self.paused, [1, 1])

    def test_joins_that_deliver_nothing_back_off(self):
        self.joins(itertools.count().__next__, [b""] * 5)
        self.assertEqual(self.paused, [1, 2, 4, 8, 16])

    def test_a_notify_line_for_a_members_only_room_is_not_posted(self):
        self.signer.rooms[server.chat_channel("j-1")] = room(members_only=True)
        self.notified(b"ducktape-notify j-1 r1 ended: exit 0, GATES ok\n")
        self.assertEqual(self.signer.submits, [])

    def test_its_status_is_its_runners_ping_over_the_gate_key(self):
        status, answer = self.call("GET", "/v1/pods/desktop")
        self.assertEqual((status, answer["pod"]["name"], answer["pod"]["phase"]), (200, "desktop", "Running"))
        self.assertEqual(answer["pod"]["runner"], UP)
        ((host, words, argv),) = self.gate.calls
        self.assertEqual((host, words, argv[-2]), ("lan", ["ping"], "orthory@lan"))
        for option in ("IdentitiesOnly=yes", "BatchMode=yes", "StrictHostKeyChecking=yes", f"UserKnownHostsFile={self.keys / 'known_hosts'}"):
            self.assertIn(option, argv)
        key = pathlib.Path(argv[argv.index("-i") + 1])
        self.assertEqual((key.read_text(), key.stat().st_mode & 0o777), ("private", 0o600))
        self.assertNotIn("get", self.kube.verbs())

    def test_a_runner_that_is_not_up_or_is_down_answers_503(self):
        self.gate.answers["ping"] = ran({"up": False, "state": "stopped"})
        self.refused("GET", "/v1/pods/desktop", None, 503, ["not up", "stopped"])
        self.gate.answers["ping"] = ran(code=69, err="gate: runner_down")
        self.refused("GET", "/v1/pods/desktop", None, 503, ["down", "runner_down"])

    def test_each_host_is_tried_in_order_and_the_one_that_answered_goes_first_after(self):
        self.gate.down = {"lan"}
        self.assertEqual(self.call("GET", "/v1/pods/desktop")[0], 200)
        self.assertEqual([host for host, _, _ in self.gate.calls], ["lan", "tail"])
        self.call("GET", "/v1/pods/desktop")
        self.assertEqual([host for host, _, _ in self.gate.calls][2:], ["tail"])

    def test_an_exec_goes_to_the_next_host_only_when_ssh_never_connected(self):
        self.gate.down = {"lan"}
        self.gate.answers["exec"] = ran(b"run r1\n")
        status, _ = self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "busy"]})
        self.assertEqual((status, [host for host, _, _ in self.gate.calls]), (200, ["lan", "tail"]))

    def test_an_exec_whose_session_broke_is_never_run_again(self):
        self.gate.cut = {"lan"}
        self.refused("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "busy"]}, 502, ["may still be running", "closed by remote host"])
        self.assertEqual([host for host, _, _ in self.gate.calls], ["lan"])

    def test_an_exec_whose_own_output_names_a_failed_connect_is_not_run_again(self):
        self.gate.said = {"lan": "ssh: connect to host forge port 22: Connection refused\nConnection to lan closed by remote host."}
        self.gate.said["lan"] = "build.sh: " + self.gate.said["lan"]
        self.refused("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "busy"]}, 502, ["may still be running"])
        self.assertEqual([host for host, _, _ in self.gate.calls], ["lan"])

    def test_a_read_whose_session_broke_tries_the_next_host(self):
        self.gate.cut = {"lan"}
        self.assertEqual(self.call("GET", "/v1/pods/desktop")[0], 200)
        self.assertEqual([host for host, _, _ in self.gate.calls], ["lan", "tail"])

    def test_hosts_on_one_line_are_tried_in_order(self):
        (self.keys / "hosts").write_text("lan tail\twifi\n")
        self.gate.down = {"lan", "tail"}
        self.assertEqual(self.call("GET", "/v1/pods/desktop")[0], 200)
        self.assertEqual([host for host, _, _ in self.gate.calls], ["lan", "tail", "wifi"])

    def test_no_host_answering_is_503_naming_each(self):
        self.gate.down = {"lan", "tail", "wifi"}
        self.refused("GET", "/v1/pods/desktop", None, 503, ["unreachable", "lan:", "tail:", "wifi:", "No route to host"])

    def test_no_key_mounted_is_503(self):
        (self.keys / "id_ed25519").unlink()
        self.refused("POST", "/v1/pods/desktop/exec", {"command": ["sh", BUILD, "busy"]}, 503, ["no key"])
        self.assertEqual(self.gate.calls, [])

    def test_an_exec_of_build_sh_goes_to_the_gates_exec_with_its_words(self):
        self.gate.answers["exec"] = ran(b"run 20261019-1\n")
        command = ["sh", BUILD, "JOB=j-1", "programs", "dev", "gates"]
        status, answer = self.call("POST", "/v1/pods/desktop/exec", {"command": command})
        self.assertEqual((status, answer), (200, {"exit_code": 0, "stdout": "run 20261019-1\n", "stderr": ""}))
        self.assertEqual(self.gate.words(), [["exec", "JOB=j-1", "programs", "dev", "gates"]])

    def test_a_script_of_plain_words_reads_as_build_shs_words(self):
        script = "JOB=j-1 LIGHT=1 sh /var/tmp/warm/build.sh programs dev sh 'cargo test; echo $X # light'"
        self.call("POST", "/v1/pods/desktop/exec", {"command": ["sh", "-c", script]})
        self.assertEqual(self.gate.words(), [["exec", "JOB=j-1", "LIGHT=1", "programs", "dev", "sh", "cargo test; echo $X # light"]])

    def test_the_obvious_build_sh_forms_read_as_its_words(self):
        for request, command in enumerate((
            ["build.sh", "JOB=j-1", "programs", "dev", "gates"],
            [BUILD, "JOB=j-1", "programs", "dev", "gates"],
            ["sh", "build.sh", "JOB=j-1", "programs", "dev", "gates"],
            ["env", "JOB=j-1", "sh", BUILD, "programs", "dev", "gates"],
            ["env", "JOB=j-1", "build.sh", "programs", "dev", "gates"],
            ["JOB=j-1", "build.sh", "programs", "dev", "gates"],
            ["sh", "-c", "env JOB=j-1 build.sh programs dev gates"],
        ), start=70):
            self.gate.calls.clear()
            self.call("POST", "/v1/pods/desktop/exec", {"command": command}, headers=asked(request))
            self.assertEqual(self.gate.words(), [["exec", "JOB=j-1", "programs", "dev", "gates"]], command)

    def test_a_refused_command_names_a_call_to_copy(self):
        self.refused("POST", "/v1/pods/desktop/exec", {"command": ["make"]}, 400, ['as in ["sh", "/var/tmp/warm/build.sh", "JOB=j-1", "NOTIFY=j-1", "programs", "dev", "lint"]', "Not make"])
        for request, command in enumerate((["env", "-i", "build.sh", "busy"], ["sh", "-c", "sh -x build.sh busy"], ["bash", BUILD, "busy"], ["sh", "-c", "env", "build.sh"]), start=80):
            status, answer = self.call("POST", "/v1/pods/desktop/exec", {"command": command}, headers=asked(request))
            self.assertEqual(status, 400, (command, answer))
            self.assertIn("runs build.sh alone", answer["error"])
        self.assertEqual(self.gate.calls, [])

    def test_any_other_command_is_refused_before_ssh(self):
        for command in (
            ["make"],
            ["cat", "--", "/out/x"],
            ["sh", "-c", "sh /var/tmp/warm/build.sh busy; rm -rf /"],
            ["sh", "-c", "sh /var/tmp/warm/build.sh $(id)"],
            ["sh", "-c", 'sh /var/tmp/warm/build.sh "$HOME"'],
            ["sh", "-c", "sh /var/tmp/warm/build.sh busy # x"],
        ):
            self.refused("POST", "/v1/pods/desktop/exec", {"command": command}, 400, ["runs build.sh alone"])
        self.assertEqual(self.gate.calls, [])

    def test_a_streamed_exec_keeps_ssh_alive_at_its_own_every(self):
        self.gate.answers["exec"] = ran(b"ended\n", code=1)
        response = self.stream("/v1/pods/desktop/exec", "POST", {"command": ["sh", BUILD, "wait", "r1"], "every": 7})
        kind, data = self.past_alive(response)
        self.assertEqual((kind, data["exit_code"], data["stdout"]), ("ended", 1, "ended\n"))
        ((_, words, argv),) = self.gate.calls
        self.assertEqual(words, ["exec", "wait", "r1"])
        self.assertIn("ServerAliveInterval=7", argv)

    def test_a_blob_is_read_through_the_gates_get(self):
        self.gate.answers["get"] = ran(b"\0asm")
        status, answer = self.call("POST", "/v1/pods/desktop/blob", {"path": "/out/a.wasm", "kind": "program"})
        self.assertEqual((status, answer["size"]), (200, 4))
        self.assertEqual(self.gate.words(), [["get", "/out/a.wasm"]])
        self.assertEqual(self.signer.puts, [([b"\0asm"], "program")])
        self.assertNotIn("get", self.kube.verbs())

    def test_a_push_resolves_and_packs_through_the_gate(self):
        self.gate.answers["resolve"] = ran(f"{COMMIT} {HAVE}\n1\n".encode())
        self.gate.answers["pack"] = ran(pack_of(3))
        tree = "/var/tmp/warm/programs/git"
        status, answer = self.call("POST", "/v1/pods/desktop/push", {"path": tree, "have": [HAVE]})
        self.assertEqual((status, answer["commit"], answer["objects"]), (200, COMMIT, 3))
        self.assertEqual(self.gate.words(), [["resolve", tree, "HEAD", HAVE], ["pack", tree, COMMIT, HAVE]])

    def test_logs_are_the_runners_with_tail_and_since(self):
        self.gate.answers["logs"] = ran(b"[r1] GATES ok\n")
        status, answer = self.call("GET", "/v1/pods/desktop/logs?tail=5&since=60&container=run")
        self.assertEqual((status, answer), (200, b"[r1] GATES ok\n"))
        self.assertEqual(self.gate.words(), [["logs", "--tail=5", "--since=60s"]])

    def test_a_wait_ends_when_the_runners_log_stream_does(self):
        self.gate.processes.append(Followed(255, b"Timeout, server lan not responding.\n"))
        response = self.stream("/v1/pods/desktop/wait?every=9")
        self.assertEqual(response.getheader("ducktape-reattach"), "request")
        kind, data = self.past_alive(response)
        self.assertEqual(kind, "ended")
        self.assertEqual((data["exit_code"], data["pod"]), (None, None))
        self.assertIn("ssh exit 255", data["line"])
        self.assertIn("not responding", data["line"])
        self.assertEqual(self.gate.words(), [["ping"], ["logs", "--tail=0", "--follow"]])
        self.assertIn("ServerAliveInterval=9", self.gate.calls[1][2])

    def test_a_wait_on_a_runner_that_is_not_up_is_refused_before_any_stream(self):
        self.gate.answers["ping"] = ran({"up": False, "state": "stopped"})
        self.refused("GET", "/v1/pods/desktop/wait?every=9", None, 503, ["not up"])

    def test_a_wait_on_a_runner_reads_no_log(self):
        self.refused("GET", "/v1/pods/desktop/wait?every=9&log=/out/log", None, 400, ["build runner", "no log"])
        self.assertEqual(self.gate.calls, [])

    def test_it_is_never_deleted(self):
        self.refused("DELETE", "/v1/pods/desktop", None, 403, ["not a pod"])
        self.assertEqual(self.gate.calls, [])


def status(network, block_ms):
    return server.borsh_text(network) + server.u64(1) + server.u64(block_ms) + server.u64(10) + server.u64(500) + b"the rest of the status"


NODE_BLOCK_MS = 1000


class Node(BaseHTTPRequestHandler):
    def do_GET(self):
        named = self.headers.get("User-Agent") == "pods"
        body = status("duckpond", NODE_BLOCK_MS) if self.path == "/v1/status" and named else b""
        self.send_response(200 if body else 404)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


class Paced(unittest.TestCase):
    def test_the_block_time_is_read_from_a_status(self):
        self.assertEqual(server.block_ms_of(status("sealpond", 2500)), 2500)

    def test_a_fetcher_given_no_block_time_reads_its_nodes(self):
        served = ThreadingHTTPServer(("127.0.0.1", 0), Node)
        threading.Thread(target=served.serve_forever, daemon=True).start()
        self.addCleanup(served.server_close)
        self.addCleanup(served.shutdown)
        node = f"http://127.0.0.1:{served.server_address[1]}"
        with tempfile.TemporaryDirectory() as home:
            fetcher = server.Fetcher(["ducktape"], node, "duck://duckpond/forge", pathlib.Path(home))
        self.assertEqual(fetcher.retry_pause, 1.0)

    def test_a_status_cut_short_is_refused(self):
        with self.assertRaises(server.Refused):
            server.block_ms_of(server.borsh_text("duckpond") + server.u64(1))

    def test_every_retry_pauses_from_the_networks_block_time(self):
        fetching = server.Fetching(image="pods", node="https://node.example", forge="duck://duckpond/forge", block_ms=2500)
        service = server.Service(Kube(), Signer(), fetching, Stats(), Api())
        paused = []
        service.pause = paused.append
        answers = iter([ran(code=1, err=DROPPED), ran(code=1, err=DROPPED), ran(b"ok")])
        self.assertEqual(service.retried(ASKED_BY, lambda: next(answers), server.failed).out, b"ok")
        self.assertEqual(paused, [2.5, 5.0])


ASKED_BY = {"chain": "duckpond", "program": "harness", "request": "41"}


def files_answer(data):
    return server.u8(5) + server.u8(0) + b"\xab" * 32 + server.u64(len(data)) + server.u64(0) + server.borsh_bytes(data) + server.u8(1)


WARM = "duckpond-77679"
HEAVY = "programs 20261019-1 heavy j-1 [1m] programs dev gates"


class Builds(Streamed):
    def setUp(self):
        DesktopRunner.setUp(self)
        self.service.warm_pod = "/shared/tools/warm-pod"
        self.signer.head = ran(files_answer(f"{WARM}\n".encode()))
        self.gate.answers["exec"] = ran(b"run r1\nread it with: build.sh status r1\n")
        self.kube.answer("exec", ran(b"run w1\nread it with: build.sh status w1\n"))
        self.service.reroute_after = lambda asked: None
        self.service.claim = lambda asked, settings, words, alive: None

    def built(self, *words, request=41):
        return self.call("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, *words]}, headers=asked(request))

    def warm_execs(self):
        return [call[1:] for call in self.kube.calls if call[0] == "exec"]

    def test_a_desktop_line_concerns_the_warm_pods_requester_and_is_grouped_by_its_run(self):
        self.kube.answer("get", ran(pod_json(name=WARM, requester="21")))
        DesktopRunner.notified(self, b"ducktape-notify j-1 r1 ended: exit 0, GATES ok\n")
        said = server.line_said("desktop", "r1 ended: exit 0, GATES ok", 21)
        self.assertEqual((said["group"], said["concerns"]), ("desktop:r1", [21]))
        message_id = server.notify_id("desktop", "j-1", "desktop \u00b7 r1 ended: exit 0, GATES ok")
        self.assertIn(("chat", server.notice_message("j-1", message_id, None, said)), self.signer.submits)

    def test_a_warm_pod_that_cannot_be_read_leaves_a_desktop_line_for_no_one_and_says_why(self):
        self.kube.answer("get", ran(code=1, err=f'Error from server (NotFound): pods "{WARM}" not found'))
        with unittest.mock.patch.object(server, "say") as said:
            DesktopRunner.notified(self, b"ducktape-notify j-1 r1 ended: exit 0, GATES ok\n")
        message_id = server.notify_id("desktop", "j-1", "desktop \u00b7 r1 ended: exit 0, GATES ok")
        posted = server.notice_message("j-1", message_id, None, server.line_said("desktop", "r1 ended: exit 0, GATES ok", None))
        self.assertIn(("chat", posted), self.signer.submits)
        said.assert_any_call("pods_unconcerned", pod="desktop", detail=unittest.mock.ANY)

    def test_a_run_called_again_goes_to_a_runner_that_takes_it_with_again_set(self):
        self.assertEqual(self.service.recalled(["JOB=j-1", "programs", "dev", "gates"], None), "desktop:r1")
        self.assertEqual(self.gate.words()[-1], ["exec", "JOB=j-1", "AGAIN=1", "programs", "dev", "gates"])
        self.assertEqual(self.warm_execs(), [])

    def rerun(self):
        room = "forge:programs:5"
        return ["sh", BUILD, f"JOB={room}", f"NOTIFY={room}", "programs", COMMIT, "lane"]

    def test_a_lane_the_service_asks_for_itself_goes_through_builds_with_no_request_behind_it(self):
        answer = self.service.routed(None, self.rerun(), None)
        self.assertEqual(answer.out.splitlines()[0], b"run desktop:r1")
        execs = [words for words in self.gate.words() if words[0] == "exec"]
        self.assertEqual(execs[-1], ["exec", "JOB=forge:programs:5", "NOTIFY=forge:programs:5", "programs", COMMIT, "lane"])
        self.assertEqual(([request for program, request in self.signer.queries if program == "egress"], self.signer.senders), ([], []))

    def test_the_service_bumps_a_lane_it_started_on_the_machine_that_runs_it(self):
        self.service.rerunner.build("bump", "desktop:r1")
        self.assertEqual([words for words in self.gate.words() if words[0] == "exec"][-1], ["exec", "bump", "r1"])

    def test_the_service_stops_a_superseded_lane_where_build_sh_moved_it(self):
        moved = ran(code=2, err="build.sh: run r0 (job forge:programs:5) was moved to run desktop:r9 on another machine: stop that one there")
        self.gate.sequence["exec stop"] = [moved, ran(b"stopped r9 (job forge:programs:5)\n")]
        with unittest.mock.patch.object(server, "say") as said:
            self.service.rerunner.build("stop", "desktop:r0")
        stops = [words for words in self.gate.words() if words[:2] == ["exec", "stop"]]
        self.assertEqual(stops, [["exec", "stop", "r0"], ["exec", "stop", "r9"]])
        self.assertNotIn("pods_rerun_unbuilt", [call.args[0] for call in said.call_args_list])

    def test_stopping_a_lane_that_already_ended_is_done_and_says_nothing(self):
        self.gate.sequence["exec stop"] = [ran(code=2, err="build.sh: run r0 (job forge:programs:5) has ended")]
        with unittest.mock.patch.object(server, "say") as said:
            self.service.rerunner.build("stop", "desktop:r0")
        self.assertNotIn("pods_rerun_unbuilt", [call.args[0] for call in said.call_args_list])

    def test_a_stop_build_sh_refuses_for_another_reason_says_so(self):
        self.gate.sequence["exec stop"] = [ran(code=2, err="build.sh: no run r0 here")]
        with unittest.mock.patch.object(server, "say") as said:
            self.service.rerunner.build("stop", "desktop:r0")
        said.assert_any_call("pods_rerun_unbuilt", verb="stop", run=json.dumps("desktop:r0"), detail=json.dumps("build.sh stop answered exit 2"))

    def test_a_lane_the_service_asks_for_states_the_operator_s_first_limit(self):
        with unittest.mock.patch.object(server, "LANE_FIRST_LIMIT", "40"):
            command = server.rerun_command({"repo": "programs", "n": 5, "commit": COMMIT})
        self.assertEqual(command, ["sh", BUILD, "JOB=forge:programs:5", "NOTIFY=forge:programs:5", "FIRST_LIMIT=40", "programs", COMMIT, "lane"])

    def test_a_lane_the_service_asks_for_with_no_first_limit_set_states_none(self):
        with unittest.mock.patch.object(server, "LANE_FIRST_LIMIT", ""):
            command = server.rerun_command({"repo": "programs", "n": 5, "commit": COMMIT})
        self.assertEqual(command, self.rerun())

    def test_a_lane_the_service_asks_for_again_while_its_first_run_still_waits_gets_that_run(self):
        self.service.claim = server.Service.claim.__get__(self.service)
        self.service.launched[tuple(server.runner_words(self.rerun(), server.BUILDS))] = ("desktop", "r1")
        self.gate.answers["exec claim"] = ran(b"same r1\n")
        answer = self.service.routed(None, self.rerun(), None)
        self.assertEqual(answer.out.splitlines()[0], b"run desktop:r1")
        execs = [words for words in self.gate.words() if words[0] == "exec"]
        self.assertEqual(execs, [["exec", "claim", "r1", "JOB=forge:programs:5", "NOTIFY=forge:programs:5", "programs", COMMIT, "lane"]])

    def test_a_free_desktop_takes_the_run_and_the_answer_names_it(self):
        status, answer = self.built("JOB=j-1", "programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"]), (200, "run desktop:r1\nread it with: build.sh status r1\n"))
        self.assertTrue(answer["stderr"].startswith("builds: on desktop"))
        self.assertEqual(self.gate.words(), [["ping"], ["exec", "ahead", "JOB=j-1", "programs", "dev", "gates"], ["exec", "JOB=j-1", "programs", "dev", "gates"]])
        self.assertEqual(self.warm_execs(), [])

    def test_a_run_goes_where_its_lane_waits_least(self):
        self.gate.answers["ahead"] = ran(b"ahead quick 1\n")
        self.kube.answer("exec", ran(b"ahead quick 0\n"), ran(b"run w1\nread it with: build.sh status w1\n"))
        status, answer = self.built("programs", "dev", "quick", "-p", "chat")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, f"run {WARM}:w1"))
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "ahead", "programs", "dev", "quick", "-p", "chat"]), (WARM, None, ["sh", BUILD, "programs", "dev", "quick", "-p", "chat"])])
        self.assertEqual(self.gate.words(), [["ping"], ["exec", "ahead", "programs", "dev", "quick", "-p", "chat"]])

    def test_an_equal_wait_goes_to_the_desktop(self):
        self.gate.answers["ahead"] = ran(b"ahead heavy 2\n")
        self.kube.answer("exec", ran(b"ahead heavy 2\n"))
        self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "ahead", "programs", "dev", "gates"])])

    def test_a_desktop_that_cannot_say_its_wait_sends_the_run_to_the_warm_pod_and_logs_none_of_its_words(self):
        self.gate.answers["ahead"] = ran(code=2, err="build.sh: a repo is a forge repository name, not /home/x/secret.sh")
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, f"run {WARM}:w1"))
        said.assert_any_call("pods_routed", to="warm", reason="runner_unasked", runner="desktop", detail=json.dumps("build.sh ahead on pod desktop answered exit 2 with no ahead line"))
        self.assertNotIn("secret.sh", repr(said.call_args_list))

    def test_a_warm_pod_that_cannot_say_its_wait_leaves_the_run_on_the_desktop(self):
        self.gate.answers["ahead"] = ran(b"ahead heavy 3\n")
        self.kube.answer("exec", ran(code=2, err="build.sh: usage"))
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r1"))
        said.assert_any_call("pods_routed", to="desktop", reason="warm_unasked", detail=json.dumps(f"build.sh ahead on pod {WARM} answered exit 2 with no ahead line"))

    def test_a_desktop_that_is_down_sends_the_run_to_the_warm_pod(self):
        self.gate.answers["ping"] = ran(code=69, err="gate: runner_down")
        self.assertEqual(self.built("programs", "dev", "check")[1]["stdout"].splitlines()[0], f"run {WARM}:w1")

    def test_a_read_only_run_whose_desktop_session_broke_runs_again_on_the_warm_pod(self):
        self.gate.cut_words = {"gates"}
        status, answer = self.built("WAIT=1", "programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, f"run {WARM}:w1"))
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "programs", "dev", "gates"]), (WARM, None, ["sh", BUILD, "wait", "w1"])])
        self.assertEqual(self.service.launched, {("WAIT=1", "programs", "dev", "gates"): (WARM, "w1")})

    def test_a_run_that_changes_things_whose_desktop_session_broke_and_cannot_be_read_again_is_parked_with_its_run(self):
        self.gate.cut_words = {"apply"}
        self.gate.cut_said = b"run r7\n"
        self.gate.sequence["exec wait"] = [ran(code=69, err="gate: runner_down")]
        with unittest.mock.patch.object(server, "say") as said:
            self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "WAIT=1", "apply", "programs", "dev", "/home/x/edit.sh"]}, 502, ["parked", "build.sh apply", "as run desktop:r7", "build.sh status desktop:r7"])
        self.assertEqual(self.warm_execs(), [])
        said.assert_any_call("pods_parked", verb=json.dumps("apply"), run="desktop:r7", detail=unittest.mock.ANY)

    def test_a_parked_run_that_never_started_points_at_busy(self):
        self.gate.cut_words = {"apply"}
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "apply", "programs", "dev", "/home/x/edit.sh"]}, 502, ["build.sh busy on pod desktop"])

    def test_a_desktop_that_only_looks_free_sends_the_run_to_the_warm_pod(self):
        quiet = "programs 20261019-2 quiet j-1 [1m] programs dev sh measure"
        light = "programs 20261019-3 light heavy [1m] programs dev show x"
        stray = "stray 412 holds /var/tmp/warm/heavy.lock: sleep"
        pings = [
            {**UP, "busy": [quiet]},
            {**UP, "busy_exit": 1},
            {**UP, "busy": HEAVY},
            {**UP, "busy_exit": "0"},
            {**UP, "busy_exit": True},
            {**UP, "busy": [stray]},
        ]
        for request, ping in enumerate(pings, start=60):
            self.gate.answers["ping"] = ran(ping)
            self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(b"run w1\nread it with: build.sh status w1\n"))
            status, answer = self.built("programs", "dev", "gates", request=request)
            self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, f"run {WARM}:w1"), ping)
        self.gate.answers["ping"] = ran({**UP, "busy": [light]})
        self.assertEqual(self.built("programs", "dev", "gates", request=70)[1]["stdout"].splitlines()[0], "run desktop:r1")

    def test_a_busy_line_that_names_no_weight_is_skipped(self):
        split = ["programs 20261020-1 light j-126 [1m] programs 1a246b60 sh cat > /tmp/x.py <<'PY'", "    s.sendall(f\"GET {path} HTTP/1.1", "Host: {HOST}", "", "\".encode())", "PY"]
        for request, busy in enumerate((split, ["programs r bogus j-1 [1m] programs dev gates"], ["programs r"], [""]), start=80):
            self.gate.answers["ping"] = ran({**UP, "busy": busy})
            self.assertEqual(self.built("programs", "dev", "gates", request=request)[1]["stdout"].splitlines()[0], "run desktop:r1", busy)
        self.assertEqual(self.warm_execs(), [])

    def test_skipped_busy_lines_beside_a_quiet_run_or_a_stray_holder_still_send_the_run_to_the_warm_pod(self):
        split = ["programs 20261020-1 light j-126 [1m] programs 1a246b60 sh cat > /tmp/x.py <<'PY'", "Host: {HOST}", "", "PY"]
        beside = ("programs 20261019-2 quiet j-1 [1m] programs dev sh measure", "stray 412 holds /var/tmp/warm/heavy.lock: sleep")
        for request, line in enumerate(beside, start=90):
            self.gate.answers["ping"] = ran({**UP, "busy": [*split, line]})
            self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(b"run w1\nread it with: build.sh status w1\n"))
            self.assertEqual(self.built("programs", "dev", "gates", request=request)[1]["stdout"].splitlines()[0], f"run {WARM}:w1", line)

    def test_a_run_goes_where_it_ends_first_when_both_machines_say_when(self):
        self.gate.answers["ahead"] = ran(b"ahead heavy 20 4546\n")
        self.kube.answer("exec", ran(b"ahead heavy 19 3590\n"), ran(b"run w1\n"))
        with unittest.mock.patch.object(server, "say") as said:
            self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], f"run {WARM}:w1")
        said.assert_any_call("pods_routed", to="warm", reason="sooner_end", detail=json.dumps(f"heavy: ends in 4546s on pod desktop, 3590s on pod {WARM}"))

    def test_a_run_stays_on_the_desktop_that_ends_it_first_though_more_wait_there(self):
        self.gate.answers["ahead"] = ran(b"ahead heavy 6 1200\n")
        self.kube.answer("exec", ran(b"ahead heavy 2 1500\n"))
        self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")

    def test_an_equal_end_goes_to_the_desktop(self):
        self.gate.answers["ahead"] = ran(b"ahead heavy 3 900\n")
        self.kube.answer("exec", ran(b"ahead heavy 1 900\n"))
        self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")

    def test_a_machine_with_no_run_time_to_go_by_leaves_the_pick_to_the_counts(self):
        for request, (desktop, warm, where) in enumerate(
            (
                (b"ahead heavy 2 -\n", b"ahead heavy 0 99999\n", f"run {WARM}:w1"),
                (b"ahead heavy 2 10\n", b"ahead heavy 0 -\n", f"run {WARM}:w1"),
                (b"ahead heavy 2\n", b"ahead heavy 3 10\n", "run desktop:r1"),
            ),
            start=110,
        ):
            self.gate.answers["ahead"] = ran(desktop)
            self.kube.answer("exec", ran(warm), ran(b"run w1\n"))
            self.assertEqual(self.built("programs", "dev", "gates", request=request)[1]["stdout"].splitlines()[0], where, (desktop, warm))

    def test_routing_logs_a_token_and_its_sentence_apart(self):
        self.gate.answers["ahead"] = ran(b"ahead heavy 2\n")
        self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(b"run w1\n"))
        with unittest.mock.patch.object(server, "say") as said:
            self.built("programs", "dev", "gates")
        said.assert_any_call("pods_routed", to="warm", reason="shorter_wait", detail=json.dumps(f"heavy: 2 ahead on pod desktop, 0 on pod {WARM}"))

    def test_a_ping_that_does_not_read_logs_its_field_and_type_not_its_lines(self):
        self.gate.answers["ping"] = ran({**UP, "busy": "programs r1 heavy j-1 [1m] programs dev apply /home/x/secret.sh"})
        self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(b"run w1\n"))
        with unittest.mock.patch.object(server, "say") as said:
            self.built("programs", "dev", "gates")
        said.assert_any_call("pods_routed", to="warm", reason="runner_busy", runner="desktop", detail=json.dumps("the desktop's ping has busy as a str, not a list of lines"))
        self.assertNotIn("secret.sh", repr(said.call_args_list))

    def test_a_log_through_builds_keeps_its_lines_as_they_are(self):
        self.gate.answers["exec"] = ran(b"run 5\n")
        self.assertEqual(self.built("log", "desktop:r1")[1]["stdout"], "run 5\n")

    def test_a_requeue_through_builds_names_the_machine_of_its_new_run(self):
        self.gate.answers["exec"] = ran(b"run r9\n")
        status, answer = self.built("requeue", "desktop:r1")
        self.assertEqual((status, answer["stdout"]), (200, "run desktop:r9\n"))
        self.assertTrue(answer["stderr"].startswith("builds: on desktop"))

    def test_a_run_is_read_on_the_machine_its_id_names(self):
        self.gate.answers["exec"] = ran(b"GATES ok\n")
        self.assertEqual(self.built("status", "desktop:r1")[1]["stdout"], "GATES ok\n")
        self.assertEqual(self.gate.words(), [["exec", "status", "r1"]])
        self.built("wait", f"{WARM}:w1", request=42)
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "wait", "w1"])])

    def test_failures_and_diag_read_the_run_on_the_machine_its_id_names(self):
        self.gate.answers["exec"] = ran(b"== r1/test.log\ntest a ... FAILED\n")
        status, answer = self.built("failures", "desktop:r1")
        self.assertEqual((status, answer["stdout"]), (200, "== r1/test.log\ntest a ... FAILED\n"))
        self.assertTrue(answer["stderr"].startswith("builds: on desktop"))
        self.assertEqual(self.gate.words(), [["exec", "failures", "r1"]])
        self.built("diag", f"{WARM}:w1", request=42)
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "diag", "w1"])])

    def test_each_run_a_call_names_on_its_machine_goes_without_the_machine(self):
        self.kube.answer("exec", ran(b"stopped w1 (job j-1)\nstopped w2 (job j-1)\n"))
        self.assertEqual(self.built("stop", f"{WARM}:w1", f"{WARM}:w2", "w3")[0], 200)
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "stop", "w1", "w2", "w3"])])

    def test_a_report_is_checked_only_against_a_build_machines_own_record(self):
        self.gate.answers["exec"] = ran(lane_run("r1").encode())
        self.kube.answer("exec", ran(lane_run("w1").encode()))
        self.assertIsNone(self.service.lane_record("duckpond-41", "r9"))
        self.assertIn("run w1: programs", self.service.lane_record(WARM, "w1"))
        self.assertIn("run r1: programs", self.service.lane_record("desktop", "r1"))
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "status", "w1"])])
        self.assertEqual(self.gate.words(), [["exec", "status", "r1"]])
        self.assertIs(self.service.notifier.records.__func__, server.Service.lane_record)

    def test_a_bump_through_builds_follows_its_run_to_the_machine_it_moved_to(self):
        self.gate.answers["exec"] = ran(code=2, err="build.sh: no run w1 in pod orthorymain (a run of an earlier warm pod is gone with it)")
        self.gate.answers["exec bump"] = ran(b"bumped r9: place 1 in the pod's heavy-run queue\n")
        self.kube.answer("exec", ran(b"job j-1\nmoved to run desktop:r9 by build.sh moved\n"), ran(code=2, err="build.sh: that run was moved to run desktop:r9 on another machine: bump it there"))
        status, answer = self.built("bump", "w1")
        self.assertEqual((status, answer["stdout"]), (200, "bumped r9: place 1 in the pod's heavy-run queue\n"))
        self.assertIn("w1 bumped on desktop", answer["stderr"])
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "status", "w1"]), (WARM, None, ["sh", BUILD, "bump", "w1"])])
        self.assertEqual(self.gate.words(), [["exec", "status", "w1"], ["exec", "bump", "r9"]])

    def test_a_bump_through_builds_bumps_each_run_on_its_machine_in_the_order_named(self):
        self.gate.answers["exec bump"] = ran(b"bumped r1: place 1 in the pod's heavy-run queue\n")
        self.kube.answer("exec", ran(b"bumped w1: place 1 in the pod's heavy-run queue\n"))
        status, answer = self.built("bump", "desktop:r1", f"{WARM}:w1")
        self.assertEqual((status, answer["stdout"]), (200, "bumped r1: place 1 in the pod's heavy-run queue\nbumped w1: place 1 in the pod's heavy-run queue\n"))
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "bump", "w1"])])
        self.assertEqual(self.gate.words(), [["exec", "bump", "r1"]])

    def test_runs_on_two_machines_in_one_call_are_refused(self):
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "stop", f"{WARM}:w1", "desktop:r1"]}, 400, ["one machine per call", "desktop:r1 names pod desktop", "call it once for each"])
        self.assertEqual((self.warm_execs(), self.gate.words()), ([], []))

    def test_a_bare_run_id_is_read_on_the_desktop_that_holds_it(self):
        self.gate.answers["exec"] = ran(b"job j-1\nsh exit 0\n")
        status, answer = self.built("status", "r1")
        self.assertEqual((status, answer["stdout"]), (200, "job j-1\nsh exit 0\n"))
        self.assertTrue(answer["stderr"].startswith("builds: on desktop"))
        self.assertEqual(self.gate.words(), [["exec", "status", "r1"]])
        self.assertEqual(self.warm_execs(), [])

    def test_a_bare_run_id_the_desktop_lacks_is_read_on_the_warm_pod(self):
        self.gate.answers["exec"] = ran(code=2, err="build.sh: no run w1 in pod orthorymain (a run of an earlier warm pod is gone with it)")
        self.kube.answer("exec", ran(b"job j-1\nGATES ok\n"))
        status, answer = self.built("wait", "w1")
        self.assertEqual((status, answer["stdout"]), (200, "job j-1\nGATES ok\n"))
        self.assertTrue(answer["stderr"].startswith(f"builds: on {WARM}"))
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "status", "w1"]), (WARM, None, ["sh", BUILD, "wait", "w1"])])

    def test_a_bare_run_id_no_machine_holds_is_refused_naming_both(self):
        self.gate.answers["exec"] = ran(code=2, err="build.sh: no run r0 in pod orthorymain (a run of an earlier warm pod is gone with it)")
        self.kube.answer("exec", ran(code=2, err=f"build.sh: no run r0 in pod {WARM} (a run of an earlier warm pod is gone with it)"))
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "stop", "r0"]}, 404, ["no machine of pod builds holds run r0", "pod desktop has no run r0", f"pod {WARM} has no run r0"])

    def test_a_bare_run_id_skips_a_desktop_that_does_not_answer(self):
        self.gate.down = {"lan", "tail", "wifi"}
        self.kube.answer("exec", ran(b"job j-1\nGATES ok\n"))
        self.assertTrue(self.built("log", "w1")[1]["stderr"].startswith(f"builds: on {WARM}"))

    def test_a_bare_run_id_is_never_called_gone_while_a_machine_could_not_be_asked(self):
        self.gate.down = {"lan", "tail", "wifi"}
        self.kube.answer("exec", ran(code=2, err=f"build.sh: no run r1 in pod {WARM} (a run of an earlier warm pod is gone with it)"))
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "stop", "r1"]}, 503, ["not every machine of pod builds could be asked", "pod desktop could not be asked", f"pod {WARM} has no run r1", "<machine>:<run>"])
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "status", "r1"])])

    def test_a_bare_run_id_holds_only_where_status_prints_its_job_line(self):
        for request, answer in enumerate((ran(code=125, err="Error: no container with name or ID \"build-runner\" found"), ran(code=64, err="gate: refused")), start=80):
            self.kube.calls.clear()
            self.gate.calls.clear()
            self.gate.answers["exec"] = answer
            self.kube.answer("exec", ran(b"job j-1\nstopped r1 (job j-1)\n"))
            status, said = self.built("stop", "r1", request=request)
            self.assertEqual(status, 200, said)
            self.assertTrue(said["stderr"].startswith(f"builds: on {WARM}"))
            self.assertEqual(self.gate.words(), [["exec", "status", "r1"]])
            self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "status", "r1"]), (WARM, None, ["sh", BUILD, "stop", "r1"])])

    def test_a_bare_run_id_with_no_warm_pod_named_says_so(self):
        self.service.warm_pod = None
        self.gate.answers["exec"] = ran(code=2, err="build.sh: no run r1 in pod orthorymain (a run of an earlier warm pod is gone with it)")
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "status", "r1"]}, 503, ["the warm pod could not be asked: this service names no warm pod (PODS_WARM_POD)", "pod desktop has no run r1"])

    def test_a_run_named_without_its_machine_or_a_machine_wide_call_is_refused(self):
        refusals = ((["sh", BUILD, "status"], "names its run"), (["sh", BUILD, "busy"], "acts on one machine"), (["build.sh", "busy"], "acts on one machine"), (["env", "JOB=j-1", "build.sh", "busy"], "acts on one machine"), (["sh", BUILD, "disk-targets", "programs"], "acts on one machine"), (["make"], "pod builds runs build.sh alone"))
        for request, (command, words) in enumerate(refusals, start=50):
            status, answer = self.call("POST", "/v1/pods/builds/exec", {"command": command}, headers=asked(request))
            self.assertEqual(status, 400, answer)
            self.assertIn(words, answer["error"])
        self.assertEqual(self.gate.calls, [])

    def test_a_call_whose_request_is_gone_before_build_sh_is_called_runs_nothing(self):
        self.signer.request = ran(GONE)
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "programs", "dev", "gates"]}, 502, ["is gone, so build.sh was not called on pod desktop: nothing ran"])
        self.assertNotIn(["exec", "programs", "dev", "gates"], self.gate.words())

    def test_a_call_whose_request_egress_cannot_be_asked_about_runs_and_says_so(self):
        self.signer.request = ran(code=1, err="egress: no such query")
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r1"))
        said.assert_any_call("pods_request_unchecked", request="41", detail=json.dumps("egress was not asked whether request 41 still waits: egress: no such query"))

    def test_a_retry_whose_first_answer_never_came_gets_the_run_its_call_started(self):
        del self.service.claim
        self.built("JOB=j-1", "programs", "dev", "gates")
        self.gate.answers["exec claim"] = ran(b"same r1\n")
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("JOB=j-1", "programs", "dev", "gates", request=42)
        self.assertEqual((status, answer["stdout"]), (200, "run desktop:r1\nread it with: build.sh status r1\n"))
        self.assertIn("it is still queued or running there, so this call gets that run rather than a second one", answer["stderr"])
        runs = [words for words in self.gate.words() if words[:1] == ["exec"] and words[1:2] not in (["ahead"], ["claim"])]
        self.assertEqual(runs, [["exec", "JOB=j-1", "programs", "dev", "gates"]])
        self.assertEqual(self.gate.words()[-1], ["exec", "claim", "r1", "JOB=j-1", "programs", "dev", "gates"])
        said.assert_any_call("pods_run_claimed", run=json.dumps("desktop:r1"), request="42")

    def test_a_held_retry_waits_on_the_run_its_call_started(self):
        del self.service.claim
        self.built("WAIT=1", "programs", "dev", "check")
        self.gate.answers["exec claim"] = ran(b"same r1\n")
        self.gate.answers["exec wait"] = ran(b"run r1: programs dev, check\nGATES ok\n")
        status, answer = self.built("WAIT=1", "programs", "dev", "check", request=42)
        self.assertEqual((status, answer["stdout"]), (200, "run desktop:r1\nrun r1: programs dev, check\nGATES ok\n"))
        self.assertEqual(self.gate.words()[-1], ["exec", "wait", "r1"])

    def test_a_retry_follows_a_run_reroute_moved_to_the_run_that_stands_for_it(self):
        del self.service.claim
        self.gate.answers["ahead"] = ran(b"ahead heavy 2\n")
        self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(b"run w1\nread it with: build.sh status w1\n"), ran(b"moved desktop:r9\n"))
        self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], f"run {WARM}:w1")
        self.gate.answers["exec claim"] = ran(b"same r9\n")
        status, answer = self.built("programs", "dev", "gates", request=42)
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r9"))
        self.assertEqual(self.warm_execs()[-1], (WARM, None, ["sh", BUILD, "claim", "w1", "programs", "dev", "gates"]))
        self.assertEqual(self.gate.words()[-1], ["exec", "claim", "r9", "programs", "dev", "gates"])
        self.assertEqual(self.service.launched[("programs", "dev", "gates")], ("desktop", "r9"))

    def test_a_retry_whose_run_cannot_be_checked_gets_that_run_and_never_a_second(self):
        del self.service.claim
        self.built("programs", "dev", "gates")
        self.gate.down = {"lan", "tail", "wifi"}
        status, answer = self.built("programs", "dev", "gates", request=42)
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r1"))
        self.assertIn("it was not checked there", answer["stderr"])
        self.assertEqual(self.warm_execs(), [])
        self.assertEqual(self.service.launched[("programs", "dev", "gates")], ("desktop", "r1"))

    def test_a_retry_whose_run_is_not_the_one_its_words_make_now_starts_its_own(self):
        del self.service.claim
        self.built("programs", "j/x", "gates")
        self.gate.answers["exec claim"] = ran(b"other r1 took aa, and j/x is at bb now\n", code=1)
        self.gate.answers["exec"] = ran(b"run r2\nread it with: build.sh status r2\n")
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("programs", "j/x", "gates", request=42)
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r2"))
        self.assertEqual(self.service.launched[("programs", "j/x", "gates")], ("desktop", "r2"))
        said.assert_any_call("pods_run_unclaimed", run=json.dumps("desktop:r1"), reason=json.dumps("other"))

    def test_a_held_call_records_its_run_before_it_holds_on_it(self):
        seen = []
        asked_on = self.service.asked_on

        def watching(asked, machine, words, alive):
            if words[:1] == ["wait"]:
                seen.append(dict(self.service.launched))
            return asked_on(asked, machine, words, alive)

        self.service.asked_on = watching
        self.gate.answers["exec wait"] = ran(b"run r1: programs dev, check\nGATES ok\n")
        status, answer = self.built("WAIT=1", "programs", "dev", "check")
        self.assertEqual((status, answer["stdout"]), (200, "run desktop:r1\nrun r1: programs dev, check\nGATES ok\n"))
        self.assertEqual(seen, [{("WAIT=1", "programs", "dev", "check"): ("desktop", "r1")}])
        self.assertEqual([words for words in self.gate.words() if words[:1] == ["exec"] and words[1:2] != ["ahead"]], [["exec", "programs", "dev", "check"], ["exec", "wait", "r1"]])

    def test_a_held_call_whose_desktop_session_broke_mid_wait_and_cannot_be_asked_again_is_parked_with_its_run(self):
        self.gate.sequence["exec wait"] = [ran(code=255, err="Connection to lan closed by remote host."), ran(code=69, err="gate: runner_down")]
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "WAIT=1", "apply", "programs", "dev", "/home/x/edit.sh"]}, 502, ["parked", "as run desktop:r1"])
        self.assertEqual(self.service.launched[("WAIT=1", "apply", "programs", "dev", "/home/x/edit.sh")], ("desktop", "r1"))

    def test_a_cut_desktop_session_that_printed_its_run_is_recorded(self):
        self.gate.cut_words = {"apply"}
        self.gate.cut_said = b"run r7\n"
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "apply", "programs", "dev", "/home/x/edit.sh"]}, 502, ["as run desktop:r7"])
        self.assertEqual(self.service.launched, {("apply", "programs", "dev", "/home/x/edit.sh"): ("desktop", "r7")})

    def test_a_run_the_desktop_lost_and_the_warm_pod_ran_is_recorded_there(self):
        self.gate.cut_words = {"gates"}
        self.built("programs", "dev", "gates")
        self.assertEqual(self.service.launched, {("programs", "dev", "gates"): (WARM, "w1")})

    def test_a_retry_whose_branch_forge_cannot_place_gets_that_run_and_keeps_its_record(self):
        del self.service.claim
        self.built("programs", "dev", "gates")
        self.gate.answers["exec claim"] = ran(b"unknown forge did not say where programs dev points\n", code=3)
        status, answer = self.built("programs", "dev", "gates", request=42)
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r1"))
        self.assertIn("it was not checked there (forge did not say where programs dev points)", answer["stderr"])
        self.assertEqual(self.service.launched[("programs", "dev", "gates")], ("desktop", "r1"))
        self.assertEqual([words for words in self.gate.words() if words[:1] == ["exec"] and words[1:2] not in (["ahead"], ["claim"])], [["exec", "programs", "dev", "gates"]])

    def test_the_sweep_drops_ended_runs_follows_moved_ones_and_keeps_those_it_cannot_ask(self):
        self.built("programs", "dev", "gates")
        self.gate.answers["ahead"] = ran(b"ahead heavy 2\n")
        self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(b"run w1\nread it with: build.sh status w1\n"))
        self.built("programs", "dev", "check", request=42)
        self.service.launched[("core", "dev", "gates")] = ("desktop", "r3")
        self.gate.answers["exec ended"] = ran(b"ended r1\n")
        self.kube.answer("exec", ran(b"moved w1 desktop:r5\n"))
        self.service.sweep()
        self.assertEqual(self.service.launched, {("programs", "dev", "check"): ("desktop", "r5"), ("core", "dev", "gates"): ("desktop", "r3")})
        self.assertIn(["exec", "ended", "r1", "r3"], self.gate.words())
        self.assertEqual(self.warm_execs()[-1], (WARM, None, ["sh", BUILD, "ended", "w1"]))
        self.gate.down = {"lan", "tail", "wifi"}
        self.service.sweep()
        self.assertEqual(len(self.service.launched), 2)
        self.service.launched = {("programs", "dev", "check"): (WARM, "w1")}
        self.kube.answer("exec", ran(code=1, err=f'Error from server (NotFound): pods "{WARM}" not found'))
        self.service.sweep()
        self.assertEqual(self.service.launched, {})

    def test_a_commit_only_the_desktop_holds_runs_there_though_the_warm_pod_waits_less(self):
        made = "f1916715" + "0" * 32
        self.gate.answers["ahead"] = ran(b"ahead heavy 2\n")
        self.gate.answers["exec holds"] = ran(f"holds {made}\n".encode())
        self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(f"lacks {made}\n".encode(), code=1), ran(b"run w1\n"))
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("JOB=j-1", "programs", made, "lane")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r1"))
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "ahead", "JOB=j-1", "programs", made, "lane"]), (WARM, None, ["sh", BUILD, "holds", "programs", made])])
        self.assertEqual(self.gate.words()[-2:], [["exec", "holds", "programs", made], ["exec", "JOB=j-1", "programs", made, "lane"]])
        said.assert_any_call("pods_routed", to="desktop", reason="holds_commit", detail=json.dumps(f"programs {made} is in pod desktop's clone and not in pod {WARM}'s"))

    def test_an_unpushed_apply_base_the_warm_pod_holds_goes_there(self):
        made = "abc1234"
        self.gate.answers["exec holds"] = ran(f"lacks {made}\n".encode(), code=1)
        self.kube.answer("exec", ran(f"holds {made}{'0' * 33}\n".encode()), ran(b"run w1\n"))
        status, answer = self.built("apply", "--no-gate", "programs", made, "/home/x/edit.sh", "-m", "a subject")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, f"run {WARM}:w1"))
        self.assertEqual(self.warm_execs()[0], (WARM, None, ["sh", BUILD, "holds", "programs", made]))

    def test_a_commit_the_picked_machine_holds_asks_no_other(self):
        made = "f1916715" + "0" * 32
        self.gate.answers["exec holds"] = ran(f"holds {made}\n".encode())
        self.assertEqual(self.built("programs", made, "lane")[1]["stdout"].splitlines()[0], "run desktop:r1")
        self.assertEqual(self.warm_execs(), [])

    def test_a_commit_no_clone_holds_runs_where_it_waits_least_and_fetches_it(self):
        made = "f1916715" + "0" * 32
        self.gate.answers["exec holds"] = ran(f"lacks {made}\n".encode(), code=1)
        self.kube.answer("exec", ran(f"lacks {made}\n".encode(), code=1))
        self.assertEqual(self.built("programs", made, "lane")[1]["stdout"].splitlines()[0], "run desktop:r1")
        self.assertEqual(self.gate.words()[-1], ["exec", "programs", made, "lane"])

    def test_a_holds_that_fails_on_the_picked_machine_keeps_the_pick_and_asks_no_other(self):
        made = "f1916715" + "0" * 32
        self.gate.answers["exec holds"] = ran(code=2, err="build.sh: usage: build.sh holds <repo> <commit>")
        self.assertEqual(self.built("programs", made, "lane")[1]["stdout"].splitlines()[0], "run desktop:r1")
        self.assertEqual(self.warm_execs(), [])

    def test_an_answer_from_the_other_machine_that_is_neither_holds_nor_lacks_keeps_the_pick(self):
        made = "f1916715" + "0" * 32
        self.gate.answers["ahead"] = ran(b"ahead heavy 2\n")
        self.gate.answers["exec holds"] = ran(b"run r9\nread it with: build.sh status r9\n")
        self.kube.answer("exec", ran(b"ahead heavy 0\n"), ran(f"lacks {made}\n".encode(), code=1), ran(b"run w1\n"))
        self.assertEqual(self.built("programs", made, "lane")[1]["stdout"].splitlines()[0], f"run {WARM}:w1")
        self.assertNotIn(["exec", "programs", made, "lane"], self.gate.words())

    def test_an_apply_flag_value_that_looks_like_a_commit_is_not_its_base(self):
        self.built("apply", "programs", "-m", "abc1234", "dev", "/home/x/edit.sh")
        self.assertNotIn("holds", [words[1] for words in self.gate.words() if len(words) > 1])
        self.assertNotIn("holds", [call[2][2] for call in self.warm_execs() if len(call[2]) > 2])

    def test_a_branch_is_never_asked_after(self):
        self.built("programs", "dev", "lane")
        self.assertNotIn("holds", [words[1] for words in self.gate.words() if len(words) > 1])

    def test_a_call_to_a_pod_that_is_gone_names_the_warm_pod_now(self):
        self.kube.answer("get", ran(code=1, err='Error from server (NotFound): pods "duckpond-53809" not found'))
        self.refused("GET", "/v1/pods/duckpond-53809", None, 404, ['pods "duckpond-53809" not found', f"if pod duckpond-53809 was the warm pod, it has been replaced: the warm pod is now {WARM}"])

    def test_a_call_to_the_warm_pod_itself_that_finds_nothing_names_no_other(self):
        self.kube.answer("get", ran(code=1, err=f'Error from server (NotFound): pods "{WARM}" not found'))
        status, answer = self.call("GET", f"/v1/pods/{WARM}")
        self.assertEqual(status, 404)
        self.assertNotIn("has been replaced", answer["error"])

    def test_a_warm_pod_file_that_cannot_be_read_leaves_the_404_as_it_was(self):
        self.signer.head = ran(code=1, err="ducktape: files could not read its query")
        self.kube.answer("get", ran(code=1, err='Error from server (NotFound): pods "duckpond-53809" not found'))
        status, answer = self.call("GET", "/v1/pods/duckpond-53809")
        self.assertEqual((status, answer["error"]), (404, 'pod duckpond-53809 was not read: Error from server (NotFound): pods "duckpond-53809" not found'))

    def test_a_404_from_builds_names_no_warm_pod(self):
        self.gate.answers["exec"] = ran(code=2, err="build.sh: no run r0 in pod orthorymain (a run of an earlier warm pod is gone with it)")
        self.kube.answer("exec", ran(code=2, err=f"build.sh: no run r0 in pod {WARM} (a run of an earlier warm pod is gone with it)"))
        status, answer = self.call("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "stop", "r0"]}, headers=asked(90))
        self.assertEqual(status, 404, answer)
        self.assertNotIn("has been replaced", answer["error"])

    def test_with_no_warm_pod_named_a_gone_pod_is_a_bare_404(self):
        self.service.warm_pod = None
        self.kube.answer("get", ran(code=1, err='Error from server (NotFound): pods "duckpond-53809" not found'))
        status, answer = self.call("GET", "/v1/pods/duckpond-53809")
        self.assertEqual(status, 404)
        self.assertNotIn("has been replaced", answer["error"])

    def test_reroute_moves_each_queued_run_the_desktop_would_start_sooner(self):
        self.kube.answer("exec", ran(b"move w2 heavy 4 d29yZHMy\nmove w1 heavy 3 d29yZHMx\nkeep w3 a pack holds its result on this machine\n"), ran(b"w1: moved to run desktop:r1\nw2: moved to run desktop:r2\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 1\nlane 0\nquick:programs 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\nrun r2\n")
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("reroute")
        self.assertEqual(status, 200, answer)
        self.assertEqual(answer["stdout"].splitlines(), ["keep w3 a pack holds its result on this machine", "w1: moved to run desktop:r1", "w2: moved to run desktop:r2"])
        self.assertEqual(self.gate.words(), [["exec", "lanes"], ["exec", "take", "d29yZHMx", "d29yZHMy"]])
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "movable"]), (WARM, None, ["sh", BUILD, "moved", "w1", "desktop:r1", "w2", "desktop:r2"])])
        said.assert_any_call("pods_rerouted", moved=2, kept=1)

    def test_reroute_keeps_a_run_the_desktop_would_not_start_sooner(self):
        self.kube.answer("exec", ran(b"move w1 heavy 2 d29yZHMx\nmove w4 quick:chat 1 d29yZHM0\n"), ran(b"w4: moved to run desktop:r4\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 2\nlane 0\n")
        self.gate.answers["exec take"] = ran(b"run r4\n")
        answer = self.built("reroute")[1]
        self.assertIn("keep w1 the heavy lane waits for 2 runs on pod desktop and 2 here", answer["stdout"])
        self.assertEqual(self.gate.words(), [["exec", "lanes"], ["exec", "take", "d29yZHM0"]])

    def test_reroute_ends_the_desktop_copy_of_a_run_that_started_meanwhile_posting_nothing(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"), ran(b"w1: started or ended, so it stays here, and run desktop:r1 should yield to it\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\n")
        self.gate.answers["exec yield"] = ran(f"r1: yielded to run {WARM}:w1\n".encode())
        answer = self.built("reroute")[1]
        self.assertEqual(self.gate.words()[-1], ["exec", "yield", "r1", f"{WARM}:w1"])
        self.assertEqual(answer["stdout"].splitlines(), [f"w1: started before it moved, so it stays on pod {WARM}; its copy desktop:r1 yielded to it"])

    def test_reroute_names_a_copy_that_ended_before_yield_came_as_ended_not_running(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"), ran(b"w1: started or ended, so it stays here, and run desktop:r1 should yield to it\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\n")
        self.gate.answers["exec yield"] = ran(b"r1: ended already (exit 1)\n")
        lines = self.built("reroute")[1]["stdout"].splitlines()
        self.assertEqual(lines, [f"w1: started before it moved, so it stays on pod {WARM}; its copy desktop:r1 had already ended (exit 1), so each of the two posts its own end"])

    def test_reroute_names_a_copy_it_could_not_end(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"), ran(b"w1: started or ended, so it stays here, and run desktop:r1 should yield to it\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\n")
        self.gate.answers["exec yield"] = ran(code=2, err="build.sh: usage")
        self.assertIn(f"unstopped desktop:r1: it runs beside {WARM}:w1, which stays (build.sh yield answered exit 2); stop it", self.built("reroute")[1]["stdout"])

    def test_reroute_reads_again_a_move_the_warm_pod_did_not_answer_and_names_both_runs_when_it_never_does(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\nmove w2 heavy 4 d29yZHMy\n"), ran(code=2), ran(b"w1: moved to run desktop:r1\n"), ran(code=2))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\nrun r2\n")
        lines = self.built("reroute")[1]["stdout"].splitlines()
        self.assertIn("w1: moved to run desktop:r1", lines)
        self.assertIn(f"unknown w2: pod {WARM} never said whether it ended (build.sh moved on pod {WARM} answered exit 0 without a line for it), so desktop:r2 may run beside it", lines)
        moves = [call for call in self.warm_execs() if call[2][2] == "moved"]
        self.assertEqual([call[2][3:] for call in moves], [["w1", "desktop:r1", "w2", "desktop:r2"], ["w1", "desktop:r1", "w2", "desktop:r2"], ["w2", "desktop:r2"]][: len(moves)])

    def test_reroute_says_why_the_warm_pod_never_said_a_move_ended(self):
        lost = ran(code=1, err="error: websocket: close 1006 (abnormal closure): unexpected EOF")
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"), lost, lost)
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\n")
        lines = self.built("reroute")[1]["stdout"].splitlines()
        self.assertEqual(lines, [f"unknown w1: pod {WARM} never said whether it ended (apiserver unavailable: the exec's connection to pod {WARM} was cut while it ran (error: websocket: close 1006 (abnormal closure): unexpected EOF); its command may still be running there, so it was not run again), so desktop:r1 may run beside it"])

    def test_a_take_cut_mid_call_names_the_runs_it_may_have_started(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.cut_words = {"take"}
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "reroute"]}, 502, ["may have started some of these runs", f"each still queued on pod {WARM} (w1)", "build.sh busy on pod desktop"])

    def test_reroute_follows_the_desktop_log_before_it_moves_a_run_that_notifies(self):
        notifying = base64.b64encode(b"JOB=j-1\0NOTIFY=j-1\0programs\0dev\0gates\0").decode()
        self.kube.answer("exec", ran(f"move w1 heavy 3 {notifying}\n".encode()), ran(b"w1: moved to run desktop:r1\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\n")
        with unittest.mock.patch.object(self.service, "follow") as followed:
            self.built("reroute")
        followed.assert_called_once_with("desktop")

    def test_reroute_keeps_a_second_run_once_the_first_move_fills_the_desktops_lane(self):
        self.kube.answer("exec", ran(b"move w1 heavy 1 d29yZHMx\nmove w2 heavy 2 d29yZHMy\n"), ran(b"w1: moved to run desktop:r1\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\n")
        answer = self.built("reroute")[1]
        self.assertIn("keep w2 the heavy lane waits for 1 runs on pod desktop and 1 here", answer["stdout"])
        self.assertEqual(self.gate.words()[-1], ["exec", "take", "d29yZHMx"])

    def test_reroute_keeps_a_run_the_desktop_did_not_take(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"failed: build.sh: a commit or branch name, not x\n")
        answer = self.built("reroute")[1]
        self.assertIn("keep w1 pod desktop failed: build.sh: a commit or branch name, not x", answer["stdout"])
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "movable"])])

    def test_reroute_keeps_on_the_warm_pod_a_run_whose_commit_the_desktop_lacks(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\nmove w2 heavy 4 d29yZHMy\n"), ran(b"w1: moved to run desktop:r1\n"))
        self.gate.answers["exec lanes"] = ran(b"heavy 0\n")
        self.gate.answers["exec take"] = ran(b"run r1\nlacks c4ccf325: this machine's clone of programs lacks it after a fetch of forge's branches, so it stays where it was made\n")
        lines = self.built("reroute")[1]["stdout"].splitlines()
        self.assertIn("keep w2 pod desktop lacks c4ccf325: this machine's clone of programs lacks it after a fetch of forge's branches, so it stays where it was made", lines)
        self.assertIn("w1: moved to run desktop:r1", lines)
        self.assertEqual(self.warm_execs()[1:], [(WARM, None, ["sh", BUILD, "moved", "w1", "desktop:r1"])])

    def test_a_call_that_finds_the_desktop_idle_reroutes_once_it_is_sent(self):
        del self.service.reroute_after
        done = threading.Event()
        with unittest.mock.patch.object(self.service, "rerouted", side_effect=lambda *_: done.set()):
            self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")
            done.wait()
        self.assertEqual(self.gate.words()[-1], ["exec", "programs", "dev", "gates"])

    def test_with_no_warm_pod_named_a_desktop_with_a_wait_takes_the_run(self):
        self.service.warm_pod = None
        self.gate.answers["ping"] = ran({**UP, "busy": [HEAVY]})
        self.gate.answers["ahead"] = ran(b"ahead heavy 1\n")
        self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")

    def test_with_no_warm_pod_named_a_desktop_that_cannot_say_its_wait_takes_the_run(self):
        self.service.warm_pod = None
        self.gate.answers["ahead"] = ran(code=2, err="build.sh: a repo is a forge repository name, not Bad")
        with unittest.mock.patch.object(server, "say") as said:
            self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")
        said.assert_any_call("pods_routed", to="desktop", reason="runner_unasked", runner="desktop", detail=unittest.mock.ANY)

    def test_an_empty_warm_pod_file_sends_a_run_the_desktop_judges_busy_to_the_desktop(self):
        self.signer.head = ran(files_answer(b""))
        self.gate.answers["ping"] = ran({**UP, "busy": ["stray 412 holds /var/tmp/warm/heavy.lock: sleep"]})
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("programs", "dev", "show", "README.md")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r1"))
        said.assert_any_call("pods_routed", to="desktop", reason="runner_busy", runner="desktop", detail=json.dumps("a process no run started holds one of the desktop's build locks"))
        said.assert_any_call("pods_routed", to="desktop", reason="warm_unasked", detail=json.dumps("/shared/tools/warm-pod names no warm pod"))

    def test_a_warm_pod_file_naming_a_gone_pod_sends_a_run_the_desktop_judges_busy_to_the_desktop(self):
        self.gate.answers["ping"] = ran({**UP, "busy": ["stray 412 holds /var/tmp/warm/heavy.lock: sleep"]})
        self.kube.answer("exec", ran(code=1, err=f'Error from server (NotFound): pods "{WARM}" not found'))
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run desktop:r1"))
        self.assertEqual(self.warm_execs(), [(WARM, None, ["sh", BUILD, "ahead", "programs", "dev", "gates"])])
        said.assert_any_call("pods_routed", to="desktop", reason="warm_unasked", detail=unittest.mock.ANY)

    def test_a_cut_desktop_run_with_no_warm_pod_named_names_its_run_and_both_reasons(self):
        self.signer.head = ran(files_answer(b""))
        self.gate.cut_words = {"gates"}
        self.gate.cut_said = b"run r7\n"
        self.gate.sequence["exec wait"] = [ran(code=69, err="gate: runner_down")]
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "WAIT=1", "programs", "dev", "gates"]}, 502, ["as run desktop:r7, which may still run there", "/shared/tools/warm-pod names no warm pod", "build.sh status desktop:r7"])

    def test_an_empty_warm_pod_file_sends_a_light_call_to_an_idle_desktop(self):
        self.signer.head = ran(files_answer(b""))
        self.assertEqual(self.built("programs", "dev", "show", "README.md")[1]["stdout"].splitlines()[0], "run desktop:r1")

    def test_with_no_warm_pod_named_a_desktop_that_is_down_answers_503(self):
        self.service.warm_pod = None
        self.gate.answers["ping"] = ran(code=69, err="gate: runner_down")
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "programs", "dev", "gates"]}, 503, ["runner_down"])

    def test_it_is_no_pod_to_read(self):
        self.refused("GET", "/v1/pods/builds", None, 400, ["routes build.sh"])


MEMBER = "ad8cb74e" + "0" * 56
ZK_KINDS = "gates lint check lane lane-test test quick wasm shot sh\n"


class RoundRobin(unittest.TestCase):
    class Machine:
        def __init__(self, name, busy=()):
            self.name, self.busy, self.calls = name, list(busy), []

        def takes(self, words):
            return True

        def state(self):
            return {**UP, 'runner': self.name, 'busy': self.busy, 'heavy_runs': 1}

        def ran(self, command, alive=None):
            words = command[2:]
            self.calls.append(words)
            match words[0]:
                case 'ahead':
                    return ran(b'ahead heavy 0\n')
                case 'lanes':
                    return ran(b'heavy 3\n' if self.busy else b'heavy 0\n')
                case 'movable':
                    packed = base64.b64encode(b'programs\0dev\0gates\0').decode()
                    return ran(f'move queued heavy 3 {packed}\n'.encode() if self.busy else b'')
                case 'take':
                    return ran(b'run copied\n')
                case 'moved':
                    return ran(f'queued: moved to run {words[2]}\n'.encode())
                case _:
                    raise AssertionError(words)

    def machines(self, full=False):
        busy = [HEAVY] if full else []
        first, second = self.Machine('lat-a', busy), self.Machine('lat-b')
        service = server.Service(Kube(), Signer(), FETCHING, Stats(), Api(), [first, second])
        return service, first, second

    def test_each_runner_gets_its_turn_without_a_warm_pod(self):
        service, first, second = self.machines()
        chosen = [service.pick(ASKED_BY, ['programs', 'dev', 'gates'], None)[0] for _ in range(4)]
        self.assertEqual(chosen, [first.name, second.name, first.name, second.name])

    def test_a_full_runner_is_skipped_for_the_other_runner(self):
        service, first, second = self.machines(full=True)
        self.assertEqual(service.pick(ASKED_BY, ['programs', 'dev', 'gates'], None)[0], second.name)

    def test_a_runner_only_call_does_not_move_to_a_clone_that_holds_the_commit(self):
        service, first, second = self.machines()
        service.claim = lambda *args: None
        service.still_open = lambda *args: True
        service.reroute_after = lambda *args: None
        service.launched_on = lambda *args: ran(b'run picked\n')
        def holding(*args):
            raise AssertionError('runner-only routing must not ask another clone to take the call')
        service.holding = holding
        command = ['sh', BUILD, 'programs', 'a' * 40, 'gates']
        one = service.routed(ASKED_BY, command, None)
        two = service.routed(ASKED_BY, command, None)
        self.assertEqual((one.out, two.out), (b'run lat-a:picked\n', b'run lat-b:picked\n'))

    def test_when_every_runner_is_full_the_next_runner_holds_the_queue(self):
        service, first, second = self.machines(full=True)
        second.busy = [HEAVY]
        one = service.pick(ASKED_BY, ['programs', 'dev', 'gates'], None)
        two = service.pick(ASKED_BY, ['programs', 'dev', 'gates'], None)
        self.assertEqual((one, two), ((first.name, False), (second.name, False)))

    def test_reroute_moves_a_queued_run_between_runners_without_a_warm_pod(self):
        service, first, second = self.machines(full=True)
        said = service.rerouted(ASKED_BY, None)
        self.assertIn('queued: moved to run lat-b:copied', said)
        self.assertTrue(any(words[0] == 'take' for words in second.calls))
        self.assertIn(['moved', 'queued', 'lat-b:copied'], first.calls)


class Runners(Streamed):
    def setUp(self):
        held = tempfile.TemporaryDirectory()
        self.addCleanup(held.cleanup)
        root = pathlib.Path(held.name)
        self.gate = Gate()
        signer = Signer()
        made = []
        for name, files in (("desktop", {"hosts": "lan\n"}), ("zk-dev", {"forward": f"{MEMBER} builds-zk-dev\n", "kinds": ZK_KINDS})):
            keys = root / name
            keys.mkdir()
            for file, text in {"id_ed25519": "private", "known_hosts": f"{name} ssh-ed25519 AAAA", "user": "runner\n", **files}.items():
                (keys / file).write_text(text)
            made.append(server.Runner(name, keys, root / f"{name}-home", runner=self.gate.run, spawn=self.gate.spawn, signer=signer))
        self.serve(signer=signer, runners=made)
        self.service.rest = lambda name, pause: self.paused.append(pause)
        self.service.wall = lambda: RUNNER_STARTED
        self.service.warm_pod = "/shared/tools/warm-pod"
        self.signer.head = ran(files_answer(f"{WARM}\n".encode()))
        self.gate.answers["exec"] = ran(b"run r1\nread it with: build.sh status r1\n")
        self.gate.by_host["lan"] = {}
        self.gate.by_host["zk-dev"] = {"exec": ran(b"run z1\nread it with: build.sh status z1\n"), "ping": ran({**UP, "runner": "zk-dev"})}
        self.kube.answer("exec", ran(b"run w1\nread it with: build.sh status w1\n"))
        self.service.reroute_after = lambda asked: None
        self.service.claim = lambda asked, settings, words, alive: None

    def built(self, *words, request=41):
        return self.call("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, *words]}, headers=asked(request))

    def warm_execs(self):
        return [call[1:] for call in self.kube.calls if call[0] == "exec"]

    def on(self, host):
        return [words for called, words, _ in self.gate.calls if called == host]

    def aheads(self, desktop, zk, warm):
        self.gate.by_host["lan"]["ahead"] = ran(desktop)
        self.gate.by_host["zk-dev"]["ahead"] = ran(zk)
        self.kube.answer("exec", ran(warm), ran(b"run w1\n"))

    def test_zk_dev_is_reached_through_its_member_node_s_forward_signed_with_the_service_s_key(self):
        status, answer = self.call("GET", "/v1/pods/zk-dev")
        self.assertEqual((status, answer["pod"]["name"], answer["pod"]["runner"]["runner"]), (200, "zk-dev", "zk-dev"))
        host, words, argv = self.gate.calls[-1]
        self.assertEqual((host, words, argv[-2]), ("zk-dev", ["ping"], "runner@zk-dev"))
        self.assertIn(f"ProxyCommand=ducktape --workspace /home/pods --node https://node.example forward {MEMBER} builds-zk-dev", argv)
        self.assertEqual(self.gate.envs[-1], self.signer.env())
        self.assertEqual(self.call("GET", "/v1/pods/desktop")[0], 200)
        self.assertEqual(self.gate.calls[-1][2][-2], "runner@lan")
        self.assertFalse(any(option.startswith("ProxyCommand=") for option in self.gate.calls[-1][2]))
        self.assertIsNone(self.gate.envs[-1])

    def test_a_forward_runner_is_unreachable_while_the_service_holds_no_key_to_sign_with(self):
        self.signer.keyed = False
        self.refused("GET", "/v1/pods/zk-dev", None, 503, ["holds no key for pod zk-dev"])
        self.assertEqual(self.on("zk-dev"), [])

    def test_a_run_goes_to_the_runner_where_it_ends_first(self):
        self.aheads(b"ahead heavy 3 900\n", b"ahead heavy 1 300\n", b"ahead heavy 2 600\n")
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("JOB=j-1", "programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run zk-dev:z1"))
        self.assertTrue(answer["stderr"].startswith("builds: on zk-dev"))
        self.assertEqual(self.on("zk-dev")[-1], ["exec", "JOB=j-1", "programs", "dev", "gates"])
        said.assert_any_call("pods_routed", to="zk-dev", reason="sooner_end", detail=json.dumps(f"heavy: ends in 900s on pod desktop, 300s on pod zk-dev, 600s on pod {WARM}"))

    def test_without_every_finish_the_counts_decide_and_say_so(self):
        self.aheads(b"ahead heavy 3 900\n", b"ahead heavy 1 -\n", b"ahead heavy 2 600\n")
        with unittest.mock.patch.object(server, "say") as said:
            self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run zk-dev:z1")
        said.assert_any_call("pods_routed", to="zk-dev", reason="shorter_wait", detail=json.dumps(f"heavy: 3 ahead on pod desktop, 1 on pod zk-dev, 2 on pod {WARM}"))

    def test_an_equal_end_goes_to_the_runner_named_first(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 1 500\n", b"ahead heavy 1 500\n")
        self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")

    def test_an_idle_runner_takes_the_run_and_the_warm_pod_is_not_asked(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 0 99999\n", b"ahead heavy 0 1\n")
        self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run zk-dev:z1")
        self.assertEqual(self.warm_execs(), [])

    def test_a_kind_zk_dev_does_not_take_never_goes_there(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 0 1\n", b"ahead heavy 5 900\n")
        for request, words in enumerate((("programs", "dev", "pack", "chat"), ("programs", "dev", "sh", "make x"), ("apply", "--no-gate", "programs", "dev", "/home/x/e.sh")), start=60):
            self.kube.answer("exec", ran(b"ahead heavy 5 900\n"), ran(b"run w1\n"))
            self.assertEqual(self.built(*words, request=request)[1]["stdout"].splitlines()[0], "run desktop:r1", words)
        self.assertEqual(self.on("zk-dev"), [])
        self.assertEqual(self.built("MOVABLE=1", "programs", "dev", "sh", "make check", request=70)[1]["stdout"].splitlines()[0], "run zk-dev:z1")

    def test_a_kind_named_straight_at_zk_dev_is_refused_naming_its_kinds(self):
        self.refused("POST", "/v1/pods/zk-dev/exec", {"command": ["sh", BUILD, "programs", "dev", "pack", "chat"]}, 400, ["pod zk-dev takes only the run kinds its kinds file lists", "not build.sh pack"])
        self.assertEqual(self.on("zk-dev"), [])
        self.assertEqual(self.call("POST", "/v1/pods/zk-dev/exec", {"command": ["sh", BUILD, "status", "z1"]}, headers=asked(42))[0], 200)
        self.assertEqual(self.on("zk-dev"), [["exec", "status", "z1"]])

    def test_a_zk_dev_that_cannot_be_reached_is_passed_over_and_named(self):
        self.gate.down = {"zk-dev"}
        self.aheads(b"ahead heavy 1 500\n", b"ahead heavy 0 1\n", b"ahead heavy 2 900\n")
        with unittest.mock.patch.object(server, "say") as said:
            self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")
        said.assert_any_call("pods_routed", to="desktop", reason="runner_unready", runner="zk-dev", detail=unittest.mock.ANY)

    def test_a_runner_whose_heavy_runs_wait_on_its_pause_is_passed_over(self):
        self.aheads(b"ahead heavy 2 900\n", b"ahead heavy paused\n", b"ahead heavy 3 1200\n")
        with unittest.mock.patch.object(server, "say") as said:
            self.assertEqual(self.built("programs", "dev", "gates")[1]["stdout"].splitlines()[0], "run desktop:r1")
        said.assert_any_call("pods_routed", to="desktop", reason="runner_unasked", runner="zk-dev", detail=json.dumps("build.sh ahead on pod zk-dev says its heavy runs wait on its pause"))

    def test_a_bare_run_id_is_found_on_zk_dev(self):
        self.gate.by_host["lan"]["exec status"] = ran(code=2, err="build.sh: no run z1 in pod desktop")
        self.gate.by_host["zk-dev"]["exec status"] = ran(b"job j-1\nended: exit 0\n")
        status, answer = self.built("status", "z1")
        self.assertEqual((status, answer["stdout"]), (200, "job j-1\nended: exit 0\n"))
        self.assertTrue(answer["stderr"].startswith("builds: on zk-dev"))

    def test_a_held_run_whose_zk_dev_session_a_punch_cut_is_read_again_through_its_id(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 0 1\n", b"ahead heavy 2 900\n")
        self.gate.by_host["zk-dev"]["exec"] = ran(b"run z7\n")
        self.gate.sequence["exec wait"] = [ran(code=255, err="Connection to zk-dev closed by remote host."), ran(b"run z7: programs dev, gates\nGATES ok\n")]
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("WAIT=1", "programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"]), (200, "run zk-dev:z7\nrun z7: programs dev, gates\nGATES ok\n"))
        self.assertIn("then run z7 was read again through zk-dev:z7", answer["stderr"])
        self.assertEqual(self.on("zk-dev")[-3:], [["exec", "programs", "dev", "gates"], ["exec", "wait", "z7"], ["exec", "wait", "z7"]])
        self.assertEqual(self.warm_execs(), [])
        said.assert_any_call("pods_reread", run=json.dumps("zk-dev:z7"), verb="wait")
        self.assertEqual(self.service.launched, {("WAIT=1", "programs", "dev", "gates"): ("zk-dev", "z7")})

    def test_a_held_run_cut_again_is_read_again_while_its_request_waits(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 0 1\n", b"ahead heavy 2 900\n")
        self.gate.by_host["zk-dev"]["exec"] = ran(b"run z7\n")
        cut = ran(code=255, err="Connection to zk-dev closed by remote host.")
        self.gate.sequence["exec wait"] = [cut, cut, cut, ran(b"GATES ok\n")]
        status, answer = self.built("WAIT=1", "programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"]), (200, "run zk-dev:z7\nGATES ok\n"))
        self.assertEqual(self.paused, [1, 2])

    def test_a_held_run_whose_request_is_gone_is_not_read_again(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 0 1\n", b"ahead heavy 2 900\n")
        self.kube.answer("exec", ran(b"run w1\n"))
        self.gate.by_host["zk-dev"]["exec"] = ran(b"run z7\n")
        cut = ran(code=255, err="Connection to zk-dev closed by remote host.")
        self.gate.sequence["exec wait"] = [cut, cut, ran(b"GATES ok\n")]
        self.signer.request = [ran(OPEN), ran(GONE)]
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "WAIT=1", "programs", "dev", "gates"]}, 502, ["request 41, which asked for this, is gone, so it was not run on another machine, and run zk-dev:z7 may still run there"])
        self.assertEqual(self.on("zk-dev").count(["exec", "wait", "z7"]), 2)
        self.assertEqual(self.paused, [])
        self.assertEqual(self.warm_execs(), [])

    def test_a_run_whose_zk_dev_session_was_cut_after_it_started_is_read_again_and_not_run_elsewhere(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 0 1\n", b"ahead heavy 2 900\n")
        self.gate.sequence["exec programs"] = [ran(b"run z8\n", code=255, err="Connection to zk-dev closed by remote host.")]
        self.gate.by_host["zk-dev"]["exec status"] = ran(b"job j-1\nrunning: 1m\n", code=3)
        status, answer = self.built("programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run zk-dev:z8"))
        self.assertEqual(self.on("zk-dev")[-1], ["exec", "status", "z8"])
        self.assertEqual(self.warm_execs(), [])

    def test_a_cut_run_its_runner_cannot_answer_again_runs_again_on_the_warm_pod_when_it_only_reads(self):
        self.aheads(b"ahead heavy 2 500\n", b"ahead heavy 0 1\n", b"ahead heavy 2 900\n")
        self.kube.answer("exec", ran(b"run w1\n"))
        self.gate.by_host["zk-dev"]["exec"] = ran(b"run z7\n")
        self.gate.sequence["exec wait"] = [ran(code=255, err="Connection to zk-dev closed by remote host."), ran(code=69, err="gate: runner_down")]
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("WAIT=1", "programs", "dev", "gates")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, f"run {WARM}:w1"))
        said.assert_any_call("pods_routed", to="warm", reason="runner_lost", runner="zk-dev", detail=unittest.mock.ANY)

    def test_zk_dev_s_end_lines_post_under_its_name(self):
        self.service.warm_pod = None
        self.call("POST", "/v1/pods/zk-dev/exec", {"command": ["sh", BUILD, "JOB=j-1", "NOTIFY=j-1", "programs", "dev", "gates"]})
        words = self.gate.spawned.get(timeout=10)
        self.assertEqual(words[0], "logs")
        self.gate.followers.put(Followed(255, b""))
        self.gate.followers.put(Followed(0, b"", b"ducktape-notify j-1 z1 ended: exit 0, GATES ok\n"))
        self.gate.spawned.get(timeout=10)
        self.gate.spawned.get(timeout=10)
        line = "zk-dev \u00b7 z1 ended: exit 0, GATES ok"
        self.assertIn(("chat", notified("j-1", server.notify_id("zk-dev", "j-1", line), None, line, None)), self.signer.submits)
        self.assertEqual({host for host, words, _ in self.gate.calls if words[:1] == ["logs"]}, {"zk-dev"})

    def test_a_commit_only_zk_dev_holds_runs_there(self):
        made = "f1916715" + "0" * 32
        self.aheads(b"ahead heavy 0 1\n", b"ahead heavy 0 1\n", b"ahead heavy 0 1\n")
        self.gate.by_host["lan"]["exec holds"] = ran(f"lacks {made}\n".encode(), code=1)
        self.gate.by_host["zk-dev"]["exec holds"] = ran(f"holds {made}\n".encode())
        with unittest.mock.patch.object(server, "say") as said:
            status, answer = self.built("programs", made, "lane")
        self.assertEqual((status, answer["stdout"].splitlines()[0]), (200, "run zk-dev:z1"))
        self.assertEqual(self.warm_execs(), [])
        said.assert_any_call("pods_routed", to="zk-dev", reason="holds_commit", detail=json.dumps(f"programs {made} is in pod zk-dev's clone and not in pod desktop's"))

    def test_reroute_moves_each_run_to_the_runner_that_takes_it_and_waits_least(self):
        gates = base64.b64encode(b"JOB=j-1\0NOTIFY=\0programs\0dev\0gates\0").decode()
        diff = base64.b64encode(b"JOB=j-1\0NOTIFY=\0programs\0dev\0schema-diff\0chat\0").decode()
        self.kube.answer("exec", ran(f"move w1 heavy 3 {gates}\nmove w2 heavy 4 {diff}\n".encode()), ran(b"w1: moved to run zk-dev:z1\n"), ran(b"w2: moved to run desktop:r2\n"))
        self.gate.by_host["lan"].update({"exec lanes": ran(b"heavy 1\nlane 0\n"), "exec take": ran(b"run r2\n")})
        self.gate.by_host["zk-dev"].update({"exec lanes": ran(b"heavy 0\nlane 0\n"), "exec take": ran(b"run z1\n")})
        lines = self.built("reroute")[1]["stdout"].splitlines()
        self.assertEqual(lines, ["w1: moved to run zk-dev:z1", "w2: moved to run desktop:r2"])
        self.assertEqual([(host, words) for host, words, _ in self.gate.calls if words[1:2] == ["take"]], [("zk-dev", ["exec", "take", gates]), ("lan", ["exec", "take", diff])])

    def test_reroute_leaves_a_lane_a_runner_says_is_paused(self):
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"), ran(b"w1: moved to run desktop:r1\n"))
        self.gate.by_host["lan"].update({"exec lanes": ran(b"heavy 2\n"), "exec take": ran(b"run r1\n")})
        self.gate.by_host["zk-dev"]["exec lanes"] = ran(b"heavy paused\nlane 0\n")
        self.assertEqual(self.built("reroute")[1]["stdout"].splitlines(), ["w1: moved to run desktop:r1"])
        self.assertNotIn(["exec", "take", "d29yZHMx"], self.on("zk-dev"))

    def test_reroute_passes_over_a_runner_it_cannot_ask(self):
        self.gate.down = {"zk-dev"}
        self.kube.answer("exec", ran(b"move w1 heavy 3 d29yZHMx\n"), ran(b"w1: moved to run desktop:r1\n"))
        self.gate.by_host["lan"].update({"exec lanes": ran(b"heavy 0\n"), "exec take": ran(b"run r1\n")})
        self.assertEqual(self.built("reroute")[1]["stdout"].splitlines(), ["w1: moved to run desktop:r1"])

    def test_a_runner_is_never_deleted_and_builds_names_every_runner(self):
        self.refused("DELETE", "/v1/pods/zk-dev", None, 403, ["pod zk-dev is a build runner"])
        self.refused("GET", "/v1/pods/builds", None, 400, ["read pod desktop, pod zk-dev or the warm pod"])
        self.refused("POST", "/v1/pods/builds/exec", {"command": ["sh", BUILD, "busy"]}, 400, ["(pod desktop, pod zk-dev, or the warm pod)"])


class RunnerList(unittest.TestCase):
    def test_pods_runners_names_each_runner_and_its_keys_in_tie_order(self):
        runners = server.runners_of("desktop=/desktop \t zk-dev=/runners/zk-dev\n", None)
        self.assertEqual([(runner.name, runner.keys) for runner in runners], [("desktop", pathlib.Path("/desktop")), ("zk-dev", pathlib.Path("/runners/zk-dev"))])
        for wrong in ("desktop", "builds=/b", "desktop=/a desktop=/b", "zk-dev=relative", "Zk=/z", "-a=/a", "=/a"):
            with self.assertRaises(ValueError, msg=wrong):
                server.runners_of(wrong, None)

    def test_a_kinds_file_limits_only_the_calls_that_start_a_run(self):
        with tempfile.TemporaryDirectory() as held:
            keys = pathlib.Path(held)
            runner = server.Runner("zk-dev", keys)
            self.assertTrue(runner.takes(["programs", "dev", "pack"]))
            (keys / "kinds").write_text(ZK_KINDS)
            takes = {
                ("programs", "dev", "gates"): True,
                ("JOB=j-1", "programs", "dev", "lane"): True,
                ("programs", "dev", "pack"): False,
                ("programs", "dev", "schema-diff", "chat"): False,
                ("apply", "programs", "dev", "/x.sh"): False,
                ("programs", "dev", "sh", "make"): False,
                ("MOVABLE=1", "programs", "dev", "sh", "make"): True,
                ("status", "z1"): True,
                ("busy",): True,
                ("ahead", "programs", "dev", "pack"): True,
                ("stop", "z1", "z2"): True,
            }
            for words, expected in takes.items():
                self.assertEqual(runner.takes(list(words)), expected, words)


def unblock(place):
    for name, flags in (("started", os.O_RDONLY), ("watching", os.O_RDONLY), ("release", os.O_WRONLY), ("hold", os.O_WRONLY)):
        try:
            held = os.open(place / name, flags | os.O_NONBLOCK)
        except OSError:
            continue
        if flags == os.O_WRONLY:
            os.write(held, b"go\n")
        os.close(held)


FAKE_KUBECTL = """#!/usr/bin/env python3
import os, pathlib, sys
place = pathlib.Path(os.environ["FAKE_KUBE"])
words = sys.argv[1:]
if "exec" in words:
    (place / "started").write_text("exec\\n")
    print("built", (place / "release").read_text().strip())
elif "pod" in words and "--watch" in words:
    (place / "watching").write_text("watch\\n")
    (place / "hold").read_text()
elif "pod" in words:
    print(POD)
else:
    sys.exit(1)
"""


STOPS_WITHIN = int(re.search(r"^default_wait=(\d+)$", (HERE / "deploy").read_text(), re.M).group(1)) * NODE_BLOCK_MS / 1000


class Heard:
    def __init__(self, process):
        self.process = process
        self.lines = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        with self.process.stderr:
            for line in self.process.stderr:
                self.lines.put(line)
        self.lines.put("")

    def next(self, until):
        try:
            return self.lines.get(timeout=None if until is None else max(0.0, until - time.monotonic()))
        except queue.Empty:
            return None

    def line(self, word, until=None):
        while True:
            line = self.next(until)
            if line is None:
                raise AssertionError(f"the service wrote no {word} line in the {STOPS_WITHIN:g} s after its SIGTERM that Kubernetes gives it past its preStop before it kills it")
            if not line:
                raise AssertionError(f"the service's stderr ended before any {word} line")
            if word in line:
                return line

    def rest(self, until):
        said = []
        while True:
            line = self.next(until)
            if line is None:
                raise AssertionError(f"the service still ran {STOPS_WITHIN:g} s after its SIGTERM, when Kubernetes would kill it, having said since: {''.join(said)[-4000:]}")
            if not line:
                return "".join(said)
            said.append(line)


def terminated(process):
    process.send_signal(signal.SIGTERM)
    return time.monotonic() + STOPS_WITHIN


class Stopped(unittest.TestCase):
    def node(self):
        served = ThreadingHTTPServer(("127.0.0.1", 0), Node)
        threading.Thread(target=served.serve_forever, daemon=True).start()
        self.addCleanup(served.server_close)
        self.addCleanup(served.shutdown)
        return f"http://127.0.0.1:{served.server_address[1]}"

    def test_a_node_whose_block_time_is_not_read_stops_the_service_at_start(self):
        with tempfile.TemporaryDirectory() as keys:
            unread = self.node() + "/nothing"
            env = dict(os.environ, PODS_OUT_IMAGE="pods:test", PODS_NODE=unread, PODS_FORGE="duck://chain/forge", PODS_KUBECTL="false", PODS_LISTEN="127.0.0.1:0", PODS_KEYS=keys)
            done = subprocess.run([sys.executable, str(HERE / "server.py")], env=env, capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0)
            self.assertIn("block time was not read", done.stderr)

    def test_a_sigterm_ends_the_service_at_once(self):
        with tempfile.TemporaryDirectory() as keys:
            env = dict(os.environ, PODS_OUT_IMAGE="pods:test", PODS_NODE=self.node(), PODS_FORGE="duck://chain/forge", PODS_KUBECTL="false", PODS_LISTEN="127.0.0.1:0", PODS_KEYS=keys)
            process = subprocess.Popen([sys.executable, str(HERE / "server.py")], env=env, stderr=subprocess.PIPE, text=True)
            self.addCleanup(process.kill)
            heard = Heard(process)
            self.assertIn("namespace=agents", heard.line("event=pods_serving"))
            said = heard.rest(terminated(process))
            self.assertEqual(process.wait(), 0, said)
            self.assertIn("event=pods_stopping signal=SIGTERM", said)

    def test_a_sigterm_with_requests_in_flight_ends_the_service_at_once(self):
        with tempfile.TemporaryDirectory() as keys:
            env = dict(os.environ, PODS_OUT_IMAGE="pods:test", PODS_NODE=self.node(), PODS_FORGE="duck://chain/forge", PODS_KUBECTL="false", PODS_LISTEN="127.0.0.1:0", PODS_KEYS=keys)
            process = subprocess.Popen([sys.executable, str(HERE / "server.py")], env=env, stderr=subprocess.PIPE, text=True)
            self.addCleanup(process.kill)
            heard = Heard(process)
            serving = heard.line("event=pods_serving")
            port = int(serving.split("listen=127.0.0.1:")[1].split()[0])
            stopped = threading.Event()

            def asking():
                while not stopped.is_set():
                    try:
                        link = http.client.HTTPConnection("127.0.0.1", port)
                        link.request("GET", "/")
                        link.getresponse().read()
                        link.close()
                    except (OSError, http.client.HTTPException):
                        return

            clients = [threading.Thread(target=asking) for _ in range(os.cpu_count())]
            for client in clients:
                client.start()
            for _ in range(len(clients)):
                heard.line("event=pods_request")
            try:
                said = heard.rest(terminated(process))
            finally:
                stopped.set()
                process.kill()
                for client in clients:
                    client.join()
            self.assertEqual(process.wait(), 0, said)
            self.assertIn("event=pods_stopping signal=SIGTERM", said)

    def test_a_sigterm_cuts_streams_at_once_and_answers_whole_calls_before_the_service_ends(self):
        with tempfile.TemporaryDirectory() as temp:
            place = pathlib.Path(temp)
            for name in ("started", "release", "watching", "hold"):
                os.mkfifo(place / name)
            kubectl = place / "kubectl"
            kubectl.write_text(FAKE_KUBECTL.replace("POD", repr(json.dumps(pod_json(state=RUNNING, phase="Running")))))
            kubectl.chmod(0o755)
            env = dict(os.environ, PODS_OUT_IMAGE="pods:test", PODS_NODE=self.node(), PODS_FORGE="duck://chain/forge", PODS_KUBECTL=str(kubectl), PODS_LISTEN="127.0.0.1:0", PODS_KEYS=temp, FAKE_KUBE=temp)
            process = subprocess.Popen([sys.executable, str(HERE / "server.py")], env=env, stderr=subprocess.PIPE, text=True)
            self.addCleanup(process.kill)
            try:
                self.drained(process, place)
            finally:
                unblock(place)

    def drained(self, process, place):
        heard = Heard(process)
        serving = heard.line("event=pods_serving")
        port = int(serving.split("listen=127.0.0.1:")[1].split()[0])
        waiting = http.client.HTTPConnection("127.0.0.1", port)
        waiting.request("GET", "/v1/pods/duckpond-41/wait?every=3600", headers=ASKED)
        stream = waiting.getresponse()
        self.assertEqual(stream.status, 200)
        (place / "watching").read_text()
        answers = []

        def call():
            link = http.client.HTTPConnection("127.0.0.1", port)
            link.request("POST", "/v1/pods/duckpond-41/exec", body=json.dumps({"command": ["make"]}), headers=ASKED)
            answered = link.getresponse()
            answers.append((answered.status, json.loads(answered.read())))

        caller = threading.Thread(target=call)
        caller.start()
        (place / "started").read_text()
        until = terminated(process)
        heard.line("event=pods_stopping", until)
        with self.assertRaises(http.client.IncompleteRead):
            stream.read()
        with self.assertRaises(ConnectionRefusedError):
            socket.create_connection(("127.0.0.1", port))
        (place / "release").write_text("once\n")
        caller.join()
        said = heard.rest(until)
        self.assertEqual(process.wait(), 0, said)
        self.assertEqual(answers, [(200, {"exit_code": 0, "stdout": "built once\n", "stderr": ""})])
        self.assertIn("event=pods_drained", said)

    def test_a_sigterm_heard_inside_a_finalizer_or_a_line_being_written_still_stops_the_service_and_writes_nothing(self):
        self.addCleanup(signal.signal, signal.SIGTERM, signal.getsignal(signal.SIGTERM))
        self.addCleanup(signal.set_wakeup_fd, signal.set_wakeup_fd(-1))
        signals = server.Signals(signal.SIGTERM)
        self.addCleanup(signals.said.close)
        self.addCleanup(signals.heard.close)

        class Dropped:
            def __del__(self):
                signal.raise_signal(signal.SIGTERM)

        written = io.StringIO()
        with contextlib.redirect_stderr(written):
            print("event=pods_serving", end="", file=sys.stderr)
            Dropped()
        self.assertEqual(written.getvalue(), "event=pods_serving")
        self.assertEqual(signals.wait(), "SIGTERM")


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


def cluster_pod(namespace, phase, containers, level=None, name="p"):
    spec = {"containers": [{"name": f"c{at}", **({"resources": {"requests": asks}} if asks is not None else {})} for at, asks in enumerate(containers)]}
    if level is not None:
        spec["resources"] = {"requests": level}
    return {"metadata": {"namespace": namespace, "name": name}, "spec": spec, "status": {"phase": phase}}


def hardcap_quota(cpu, memory, hard_cpu="56", hard_memory="121Gi"):
    return {"metadata": {"name": "hardcap", "namespace": "agents"}, "status": {"hard": {"requests.cpu": hard_cpu, "requests.memory": hard_memory}, "used": {"requests.cpu": cpu, "requests.memory": memory, "requests.storage": "0"}}}


FABRICS = {"items": [{"status": {"allocatable": {"cpu": "56", "memory": "127142520Ki", "pods": "110"}}}]}
CLUSTER = {
    "items": [
        cluster_pod("duckpond", "Running", [{"cpu": "12", "memory": "6Gi"}, {"cpu": "312m", "memory": "500Mi"}]),
        cluster_pod("duckpond", "Pending", [{"cpu": "9", "memory": "9Gi"}], level={"cpu": "4", "memory": "1Gi"}),
        cluster_pod("kube-system", "Running", [{"cpu": "7", "memory": "312Mi"}], level={"cpu": "1"}),
        cluster_pod("pods", "Running", [None, {}]),
        cluster_pod("duckpond", "Succeeded", [{"cpu": "20", "memory": "20Gi"}]),
        cluster_pod("duckpond", "Failed", [{"cpu": "20", "memory": "20Gi"}]),
        cluster_pod("agents", "Running", [{"cpu": "16500m", "memory": "109Gi"}], name="duckpond-warm"),
        cluster_pod("agents", "Pending", [{"cpu": "1", "memory": "1Gi"}], name="duckpond-370000"),
    ]
}
CROWDED = {"items": [*CLUSTER["items"], cluster_pod("agents", "Pending", [{"cpu": "16", "memory": "10Gi"}], name="duckpond-374148"), cluster_pod("agents", "Succeeded", [{"memory": "50Gi"}], name="duckpond-360000")]}


class Hardcap(unittest.TestCase):
    def test_every_running_or_pending_pod_outside_agents_comes_off_the_allocatable(self):
        self.assertEqual(server.hardcap(FABRICS, CLUSTER, "agents"), ("38688m", "113Gi"))

    def test_a_pod_level_request_stands_for_its_containers_one_resource_at_a_time(self):
        pod = cluster_pod("duckpond", "Running", [{"cpu": "7", "memory": "312Mi"}], level={"cpu": "1"})
        self.assertEqual(server.pod_requests(pod), {"cpu": 1, "memory": 312 * 1024 * 1024})

    def hardcap_ran(self, pods, quota, refused=None):
        with tempfile.TemporaryDirectory() as temp:
            folder = pathlib.Path(temp)
            (folder / "nodes.json").write_text(json.dumps(FABRICS))
            (folder / "pods.json").write_text(json.dumps(pods))
            if quota is not None:
                (folder / "quota.json").write_text(json.dumps(quota))
            if refused is not None:
                (folder / "quota.err").write_text(refused)
            fake = folder / "kubectl.py"
            fake.write_text(
                "import os, sys\n"
                "folder, *asked = sys.argv[1:]\n"
                "known = {'get nodes -o json': 'nodes', 'get pods --all-namespaces -o json': 'pods', 'get resourcequota hardcap --namespace agents -o json': 'quota'}\n"
                "path = f'{folder}/{known[\" \".join(asked)]}.json'\n"
                "if os.path.exists(path[:-5] + '.err'):\n"
                "    sys.exit(open(path[:-5] + '.err').read())\n"
                "if not os.path.exists(path):\n"
                "    sys.exit('Error from server (NotFound): resourcequotas \"hardcap\" not found')\n"
                "sys.stdout.write(open(path).read())\n"
            )
            kubectl = " ".join(shlex.quote(word) for word in (sys.executable, str(fake), temp))
            return subprocess.run([sys.executable, str(HERE / "server.py"), "hardcap"], env={**os.environ, "PODS_KUBECTL": kubectl}, capture_output=True, text=True)

    def test_server_py_hardcap_prints_what_kubectl_lists(self):
        done = self.hardcap_ran(CLUSTER, hardcap_quota("17500m", "110Gi"))
        self.assertEqual((done.returncode, done.stdout, done.stderr), (0, "38688m 113Gi\n", ""))

    def test_a_first_deploy_with_no_hardcap_yet_prints_the_cap(self):
        done = self.hardcap_ran(CLUSTER, None)
        self.assertEqual((done.returncode, done.stdout, done.stderr), (0, "38688m 113Gi\n", ""))

    def test_a_cap_equal_to_what_agents_holds_is_printed(self):
        done = self.hardcap_ran(CROWDED, hardcap_quota("38688m", "113Gi"))
        self.assertEqual((done.returncode, done.stdout, done.stderr), (0, "38688m 113Gi\n", ""))

    def test_a_cap_below_what_agents_holds_is_refused_naming_the_pods_holding_it(self):
        done = self.hardcap_ran(CROWDED, hardcap_quota("33500m", "120Gi"))
        said = (
            "hardcap's requests.memory would drop to 113Gi, below the 120Gi that pods in agents hold now (its status.used), so deploy applies nothing; "
            "those pods hold, largest first: duckpond-warm (Running) 109Gi, duckpond-374148 (Pending) 10Gi, duckpond-370000 (Pending) 1Gi. "
            "Deploy again once enough of them end.\n"
        )
        self.assertEqual((done.returncode, done.stdout, done.stderr), (1, "", said))

    def test_a_cpu_cap_below_what_agents_holds_is_refused_in_cpu(self):
        done = self.hardcap_ran(CROWDED, hardcap_quota("40", "110Gi"))
        said = (
            "hardcap's requests.cpu would drop to 38688m, below the 40 that pods in agents hold now (its status.used), so deploy applies nothing; "
            "those pods hold, largest first: duckpond-warm (Running) 16500m, duckpond-374148 (Pending) 16, duckpond-370000 (Pending) 1. "
            "Deploy again once enough of them end.\n"
        )
        self.assertEqual((done.returncode, done.stdout, done.stderr), (1, "", said))

    def test_a_cap_that_raises_or_keeps_a_hard_already_below_what_agents_holds_is_printed(self):
        for hard in ("100Gi", "113Gi"):
            done = self.hardcap_ran(CROWDED, hardcap_quota("33500m", "120Gi", hard_memory=hard))
            self.assertEqual((hard, done.returncode, done.stdout, done.stderr), (hard, 0, "38688m 113Gi\n", ""))

    def test_a_quota_read_that_fails_otherwise_than_not_found_stops_the_deploy(self):
        refused = 'Error from server (Forbidden): resourcequotas "hardcap" is forbidden'
        done = self.hardcap_ran(CLUSTER, hardcap_quota("17500m", "110Gi"), refused=refused)
        said = f"hardcap in agents was not read, so what its pods hold is unknown and deploy applies nothing: {refused}\n"
        self.assertEqual((done.returncode, done.stdout, done.stderr), (1, "", said))


class Manifest(unittest.TestCase):
    def env(self):
        text = (HERE / "pods.yaml").read_text()
        return dict(re.findall(r'- \{name: (PODS_\w+), value: "?([^"}]*)"?\}', text))

    def documents(self, name):
        text = (HERE / "pods.yaml").read_text()
        return [document for document in text.split("\n---\n") if f"\n  name: {name}\n" in document]

    def runner_paths(self):
        return dict(word.split("=", 1) for word in self.env()["PODS_RUNNERS"].split())

    def secret_mounts(self):
        deployment, = [document for document in self.documents("pods") if "\nkind: Deployment\n" in document]
        mounts = re.search(r"\n          volumeMounts:\n((?:            .*\n)*)", deployment).group(1)
        fields = [dict(field.split(": ", 1) for field in line.strip()[3:-1].split(", ")) for line in mounts.splitlines()]
        volumes = re.search(r"\n      volumes:\n((?:        .*(?:\n|$))*)", deployment).group(1)
        secrets = dict(re.findall(r"^        - name: (\S+)\n          secret:\n(?:            .*\n)*?            secretName: (\S+)", volumes, re.M))
        return {mount["mountPath"]: (mount.get("readOnly"), secrets.get(mount["name"])) for mount in fields}

    def objects(self):
        text = (HERE / "pods.yaml").read_text()
        found = set()
        for document in text.split("\n---\n"):
            kind = re.search(r"^kind: (\S+)$", document, re.M).group(1)
            name = re.search(r"^  name: (\S+)$", document, re.M).group(1)
            namespace = re.search(r"^  namespace: (\S+)$", document, re.M)
            found.add((kind, namespace and namespace.group(1), name))
        return found

    def test_the_builds_namespace_and_the_latitude_build_classes_are_written_down(self):
        objects = self.objects()
        for wanted in [
            ("Namespace", None, "builds"),
            ("PriorityClass", None, "builds"),
            ("PriorityClass", None, "latitude-builds"),
            ("Role", "builds", "pods-builds"),
            ("RoleBinding", "builds", "pods-builds"),
            ("ValidatingAdmissionPolicy", None, "builds-pods"),
            ("ValidatingAdmissionPolicyBinding", None, "builds-pods"),
            ("MutatingAdmissionPolicy", None, "latitude-builds-ci"),
            ("MutatingAdmissionPolicyBinding", None, "latitude-builds-ci"),
            ("NetworkPolicy", "builds", "isolated"),
            ("NetworkPolicy", "builds", "to-duckpond-node"),
        ]:
            with self.subTest(object=wanted):
                self.assertIn(wanted, objects)

    def test_every_build_runner_reads_its_keys_from_a_secret_mounted_read_only_at_its_path(self):
        mounts = self.secret_mounts()
        for name, path in self.runner_paths().items():
            with self.subTest(runner=name):
                read_only, secret = mounts.get(path, (None, None))
                self.assertEqual(read_only, "true")
                self.assertIsNotNone(secret)

    def test_every_secret_mounted_besides_the_signing_key_is_a_build_runner_s_keys(self):
        mounted = {path for path, (_, secret) in self.secret_mounts().items() if secret is not None}
        self.assertEqual(sorted(mounted - {self.env()["PODS_KEYS"]} - set(self.runner_paths().values())), [])

    def test_its_own_calls_reach_the_node_inside_the_cluster(self):
        self.assertEqual(self.env()["PODS_DUCKTAPE"], "ducktape --workspace /home/pods --node http://node-http.duckpond.svc:8844")

    def test_agent_pods_are_handed_the_node_inside_the_cluster(self):
        self.assertEqual(self.env()["PODS_NODE"], "http://node-http.duckpond.svc:8844")

    def test_agent_pods_may_open_only_the_node_port_inside_the_cluster(self):
        policy = "\n".join([
            "apiVersion: networking.k8s.io/v1",
            "kind: NetworkPolicy",
            "metadata:",
            "  name: to-duckpond-node",
            "  namespace: agents",
            "spec:",
            "  podSelector: {}",
            "  policyTypes: [Egress]",
            "  egress:",
            "    - to:",
            "        - namespaceSelector:",
            "            matchLabels: {kubernetes.io/metadata.name: duckpond}",
            "          podSelector:",
            "            matchLabels: {app: node}",
            "      ports:",
            "        - {protocol: TCP, port: 8844}",
        ])
        self.assertEqual([document for document in self.documents("to-duckpond-node") if "\n  namespace: agents\n" in document], [policy])

    def test_build_pods_fit_four_to_a_lat_node(self):
        env = self.env()
        self.assertTrue(env["PODS_BUILD_IMAGE"].startswith("localhost:5000/toolchain"))
        self.assertEqual((env["PODS_BUILD_CPU"], env["PODS_BUILD_MEMORY"]), ("7", "28Gi"))

    def test_the_service_keeps_build_runs_apart_from_agents_runs(self):
        self.assertIn("resourceNames: [runs, build-runs]", (HERE / "pods.yaml").read_text())


class Lifecycle(unittest.TestCase):
    node = Stopped.node

    def test_sigterm_kills_and_reaps_the_background_kubectl_watch(self):
        with tempfile.TemporaryDirectory() as temp:
            place = pathlib.Path(temp)
            os.mkfifo(place / "background")
            os.mkfifo(place / "hold")
            kubectl = place / "kubectl"
            kubectl.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
place = pathlib.Path(os.environ["FAKE_KUBE"])
words = sys.argv[1:]
if "configmap" in words:
    print(json.dumps({"data": {}}))
elif "pods" in words and "--watch" in words:
    (place / "background").write_text(str(os.getpid()))
    (place / "hold").read_text()
else:
    print(json.dumps({"items": []}))
""")
            kubectl.chmod(0o755)
            env = dict(os.environ, PODS_OUT_IMAGE="pods:test", PODS_NODE=self.node(), PODS_FORGE="duck://chain/forge", PODS_KUBECTL=str(kubectl), PODS_LISTEN="127.0.0.1:0", PODS_KEYS=temp, PODS_RUNNERS="", FAKE_KUBE=temp)
            process = subprocess.Popen([sys.executable, str(HERE / "server.py")], env=env, stderr=subprocess.PIPE, text=True)
            heard = Heard(process)
            child = None
            try:
                heard.line("event=pods_serving")
                child = int((place / "background").read_text())
                process.send_signal(signal.SIGTERM)
                said = heard.rest(None)
                self.assertEqual(process.wait(), 0, said)
                with self.assertRaises(ProcessLookupError):
                    os.kill(child, 0)
                self.assertIn("event=pods_background_stopped", said)
                self.assertIn("event=pods_drained", said)
            finally:
                process.kill()
                process.wait()
                if child is not None:
                    try:
                        os.kill(child, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_sigterm_stops_a_reroute_reading_a_runner_while_an_exec_drains(self):
        with tempfile.TemporaryDirectory() as temp:
            place = pathlib.Path(temp)
            keys = place / "keys"
            keys.mkdir()
            for name, text in {"id_ed25519": "private", "known_hosts": "lat-1 ssh-ed25519 AAAA", "hosts": "lat-1\n", "user": "tech\n"}.items():
                (keys / name).write_text(text)
            entered, stopped = place / "entered", place / "stopped"
            os.mkfifo(entered)
            reading = "import os, pathlib, signal, sys\nif pathlib.Path(sys.argv[2]).exists():\n    sys.exit(0)\npathlib.Path(sys.argv[1]).write_text(str(os.getpid()))\nsignal.pause()\n"
            runner = server.Runner("lat-1", keys)
            runner.argv = lambda host, words, alive: [sys.executable, "-c", reading, str(entered), str(stopped)]
            service = server.Service(Kube(), Signer(), FETCHING, Stats(), Api(), [runner])
            started = threading.Event()
            release = threading.Event()

            def execute(*args, **kwargs):
                started.set()
                release.wait()
                return ran(b"finished")

            service.executed = execute
            execution = service.execution({"chain": "duckpond", "program": "harness", "request": "485269"}, "duckpond-41", None, ["make"])
            started.wait()
            workers = []
            start = service.background.start

            def starting(target, *args):
                thread = start(target, *args)
                workers.append(thread)
                return thread

            service.background.start = starting
            service.reroute_after({"chain": "duckpond", "program": "harness", "request": "485539"})
            read = int(entered.read_text())
            join = workers[0].join
            events = []

            def joining(*args, **kwargs):
                try:
                    with self.assertRaises(ProcessLookupError, msg="shutdown joined the reroute while its runner read was still running"):
                        os.kill(read, 0)
                finally:
                    stopped.touch()
                    try:
                        os.kill(read, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    join(*args, **kwargs)

            try:
                with unittest.mock.patch.object(workers[0], "join", side_effect=joining), unittest.mock.patch.object(server, "say", side_effect=lambda event, **fields: events.append(event)):
                    service.background.stopping.set()
                    service.stop_background()
                self.assertFalse(workers[0].is_alive())
                self.assertNotIn("pods_rerouted", events, "the reroute read its next runner after shutdown")
                self.assertNotIn("pods_reroute_failed", events)
                self.assertEqual(service.draining_execs(), [{"pod": "duckpond-41", "chain": "duckpond", "program": "harness", "request": "485269"}])
            finally:
                stopped.touch()
                release.set()
                try:
                    os.kill(read, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            service.drain_execs()
            self.assertEqual(execution.get().out, b"finished")

    def test_runner_follow_does_not_reopen_after_shutdown_wakes_its_rest(self):
        service = server.Service(Kube(), Signer(), FETCHING, Stats(), Api(), [])
        service.runners = {"runner": object()}
        service.wanted = {"runner": threading.Event()}
        resting = threading.Event()
        attempts = []

        class Reopened(BaseException):
            pass

        def tail(since):
            attempts.append(since)
            if len(attempts) > 1:
                raise Reopened()
            raise server.Refused(503, "unreachable")

        def rest(name, pause):
            resting.set()
            service.wanted[name].wait()

        def follow():
            try:
                service.followed("runner", 0, 0)
            except Reopened:
                pass

        service.runners["runner"] = type("Runner", (), {"tail": staticmethod(tail)})()
        service.runner_began = lambda *args: (0, True)
        service.caught_up = lambda *args: None
        service.rest = rest
        thread = service.background.start(follow)
        resting.wait()
        service.stop_background()
        self.assertFalse(thread.is_alive())
        self.assertEqual(len(attempts), 1, "shutdown woke the reader only for it to reopen the runner")

    def test_background_shutdown_names_the_worker_and_its_wait_stack(self):
        background = server.Background()
        entered = threading.Event()
        release = threading.Event()
        events = []

        def waiting():
            entered.set()
            release.wait()

        def said(event, **fields):
            events.append((event, fields))
            if event == "pods_background_waiting":
                release.set()

        thread = background.start(waiting)
        entered.wait()
        try:
            with unittest.mock.patch.object(server, "say", side_effect=said):
                background.stop()
        finally:
            release.set()
            thread.join()
        self.assertEqual([event for event, fields in events], ["pods_background_waiting", "pods_background_joined"])
        waiting_fields = events[0][1]
        self.assertEqual(json.loads(waiting_fields["worker"]), waiting.__qualname__)
        self.assertEqual(waiting_fields["ident"], thread.ident)
        self.assertTrue(any(":waiting" in frame for frame in json.loads(waiting_fields["stack"])))

    def test_background_stop_wakes_a_retry_and_refuses_new_workers(self):
        background = server.Background()
        entered = threading.Event()
        continued = threading.Event()

        def retry():
            entered.set()
            background.pause(None)
            continued.set()

        thread = background.start(retry)
        entered.wait()
        background.stop()
        self.assertFalse(thread.is_alive())
        self.assertFalse(continued.is_set())
        self.assertIsNone(background.start(continued.set))
        with self.assertRaises(server.BackgroundStopped):
            background.popen([sys.executable, "-c", "pass"])
        self.assertFalse(continued.is_set())

    def test_background_cancellation_does_not_cancel_a_foreground_retry(self):
        background = server.Background()
        background.stopping.set()
        delay = FETCHING.block_ms / 1000
        with unittest.mock.patch.object(server.time, "sleep") as pause:
            background.pause(delay)
        pause.assert_called_once_with(delay)

    def test_an_exec_with_no_http_reader_still_drains(self):
        service = server.Service(Kube(), Signer(), FETCHING, Stats(), Api(), [])
        started = threading.Event()
        release = threading.Event()
        waiting = threading.Event()
        drained = threading.Event()

        def execute(*args, **kwargs):
            started.set()
            release.wait()
            return ran(b"finished")

        service.executed = execute
        execution = service.execution({"chain": "duckpond", "program": "harness", "request": "41"}, "duckpond-41", None, ["make"])
        started.wait()
        wait_for = service.executing.wait_for

        def checked(predicate):
            self.assertFalse(predicate())
            waiting.set()
            return wait_for(predicate)

        service.executing.wait_for = checked

        def drain():
            service.drain_execs()
            drained.set()

        thread = threading.Thread(target=drain)
        thread.start()
        try:
            waiting.wait()
            self.assertFalse(drained.is_set())
            self.assertEqual(service.draining_execs(), [{"pod": "duckpond-41", "chain": "duckpond", "program": "harness", "request": "41"}])
        finally:
            release.set()
            thread.join()
        self.assertEqual(execution.get().out, b"finished")
        self.assertTrue(drained.is_set())
        self.assertEqual(service.draining_execs(), [])

    def test_connections_admitted_after_drain_begins_do_not_start_calls(self):
        drain = server.Drain()
        left, right = socket.socketpair()
        try:
            self.assertEqual(drain.begin(), 0)
            drain.opened(left)
            self.assertEqual(right.recv(1), b"")
            self.assertFalse(drain.called(left))
            self.assertEqual(drain.whole, 0)
            drain.wait()
        finally:
            left.close()
            right.close()

    def test_sigterm_preserves_an_exec_stream_until_its_command_answers(self):
        with tempfile.TemporaryDirectory() as temp:
            place = pathlib.Path(temp)
            for name in ("started", "release", "watching", "hold"):
                os.mkfifo(place / name)
            kubectl = place / "kubectl"
            kubectl.write_text(FAKE_KUBECTL.replace("POD", repr(json.dumps(pod_json(state=RUNNING, phase="Running")))))
            kubectl.chmod(0o755)
            env = dict(os.environ, PODS_OUT_IMAGE="pods:test", PODS_NODE=self.node(), PODS_FORGE="duck://chain/forge", PODS_KUBECTL=str(kubectl), PODS_LISTEN="127.0.0.1:0", PODS_KEYS=temp, PODS_RUNNERS="", FAKE_KUBE=temp)
            process = subprocess.Popen([sys.executable, str(HERE / "server.py")], env=env, stderr=subprocess.PIPE, text=True)
            heard = Heard(process)
            connection = None
            try:
                serving = heard.line("event=pods_serving")
                port = int(serving.split("listen=127.0.0.1:")[1].split()[0])
                connection = http.client.HTTPConnection("127.0.0.1", port)
                connection.request("POST", "/v1/pods/duckpond-41/exec", body=json.dumps({"command": ["make"], "every": 1}), headers=ASKED)
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                (place / "started").read_text()
                process.send_signal(signal.SIGTERM)
                heard.line("event=pods_stopping")
                (place / "release").write_text("once\n")
                events = response.read().decode()
                self.assertIn('event: ended', events)
                self.assertIn('"stdout": "built once\\n"', events)
                said = heard.rest(None)
                self.assertEqual(process.wait(), 0, said)
                self.assertIn("event=pods_drained", said)
            finally:
                if connection is not None:
                    connection.close()
                process.kill()
                process.wait()
                unblock(place)


BUILD_POD_IMAGE = "localhost:5000/toolchain@sha256:00"
BUILT = "a" * 40


class BuildPods(Served):
    def setUp(self):
        self.builds = Kube()
        self.builds.namespace = "builds"
        self.serve(building=server.Building(self.builds, BUILD_POD_IMAGE, "7", "28Gi"))
        self.signer.sender = sent_by(17)

    def built(self):
        return [call[1] for call in self.builds.calls if call[0] == "create"][0]

    def asked(self, **fields):
        return {"repo": "programs", "commit": BUILT, "base": "dev", **fields}

    def test_a_build_is_one_pod_named_by_its_request_and_answered_at_create(self):
        self.builds.answer("create", ran(pod_json()))
        status, answer = self.call("POST", "/v1/builds", self.asked(gates="check", notify={"channel": "j-532"}))
        self.assertEqual(status, 201, answer)
        self.assertEqual(answer["build"]["name"], "duckpond-41")
        pod = self.built()
        self.assertEqual((pod["kind"], pod["metadata"]["name"], pod["metadata"]["namespace"]), ("Pod", "duckpond-41", "builds"))
        self.assertEqual(pod["metadata"]["labels"]["ducktape.industries/requester"], "17")
        self.assertEqual(pod["metadata"]["labels"]["ducktape.industries/request"], "41")
        self.assertEqual(pod["metadata"]["labels"]["app.kubernetes.io/managed-by"], "pods")
        self.assertEqual(pod["metadata"]["annotations"]["ducktape.industries/notify-channel"], "j-532")
        self.assertNotIn("create", self.kube.verbs())

    def test_a_build_pod_holds_the_lat_contract(self):
        self.call("POST", "/v1/builds", self.asked(gates="check"))
        body = self.built()["spec"]
        self.assertEqual(body["restartPolicy"], "Never")
        self.assertEqual(body["activeDeadlineSeconds"], 2400)
        self.assertIs(body["hostUsers"], False)
        self.assertIs(body["automountServiceAccountToken"], False)
        self.assertIs(body["enableServiceLinks"], False)
        self.assertEqual(body["priorityClassName"], "builds")
        self.assertEqual(body["nodeSelector"], {"builds.ducktape/pods": "true"})
        self.assertEqual(body["tolerations"], [{"key": "ducktape.industries/builds", "operator": "Equal", "value": "latitude", "effect": "NoSchedule"}])
        self.assertEqual(body["securityContext"], {"runAsNonRoot": True, "runAsUser": 10001, "runAsGroup": 10001, "seccompProfile": {"type": "RuntimeDefault"}})
        for hostless in ("hostNetwork", "hostPID", "hostIPC", "serviceAccountName"):
            self.assertNotIn(hostless, body)
        (fetch,) = body["initContainers"]
        (run,) = body["containers"]
        for container in (fetch, run):
            self.assertEqual(container["image"], BUILD_POD_IMAGE)
            self.assertEqual(container["securityContext"], {"privileged": False, "allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}})
            self.assertEqual(container["resources"], {"requests": {"cpu": "7", "memory": "28Gi"}, "limits": {"memory": "28Gi"}})
        self.assertEqual(fetch["command"], ["sh", "-euc", server.BUILD_FETCH, "fetch", "programs", BUILT, "dev", "tools", "main"])
        self.assertEqual(fetch["volumeMounts"], [{"name": "repos", "mountPath": "/cache/repos"}, {"name": "work", "mountPath": "/work"}])
        fetched = {item["name"]: item.get("value", item.get("valueFrom")) for item in fetch["env"]}
        self.assertEqual(fetched["MIRRORS"], "/cache/repos")
        self.assertEqual(set(fetched), {"NODE", "HOME", "MIRRORS", "FORGE", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"})
        self.assertEqual(fetched["GIT_CONFIG_VALUE_0"], FETCHING.node)
        self.assertEqual(
            body["volumes"],
            [
                {"name": "cargo", "hostPath": {"path": "/srv/build-jobs/cargo", "type": "Directory"}},
                {"name": "repos", "hostPath": {"path": "/srv/build-jobs/repos", "type": "Directory"}},
                {"name": "sccache", "hostPath": {"path": "/srv/build-jobs/sccache", "type": "Directory"}},
                {"name": "work", "emptyDir": {}},
            ],
        )
        self.assertEqual(run["volumeMounts"], [{"name": "cargo", "mountPath": "/cache/cargo"}, {"name": "repos", "mountPath": "/cache/repos", "readOnly": True}, {"name": "sccache", "mountPath": "/cache/sccache"}, {"name": "work", "mountPath": "/work"}])
        env = {item["name"]: item.get("value", item.get("valueFrom")) for item in run["env"]}
        self.assertEqual(
            env,
            {
                "NODE": {"fieldRef": {"fieldPath": "spec.nodeName"}},
                "GATES": "check",
                "HOME": "/work",
                "MIRRORS": "/cache/repos",
                "CARGO_HOME": "/cache/cargo",
                "CARGO_TARGET_DIR": "/work/target",
                "FORGE": FETCHING.forge,
                "GATES_OUT": "/work/out",
                "SCCACHE_DIRS": "/cache/sccache",
                "GIT_CONFIG_COUNT": "1",
                "GIT_CONFIG_KEY_0": "duck.duckpond.node",
                "GIT_CONFIG_VALUE_0": FETCHING.node,
            },
        )
        self.assertEqual(run["command"], ["sh", "-euc", server.BUILD_ENTRY, "build", "programs", BUILT, "dev", "tools", "main"])

    def test_a_gates_build_takes_the_first_sccache_dir_no_build_holds_and_a_killed_ones_again(self):
        with tempfile.TemporaryDirectory() as dirs:
            env = {**os.environ, "SCCACHE_DIRS": dirs}
            shown = server.BUILD_CACHE_SLOT + 'echo "$SCCACHE_DIR $RUSTC_WRAPPER $CARGO_INCREMENTAL"\n'
            with subprocess.Popen(["sh", "-euc", shown + "read held\n"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as holder:
                self.assertEqual(holder.stdout.readline(), f"{dirs}/0 sccache 0\n")
                beside = subprocess.run(["sh", "-euc", shown], env=env, capture_output=True, text=True, check=True)
                self.assertEqual(beside.stdout, f"{dirs}/1 sccache 0\n")
                holder.kill()
            again = subprocess.run(["sh", "-euc", shown], env=env, capture_output=True, text=True, check=True)
            self.assertEqual(again.stdout, f"{dirs}/0 sccache 0\n")
            self.assertEqual(sorted(os.listdir(dirs)), ["0.lock", "1.lock"])

    def test_a_build_pod_passes_every_builds_pods_rule(self):
        for asked in (self.asked(gates="full"), self.asked(command=["make"])):
            self.builds.calls.clear()
            self.call("POST", "/v1/builds", asked)
            pod = self.built()
            body = pod["spec"]
            containers = body["containers"] + body["initContainers"]
            context = body["securityContext"]
            contexts = [container["securityContext"] for container in containers]
            exact = ({"path": "/srv/build-jobs/cargo", "type": "Directory"}, {"path": "/srv/build-jobs/repos", "type": "Directory"}, {"path": "/srv/build-jobs/sccache", "type": "Directory"})
            rules = {
                "run-as-uid-10001": (context["runAsUser"], context["runAsGroup"]) == (10001, 10001) and all("runAsUser" not in held for held in contexts),
                "run-as-non-root": context["runAsNonRoot"] is True,
                "no-privilege-escalation": all(held["allowPrivilegeEscalation"] is False for held in contexts),
                "drop-all-capabilities": all(held["capabilities"]["drop"] == ["ALL"] for held in contexts),
                "no-added-capabilities": all("add" not in held["capabilities"] for held in contexts),
                "not-privileged": all(held["privileged"] is False for held in contexts),
                "host-users-false": body["hostUsers"] is False,
                "seccomp-runtime-default": context["seccompProfile"] == {"type": "RuntimeDefault"},
                "no-host-namespaces": not any(body.get(name) for name in ("hostNetwork", "hostPID", "hostIPC")),
                "volume-types": all(set(volume) - {"name"} in ({"hostPath"}, {"emptyDir"}) for volume in body["volumes"]),
                "host-paths-exact": all(volume["hostPath"] in exact for volume in body["volumes"] if "hostPath" in volume),
                "no-host-ports": all("ports" not in container for container in containers),
                "proc-mount-default": all("procMount" not in held for held in contexts),
                "no-sysctls": "sysctls" not in context,
                "apparmor-runtime-default": all("appArmorProfile" not in held for held in [context, *contexts]) and not any(key.startswith("container.apparmor") for key in pod["metadata"]["annotations"]),
                "no-selinux-options": all("seLinuxOptions" not in held for held in [context, *contexts]),
                "image-toolchain": all(container["image"].startswith(("localhost:5000/toolchain:", "localhost:5000/toolchain@")) for container in containers),
                "priority-class-builds": body["priorityClassName"] == "builds",
                "node-selector-pods": body["nodeSelector"] == {"builds.ducktape/pods": "true"},
                "toleration-builds-taint": {"key": "ducktape.industries/builds", "operator": "Equal", "value": "latitude", "effect": "NoSchedule"} in body["tolerations"],
                "no-service-account-token": body["automountServiceAccountToken"] is False and not any("projected" in volume for volume in body["volumes"]),
            }
            self.assertEqual(len(rules), 21)
            self.assertEqual([rule for rule, held in rules.items() if not held], [], asked)

    def test_a_command_build_runs_its_words_and_no_gates(self):
        self.call("POST", "/v1/builds", self.asked(command=["cargo", "test", "-p", "blob"], env={"KEEP_GOING": "1"}, limit=25))
        pod = self.built()
        (run,) = pod["spec"]["containers"]
        self.assertEqual(run["command"][3:], ["build", "programs", BUILT, "dev", "tools", "main", "cargo", "test", "-p", "blob"])
        names = [item["name"] for item in run["env"]]
        self.assertNotIn("GATES", names)
        self.assertEqual(names[1], "KEEP_GOING")
        self.assertIn({"name": "CARGO_HOME", "value": "/work/cargo"}, run["env"])
        self.assertIn({"name": "MIRRORS", "value": "/cache/repos"}, run["env"])
        self.assertEqual(pod["spec"]["volumes"], [{"name": "repos", "hostPath": {"path": "/srv/build-jobs/repos", "type": "Directory"}}, {"name": "work", "emptyDir": {}}])
        (fetch,) = pod["spec"]["initContainers"]
        self.assertEqual(fetch["volumeMounts"], [{"name": "repos", "mountPath": "/cache/repos"}, {"name": "work", "mountPath": "/work"}])
        self.assertEqual(run["volumeMounts"], [{"name": "repos", "mountPath": "/cache/repos", "readOnly": True}, {"name": "work", "mountPath": "/work"}])
        self.assertEqual(pod["spec"]["activeDeadlineSeconds"], 1500)

    def test_a_build_made_already_answers_the_one_there(self):
        self.builds.answer("create", ran(code=1, err='Error from server (AlreadyExists): pods "duckpond-41" already exists'))
        self.builds.answer("get", ran(pod_json(phase="Running")))
        status, answer = self.call("POST", "/v1/builds", self.asked(gates="light"))
        self.assertEqual(status, 200, answer)
        self.assertEqual((answer["build"]["name"], answer["build"]["phase"]), ("duckpond-41", "Running"))

    def test_a_build_names_its_tree_and_one_way_to_run_it(self):
        for body, words in (
            (self.asked(), ["exactly one"]),
            (self.asked(gates="check", command=["make"]), ["exactly one"]),
            (self.asked(gates="fast"), ["full, check, light"]),
            (self.asked(repo="../x", gates="check"), ["repo"]),
            (self.asked(commit="abc", gates="check"), ["commit"]),
            (self.asked(base="-x", gates="check"), ["base"]),
            ({"repo": "programs", "commit": BUILT, "gates": "check"}, ["base"]),
            (self.asked(gates="check", env={"CARGO_HOME": "/x", "HOME": "/y"}), ["CARGO_HOME, HOME"]),
            (self.asked(gates="check", limit=0), ["limit"]),
            (self.asked(gates="check", limit=41), ["from 1 to 40"]),
            (self.asked(gates="check", image="ubuntu"), ["no field image"]),
            (self.asked(gates="check", requester=9), ["requester"]),
        ):
            self.refused("POST", "/v1/builds", body, 400, words)
        self.assertNotIn("create", self.builds.verbs())

    def test_a_build_is_read_from_its_own_namespace(self):
        self.builds.answer("get", ran(pod_json(phase="Succeeded")))
        self.builds.answer("logs", ran(b"GATES ok\n"))
        status, answer = self.call("GET", "/v1/builds/duckpond-41")
        self.assertEqual((status, answer["build"]["phase"]), (200, "Succeeded"))
        status, logs = self.call("GET", "/v1/builds/duckpond-41/logs?whole=1")
        self.assertEqual((status, logs), (200, b"GATES ok\n"))
        self.assertEqual(self.builds.verbs(), ["get", "logs"])
        self.assertNotIn("get", self.kube.verbs())

    def test_only_its_requester_deletes_a_build(self):
        self.builds.answer("get", ran(pod_json(requester="99")))
        self.refused("DELETE", "/v1/builds/duckpond-41", None, 403, ["99"])
        self.assertEqual(self.api.calls, [])

    def test_the_routes_are_counted_by_shape(self):
        self.assertEqual([server.route_of(path) for path in ("/v1/builds", "/v1/builds/x", "/v1/builds/x/logs")], ["builds", "builds/NAME", "builds/NAME/logs"])


class Unbuilt(Served):
    def test_a_service_with_no_build_image_makes_no_builds(self):
        self.refused("POST", "/v1/builds", {"repo": "programs", "commit": BUILT, "base": "dev", "gates": "check"}, 503, ["PODS_BUILD_IMAGE"])

    def test_a_build_setting_names_what_is_unset(self):
        with self.assertRaises(server.Refused) as caught:
            server.building_of({"PODS_BUILD_IMAGE": "localhost:5000/toolchain:x", "PODS_BUILD_CPU": "7"}, ["kubectl"])
        self.assertIn("PODS_BUILD_MEMORY", caught.exception.sentence)
        self.assertIsNone(server.building_of({}, ["kubectl"]))
        building = server.building_of({"PODS_BUILD_IMAGE": "localhost:5000/toolchain:x", "PODS_BUILD_CPU": "7000m", "PODS_BUILD_MEMORY": "28Gi"}, ["kubectl"])
        self.assertEqual((building.kube.namespace, building.kube.kept_name, building.cpu, building.memory), ("builds", "build-runs", "7", "28Gi"))


class Suite(unittest.TestCase):
    def test_no_two_classes_share_a_name(self):
        tree = ast.parse((HERE / "server-test.py").read_text())
        names = [node.name for node in tree.body if isinstance(node, ast.ClassDef)]
        self.assertEqual(sorted({name for name in names if names.count(name) > 1}), [], "a later class of the same name hides the earlier one's tests")


class StampEdges(Holder):
    def test_lookalike_domains_remain_callers_labels_and_annotations(self):
        key = "ducktape.industries.example/requester"
        manifest = deployment()
        manifest["metadata"].update({"labels": {key: "mine"}, "annotations": {key: "mine"}})
        self.kube.answer("apply", ran({"kind": "List", "items": []}))
        status, answer = self.call("POST", "/v1/manifests", {"manifests": [manifest]})
        self.assertEqual(status, 200, answer)
        applies = [call for call in self.kube.calls if call[0] == "apply"]
        for _, objects, _ in applies:
            self.assertEqual(objects[0]["metadata"]["labels"][key], "mine")
            self.assertEqual(objects[0]["metadata"]["annotations"][key], "mine")

    def test_an_empty_service_stamp_is_still_refused_before_apply(self):
        for field in ("labels", "annotations"):
            with self.subTest(field=field):
                manifest = deployment()
                manifest["metadata"][field] = {NOTED: ""}
                status, answer = self.call("POST", "/v1/manifests", {"manifests": [manifest]})
                self.assertEqual(status, 403, answer)
        self.assertNotIn("apply", self.kube.verbs())
        self.assertNotIn("trial", self.kube.verbs())
        self.assertEqual(self.api.calls, [])

    def test_each_stateful_claim_template_keeps_its_own_stamps(self):
        name = "io.k8s.api.apps.v1.StatefulSet"
        path = "/apis/apps/v1/namespaces/{namespace}/statefulsets/{name}"
        spec = {"type": "object", "properties": {
            "template": ref(CORE + "PodTemplateSpec"),
            "volumeClaimTemplates": {"type": "array", "items": ref(CORE + "PersistentVolumeClaim")},
        }}
        document = {
            "components": {"schemas": {
                **CORE_V1["components"]["schemas"],
                **APPS_V1["components"]["schemas"],
                name: kind_of("apps", "v1", "StatefulSet", spec=spec),
            }},
            "paths": {**APPS_V1["paths"], path: operations(name, "patch")},
        }
        self.api.published["apis/apps/v1"] = ("stateful", document)
        current = held_deployment({})
        current["kind"] = "StatefulSet"
        claims = [
            {"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": name, "labels": dict(STAMPED)}, "spec": {"resources": {"requests": {"storage": "1Gi"}}}}
            for name in ("data", "cache")
        ]
        current["spec"]["volumeClaimTemplates"] = claims
        changed = json.loads(json.dumps(current))
        changed["spec"]["volumeClaimTemplates"][0]["metadata"]["labels"].update(MOVED)
        self.api.answers = [answered(200, current), answered(200, changed), answered(200, changed)]
        status, answer = self.call("PATCH", "/v1/k8s/apis/apps/v1/namespaces/agents/statefulsets/web", b'{"spec": {}}', headers=MERGE)
        self.assertEqual(status, 403, answer)
        self.assertEqual([(call[0], call[2]) for call in self.api.calls], [("GET", ""), ("PATCH", "dryRun=All")])


class SenderReplies(Served):
    def test_each_create_asks_egress_about_that_requests_exact_number(self):
        self.signer.sender = sent_by(21)
        self.kube.answer("create", ran(pod_json()))
        self.kube.answer("apply", ran({"kind": "List", "items": []}))
        cases = (("/v1/pods", {"image": "t"}, 201, 41), ("/v1/manifests", {"manifests": [deployment()]}, 200, 42))
        for path, body, expected, request in cases:
            status, answer = self.call("POST", path, body, headers=asked(request))
            self.assertEqual(status, expected, answer)
        self.assertEqual(self.signer.senders, [bytes([server.EGRESS_SENDER]) + request.to_bytes(8, "little") for *_, request in cases])

    def test_a_sender_answer_of_another_variant_makes_nothing(self):
        self.signer.sender = ran(bytes([3, 1]) + (21).to_bytes(8, "little"))
        for path, body in (("/v1/pods", {"image": "t"}), ("/v1/manifests", {"manifests": [deployment()]})):
            with self.subTest(path=path):
                status, answer = self.call("POST", path, body)
                self.assertEqual(status, 502, answer)
        self.assertNotIn("create", self.kube.verbs())
        self.assertNotIn("apply", self.kube.verbs())
        self.assertNotIn("trial", self.kube.verbs())


class ClaimOwners(Served):
    def test_a_claims_pod_is_its_pod_controller_not_another_owner_reference(self):
        runner = pod_json(name="runner", phase="Running")
        observer = pod_json(name="observer", phase="Succeeded")
        references = (
            [{"apiVersion": "v1", "kind": "Pod", "name": "observer", "controller": False}, {"apiVersion": "v1", "kind": "Pod", "name": "runner", "controller": True}],
            [{"apiVersion": "apps/v1", "kind": "Deployment", "name": "runner", "controller": True}],
            [{"apiVersion": "v1", "kind": "Pod", "name": "observer"}],
        )
        claims = [
            {"metadata": {"name": f"claim-{index}", "ownerReferences": owners}, "spec": {"resources": {"requests": {"storage": "1Gi"}}}}
            for index, owners in enumerate(references)
        ]
        self.kube.answer("list", ran({"items": [runner, observer]}))
        self.kube.answer("claims", ran({"items": claims}))
        self.kube.answer("quotas", ran({"items": []}))
        status, answer = self.call("GET", "/v1/usage")
        self.assertEqual(status, 200, answer)
        self.assertEqual([(row["claim"], row["pod"], row["phase"]) for row in answer["claims"]], [("claim-0", "runner", "Running"), ("claim-1", None, None), ("claim-2", None, None)])


if __name__ == "__main__":
    unittest.main()
