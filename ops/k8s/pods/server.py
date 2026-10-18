#!/usr/bin/env python3
# The pods service: agents' computing on the cluster through a network's egress, in one namespace: pods run with fetched inputs, exec, logs, manifests applied and deleted, outputs put as blobs or files, posts when work ends, and usage.
# ops/k8s/pods/pods.yaml runs it on fabrics-1, server-test.py tests it, and `server.py fetch` is each pod's input fetcher.
import json
import os
import re
import secrets
import shlex
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

RUN = "run"
OUT = "out"
FETCH = "fetch"
OUT_PATH = "/out"
IN_ROOT = "/in"
SERVER = "/usr/local/lib/pods/server.py"
PRIORITY = "agents"
AGENT_ACCOUNT = "agent"
NOTE = "ducktape.industries/"
MANAGED = ("app.kubernetes.io/managed-by", "pods")
DEFAULT_CONTAINER = "kubectl.kubernetes.io/default-container"
ASKED = ("chain", "program", "request")
REQUESTER = NOTE + "requester"
NOTIFY_CHANNEL = NOTE + "notify-channel"
NOTIFY_THREAD = NOTE + "notify-thread"
NOTIFIED = NOTE + "notified"
SPEC = {
    "image": str,
    "command": list,
    "args": list,
    "env": dict,
    "workdir": str,
    "resources": dict,
    "inputs": list,
    "requester": int,
    "notify": dict,
}
RESOURCES = {"limits": dict, "requests": dict}
AMOUNTS = {"cpu": str, "memory": str}
NOTIFY = {"channel": str, "thread": int}
INPUT = {"path": str, "forge": dict, "files": dict, "blob": str}
SOURCES = ("forge", "files", "blob")
FORGE = {"repo": str, "commit": str}
FILES = {"path": str, "snapshot": str}
BLOB = {"path": str, "kind": str, "container": str, "requester": int, "notify": dict}
FILE = {"path": str, "to": str, "message": str, "container": str, "requester": int, "notify": dict}
EXEC = {"command": list, "container": str}
APPLY = {"manifests": list, "requester": int}
DELETE = {"kind": str, "name": str}
LOGS = {"tail", "since", "container"}
ALIASES = {
    "secrets": "secret",
    "ingresses": "ingress",
    "ing": "ingress",
    "networkpolicies": "networkpolicy",
    "netpol": "networkpolicy",
    "roles": "role",
    "rolebindings": "rolebinding",
}
REFUSED = {
    "secret": "a Secret holds a credential, and nothing in agents holds one",
    "ingress": "an Ingress exposes a service outside the cluster",
    "networkpolicy": "an agent's NetworkPolicy would add allow rules that undo agents' isolation",
    "role": "a Role grants permissions, and agents' accounts hold none",
    "rolebinding": "a RoleBinding grants permissions, and agents' accounts hold none",
}
EXPOSING = ("LoadBalancer", "NodePort")
TEMPLATED = {"deployment", "statefulset", "daemonset", "replicaset", "job", "replicationcontroller", "podtemplate"}
STATUS = {
    "NotFound": 404,
    "AlreadyExists": 409,
    "Conflict": 409,
    "Invalid": 422,
    "BadRequest": 400,
    "Forbidden": 403,
}
SCALE = {
    "": 1,
    "m": 1e-3,
    "k": 1e3,
    "M": 1e6,
    "G": 1e9,
    "T": 1e12,
    "P": 1e15,
    "E": 1e18,
    "Ki": 2**10,
    "Mi": 2**20,
    "Gi": 2**30,
    "Ti": 2**40,
    "Pi": 2**50,
    "Ei": 2**60,
}
ENDED = ("Succeeded", "Failed")
REJECTED = {"unauthorized": 403, "not_found": 404, "already_exists": 409, "wrong_state": 409}
CHAT = "chat"
FILES_PROGRAM = "files"
CHUNK = "chunk"
CHUNK_SIZE = 1 << 20
WATCH_PAUSE = 1
SERVICE_ACCOUNT = Path("/var/run/secrets/kubernetes.io/serviceaccount")


class Refused(Exception):
    def __init__(self, status, sentence):
        super().__init__(sentence)
        self.status = status
        self.sentence = sentence


@dataclass
class Ran:
    code: int
    out: bytes
    err: str


def run(argv, stdin=None, env=None):
    done = subprocess.run(argv, input=stdin, capture_output=True, env=env)
    return Ran(done.returncode, done.stdout, done.stderr.decode(errors="replace").strip())


def say(event, **fields):
    words = " ".join(f"{name}={value}" for name, value in fields.items())
    print(f"event={event} {words}".strip(), file=sys.stderr, flush=True)


def kube_refusal(ran, doing):
    found = re.search(r"Error from server \((\w+)\)", ran.err)
    status = STATUS.get(found.group(1), 502) if found else 502
    return Refused(status, f"{doing}: {ran.err or f'kubectl exited {ran.code}'}")


def text_or_hex(data, field):
    try:
        return {field: data.decode()}
    except UnicodeDecodeError:
        return {f"{field}_hex": data.hex()}


def u8(value):
    return bytes([value])


def u64(value):
    return value.to_bytes(8, "little")


def borsh_bytes(data):
    return len(data).to_bytes(4, "little") + data


def borsh_text(value):
    return borsh_bytes(value.encode())


def borsh_vec(items):
    return len(items).to_bytes(4, "little") + b"".join(items)


def borsh_option(value, encode):
    return b"\x00" if value is None else b"\x01" + encode(value)


HASHES = {"sha256": (0, 32), "sha1": (1, 20)}


def blob_id(text):
    kind, _, digest = text.partition(":")
    known = HASHES.get(kind)
    if known is None or not re.fullmatch(r"[0-9a-f]+", digest) or len(digest) != 2 * known[1]:
        raise Refused(400, f"{text} is not a blob id: one is sha256:<64 hex> or sha1:<40 hex>")
    return u8(known[0]) + bytes.fromhex(digest)


class Reader:
    def __init__(self, data):
        self.data = data
        self.at = 0

    def take(self, count):
        piece = self.data[self.at : self.at + count]
        if len(piece) != count:
            raise Refused(502, "a reply ended early")
        self.at += count
        return piece

    def u8(self):
        return self.take(1)[0]

    def u64(self):
        return int.from_bytes(self.take(8), "little")

    def bytes(self):
        return self.take(int.from_bytes(self.take(4), "little"))

    def blob(self):
        tag = self.u8()
        for name, (index, size) in HASHES.items():
            if index == tag:
                return f"{name}:{self.take(size).hex()}"
        raise Refused(502, f"a reply names hash kind {tag}")


def mention(account):
    return borsh_text(f"<@{account}>") + borsh_vec([u8(3) + u8(0) + u64(account)])


def post_message(channel, message_id, requester, line, thread):
    plain = lambda text: borsh_text(text) + borsh_vec([])
    spans = [plain(line)] if requester is None else [mention(requester), plain(f" {line}")]
    paragraph = u8(0) + borsh_vec(spans)
    return u8(4) + borsh_text(channel) + borsh_text(message_id) + borsh_vec([paragraph]) + borsh_option(thread, u64)


def files_head():
    return u8(0)


def head_of(reply):
    reader = Reader(reply)
    if reader.u8() != 0:
        raise Refused(502, "files answered its head with something else")
    return reader.blob() if reader.u8() == 1 else None


def files_read(path, snapshot, offset, length):
    return u8(5) + borsh_text(path) + borsh_option(snapshot, blob_id) + u64(offset) + u64(length)


def contents_of(reply):
    reader = Reader(reply)
    if reader.u8() != 5:
        raise Refused(502, "files answered a read with something else")
    snapshot = reader.blob()
    size, offset, data, eof = reader.u64(), reader.u64(), reader.bytes(), reader.u8() == 1
    return snapshot, size, offset, data, eof


def files_commit(base, message, path, size, chunks):
    content = u8(1) + u64(size) + borsh_vec([blob_id(chunk) for chunk in chunks])
    put = u8(0) + borsh_text(path) + u8(0) + (0).to_bytes(4, "little") + content
    return u8(1) + borsh_option(base, blob_id) + borsh_text(message) + borsh_vec([put])


def receipt(ran):
    text = ran.out.decode(errors="replace").strip()
    applied = re.match(r"preconfirmed applied ?([0-9a-f]*)", text)
    rejected = re.match(r"preconfirmed rejected (\w+): (.*)", text, re.S)
    if ran.code == 0 and applied:
        return "applied", applied.group(1), ""
    if ran.code == 0 and rejected:
        return "rejected", rejected.group(1), rejected.group(2).strip()
    return "failed", "", ran.err or text or f"ducktape exited {ran.code}"


class Kube:
    def __init__(self, kubectl, namespace):
        self.kubectl = kubectl
        self.namespace = namespace

    def argv(self, *args):
        return [*self.kubectl, "--namespace", self.namespace, *args]

    def call(self, *args, stdin=None):
        return run(self.argv(*args), stdin=stdin)

    def create(self, pod):
        return self.call("create", "--filename", "-", "--output", "json", stdin=json.dumps(pod).encode())

    def get(self, name):
        return self.call("get", "pod", name, "--output", "json")

    def list(self):
        return self.call("get", "pods", "--output", "json")

    def logs(self, name, options):
        return self.call("logs", name, *options)

    def delete(self, name):
        return self.call("delete", "pod", name, "--wait=false")

    def exec(self, name, container, command):
        chosen = ["--container", container] if container else []
        return self.call("exec", name, *chosen, "--", *command)

    def annotate(self, name, key, value):
        return self.call("annotate", "pod", name, f"{key}={value}", "--overwrite")

    def apply(self, objects, dry):
        listed = {"apiVersion": "v1", "kind": "List", "items": objects}
        tried = ["--dry-run=server"] if dry else []
        return self.call("apply", *tried, "--filename", "-", "--output", "json", stdin=json.dumps(listed).encode())

    def trial(self, pod):
        return self.call("create", "--dry-run=server", "--filename", "-", "--output", "name", stdin=json.dumps(pod).encode())

    def objects(self, kind, name):
        return self.call("get", kind, *([name] if name else []), "--output", "json")

    def claims(self):
        return self.call("get", "persistentvolumeclaims", "--output", "json")

    def quotas(self):
        return self.call("get", "resourcequotas", "--output", "json")

    def remove(self, kind, name):
        return self.call("delete", kind, name, "--wait=false", "--output", "name")

    def cluster_kinds(self):
        return run([*self.kubectl, "api-resources", "--namespaced=false", "--no-headers"])

    def watch(self):
        argv = self.argv("get", "pods", "--selector", "=".join(MANAGED), "--watch", "--output-watch-events", "--output", "json")
        return subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


class Signer:
    def __init__(self, ducktape, keys):
        self.ducktape = ducktape
        self.keys = keys

    def held(self):
        return (self.keys / "key").is_file() and (self.keys / "password").is_file()

    def env(self):
        return dict(
            os.environ,
            DUCKTAPE_USER_KEY=str(self.keys / "key"),
            DUCKTAPE_PASSWORD=(self.keys / "password").read_text().strip(),
            NO_COLOR="1",
        )

    def put(self, paths, kind):
        return run([*self.ducktape, "blob", "put", "--kind", kind, "--wait", *map(str, paths)], env=self.env())

    def submit(self, program, payload):
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / "op"
            file.write_bytes(payload)
            return run([*self.ducktape, "submit", program, str(file), "--preconfirm"], env=self.env())

    def query(self, program, request):
        return run([*self.ducktape, "query", program], stdin=request, env=self.env())


class Stats:
    def __init__(self, mount=SERVICE_ACCOUNT):
        self.mount = mount

    def summary(self, host):
        token = (self.mount / "token").read_text().strip()
        context = ssl.create_default_context(cafile=str(self.mount / "ca.crt"))
        request = urllib.request.Request(f"https://{host}:10250/stats/summary", headers={"Authorization": f"Bearer {token}"})
        with urllib.request.urlopen(request, context=context) as answer:
            return json.load(answer)


def checked(body, shape, what):
    if not isinstance(body, dict):
        raise Refused(400, f"{what} is a JSON object")
    unknown = sorted(set(body) - set(shape))
    if unknown:
        raise Refused(400, f"{what} has no field {', '.join(unknown)}; its fields are {', '.join(shape)}")
    for field, kind in shape.items():
        mistyped = field in body and (not isinstance(body[field], kind) or isinstance(body[field], bool))
        if mistyped:
            raise Refused(400, f"{what}'s {field} is a JSON {kind.__name__}")
    return body


def named(body, fields, what):
    missing = [field for field in fields if not body.get(field)]
    if missing:
        raise Refused(400, f"{what} names its {' and '.join(missing)}")
    return body


def strings(value, field):
    if not value or not all(isinstance(item, str) for item in value):
        raise Refused(400, f"{field} holds strings")
    return value


def number(value, field):
    if value < 0:
        raise Refused(400, f"{field} is a number from 0")
    return value


def absolute(path, field):
    if not path.startswith("/"):
        raise Refused(400, f"{field} is an absolute path")
    return path


def asked_by(headers):
    asked = {name: headers.get(f"ducktape-{name}") for name in ASKED}
    missing = [f"ducktape-{name}" for name, value in asked.items() if not value]
    if missing:
        raise Refused(400, f"this service answers a network's egress calls only, and {', '.join(missing)} is missing")
    return asked


def labels_for(asked, requester):
    labels = {NOTE + name: asked[name] for name in ASKED}
    if requester is not None:
        labels[REQUESTER] = str(number(requester, "requester"))
    return labels


def notify_notes(body):
    if "notify" not in body:
        return {}
    notify = checked(body["notify"], NOTIFY, "notify")
    named(notify, ["channel"], "notify")
    notes = {NOTIFY_CHANNEL: notify["channel"]}
    if "thread" in notify:
        notes[NOTIFY_THREAD] = str(number(notify["thread"], "notify's thread"))
    return notes


def input_of(given, index):
    what = f"input {index}"
    checked(given, INPUT, what)
    sources = [source for source in SOURCES if source in given]
    if len(sources) != 1:
        raise Refused(400, f"{what} names one source of {', '.join(SOURCES)}")
    absolute(named(given, ["path"], what)["path"], f"{what}'s path")
    match sources[0]:
        case "forge":
            named(checked(given["forge"], FORGE, f"{what}'s forge"), list(FORGE), f"{what}'s forge")
        case "files":
            files = checked(given["files"], FILES, f"{what}'s files")
            absolute(files.get("path", ""), f"{what}'s files path")
            if "snapshot" in files:
                blob_id(files["snapshot"])
        case "blob":
            blob_id(given["blob"])
    return given


def confined():
    return {"allowPrivilegeEscalation": False, "seccompProfile": {"type": "RuntimeDefault"}}


@dataclass
class Fetching:
    image: str
    node: str
    forge: str


def resources_of(given):
    checked(given, RESOURCES, "resources")
    return {group: dict(checked(given[group], AMOUNTS, f"resources' {group}")) for group in RESOURCES if group in given}


def pod_spec(asked, spec, fetching, namespace):
    named(spec, ["image"], "a pod")
    env = spec.get("env", {})
    if not all(isinstance(value, str) for value in env.values()):
        raise Refused(400, "env maps names to strings")
    inputs = [input_of(given, index) for index, given in enumerate(spec.get("inputs", []))]
    mounts = [{"name": OUT, "mountPath": OUT_PATH}] + [
        {"name": f"in-{index}", "mountPath": given["path"], "subPath": "data"} for index, given in enumerate(inputs)
    ]
    run_container = {
        "name": RUN,
        "image": spec["image"],
        "env": [{"name": name, "value": value} for name, value in env.items()],
        "volumeMounts": mounts,
        "securityContext": confined(),
    }
    if "command" in spec:
        run_container["command"] = strings(spec["command"], "command")
    if "args" in spec:
        run_container["args"] = strings(spec["args"], "args")
    if "workdir" in spec:
        run_container["workingDir"] = spec["workdir"]
    out_container = {
        "name": OUT,
        "image": fetching.image,
        "command": ["sleep", "infinity"],
        "volumeMounts": [{"name": OUT, "mountPath": OUT_PATH}],
        "securityContext": confined(),
    }
    body = {
        "restartPolicy": "Never",
        "serviceAccountName": AGENT_ACCOUNT,
        "automountServiceAccountToken": False,
        "enableServiceLinks": False,
        "hostUsers": False,
        "priorityClassName": PRIORITY,
        "containers": [run_container, out_container],
        "volumes": [{"name": OUT, "emptyDir": {}}] + [{"name": f"in-{index}", "emptyDir": {}} for index in range(len(inputs))],
    }
    if "resources" in spec:
        body["resources"] = resources_of(spec["resources"])
    if inputs:
        body["initContainers"] = [fetch_container(inputs, fetching)]
    return {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": f"{asked['chain']}-{asked['request']}",
            "namespace": namespace,
            "labels": {**dict([MANAGED]), **labels_for(asked, spec.get("requester"))},
            "annotations": {DEFAULT_CONTAINER: RUN, **notify_notes(spec)},
        },
        "spec": body,
    }


def fetch_container(inputs, fetching):
    return {
        "name": FETCH,
        "image": fetching.image,
        "command": ["python3", SERVER, "fetch"],
        "env": [
            {"name": "PODS_INPUTS", "value": json.dumps(inputs)},
            {"name": "PODS_NODE", "value": fetching.node},
            {"name": "PODS_FORGE", "value": fetching.forge},
        ],
        "volumeMounts": [{"name": f"in-{index}", "mountPath": f"{IN_ROOT}/{index}"} for index in range(len(inputs))],
        "terminationMessagePolicy": "FallbackToLogsOnError",
        "securityContext": confined(),
    }


def refused_kind(kind):
    base = kind.lower().split(".")[0]
    return ALIASES.get(base, base)


def pod_specs(manifest):
    kind = manifest.get("kind", "").lower()
    spec = manifest.get("spec") or {}
    match kind:
        case "pod":
            return [(manifest, spec)]
        case "cronjob":
            template = ((spec.get("jobTemplate") or {}).get("spec") or {}).get("template") or {}
            return [(template, template.get("spec") or {})]
        case "podtemplate":
            template = manifest.get("template") or {}
            return [(template, template.get("spec") or {})]
        case templated if templated in TEMPLATED:
            template = spec.get("template") or {}
            return [(template, template.get("spec") or {})]
        case _:
            return []


def confine_pod(spec, where):
    hosted = [name for name in ("hostNetwork", "hostPID", "hostIPC") if spec.get(name)]
    if hosted:
        raise Refused(403, f"{where} asks for {', '.join(hosted)}: an agent's pod shares no host namespace")
    for volume in spec.get("volumes") or []:
        if "hostPath" in volume:
            raise Refused(403, f"{where} mounts hostPath volume {volume.get('name')}: an agent's pod mounts nothing of the host")
        projected = (volume.get("projected") or {}).get("sources") or []
        if any("serviceAccountToken" in source for source in projected):
            raise Refused(403, f"{where} projects a service account token: an agent's pod holds no token")
    for group in ("containers", "initContainers", "ephemeralContainers"):
        for container in spec.get(group) or []:
            if (container.get("securityContext") or {}).get("privileged"):
                raise Refused(403, f"{where} runs container {container.get('name')} privileged: an agent's container runs unprivileged")
    chosen = spec.get("priorityClassName", PRIORITY)
    if chosen != PRIORITY:
        raise Refused(403, f"{where} asks for priority class {chosen}: everything in agents runs at {PRIORITY}, below the rest of the cluster")
    spec["priorityClassName"] = PRIORITY
    spec["automountServiceAccountToken"] = False


def confine_manifest(manifest, index, cluster_kinds, namespace, labels):
    what = f"manifest {index}"
    if not isinstance(manifest, dict) or not manifest.get("kind") or not manifest.get("apiVersion"):
        raise Refused(400, f"{what} is a Kubernetes object with an apiVersion and a kind")
    kind = manifest["kind"]
    meta = manifest.setdefault("metadata", {})
    where = f"{what} ({kind} {meta.get('name', '')})".replace(" )", ")")
    if refused_kind(kind) in REFUSED:
        raise Refused(403, f"{where} is refused: {REFUSED[refused_kind(kind)]}")
    if kind == "List":
        raise Refused(400, f"{what} is a List: send its items as manifests")
    if kind in cluster_kinds:
        raise Refused(403, f"{where} is refused: a {kind} is cluster-scoped, and agents reach only namespace {namespace}")
    if meta.get("namespace", namespace) != namespace:
        raise Refused(403, f"{where} names namespace {meta['namespace']}: agents reach only namespace {namespace}")
    meta["namespace"] = namespace
    meta["labels"] = {**(meta.get("labels") or {}), **labels}
    if kind == "Service":
        spec = manifest.get("spec") or {}
        exposed = spec.get("type") in EXPOSING or bool(spec.get("externalIPs"))
        if exposed:
            raise Refused(403, f"{where} is refused: a {spec.get('type') or 'Service with externalIPs'} exposes it outside the cluster; a ClusterIP Service is fine")
    if kind == "ServiceAccount":
        manifest["automountServiceAccountToken"] = False
    for template, spec in pod_specs(manifest):
        template.setdefault("metadata", {})
        template["metadata"]["labels"] = {**(template["metadata"].get("labels") or {}), **labels}
        confine_pod(spec, where)
    return manifest


def trial_pod(manifest, template, spec, namespace):
    name = manifest["metadata"].get("name") or manifest["kind"].lower()
    labels = (template.get("metadata") or {}).get("labels") or {}
    return {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"generateName": f"{name}-", "namespace": namespace, "labels": labels},
        "spec": spec,
    }


def state_of(statuses, name):
    found = [entry for entry in statuses if entry.get("name") == name]
    if not found or not found[0].get("state"):
        return None
    (kind, detail), = found[0]["state"].items()
    return {
        "state": kind,
        "reason": detail.get("reason"),
        "message": detail.get("message"),
        "exit_code": detail.get("exitCode"),
        "started": detail.get("startedAt"),
        "finished": detail.get("finishedAt"),
    }


def unscheduled(status):
    held = [entry for entry in status.get("conditions", []) if entry.get("type") == "PodScheduled" and entry.get("status") == "False"]
    return held[0].get("message") if held else None


def notify_of(notes):
    channel = notes.get(NOTIFY_CHANNEL)
    if channel is None:
        return None
    thread = notes.get(NOTIFY_THREAD)
    return {"channel": channel, "thread": int(thread) if thread is not None else None}


def requester_of(labels):
    requester = labels.get(REQUESTER)
    return int(requester) if requester is not None else None


def containers_of(spec, statuses, group):
    return [
        {
            "name": container["name"],
            "image": container.get("image"),
            "command": container.get("command", []),
            "args": container.get("args", []),
            "workdir": container.get("workingDir"),
            "resources": container.get("resources") or {},
            "status": state_of(statuses, container["name"]),
        }
        for container in spec.get(group) or []
    ]


def inputs_of(spec):
    fetchers = [entry for entry in spec.get("initContainers") or [] if entry["name"] == FETCH]
    values = [item.get("value") for item in (fetchers[0].get("env") or []) if item["name"] == "PODS_INPUTS"] if fetchers else []
    return json.loads(values[0]) if values else []


def view(pod):
    meta = pod["metadata"]
    spec = pod.get("spec", {})
    status = pod.get("status", {})
    labels = meta.get("labels") or {}
    return {
        "name": meta["name"],
        "created": meta.get("creationTimestamp"),
        "phase": status.get("phase"),
        "reason": status.get("reason"),
        "message": status.get("message"),
        "unscheduled": unscheduled(status),
        "resources": spec.get("resources") or {},
        "containers": containers_of(spec, status.get("containerStatuses", []), "containers"),
        "init": containers_of(spec, status.get("initContainerStatuses", []), "initContainers"),
        "inputs": inputs_of(spec),
        "asked": {name: labels.get(NOTE + name) for name in ASKED},
        "requester": requester_of(labels),
        "notify": notify_of(meta.get("annotations") or {}),
    }


def ended_line(pod):
    name = pod["metadata"]["name"]
    status = pod.get("status", {})
    ran = state_of(status.get("containerStatuses", []), RUN)
    fetched = state_of(status.get("initContainerStatuses", []), FETCH)
    run_ended = ran is not None and ran["state"] == "terminated"
    fetch_failed = fetched is not None and fetched["state"] == "terminated" and fetched["exit_code"] != 0
    if run_ended:
        reason = f" ({ran['reason']})" if ran["reason"] else ""
        return f"pod {name} ended: exit code {ran['exit_code']}{reason}"
    if fetch_failed:
        why = (fetched["message"] or "").strip().splitlines()
        detail = f": {why[-1]}" if why else ""
        return f"pod {name} ended: fetching its inputs failed, exit code {fetched['exit_code']}{detail}"
    if status.get("phase") == "Failed":
        return f"pod {name} ended: {status.get('reason') or 'failed'}"
    return None


def log_options(query):
    unknown = sorted(set(query) - LOGS)
    if unknown:
        raise Refused(400, f"logs take no {', '.join(unknown)}; they take tail, since and container")
    options = ["--container", query["container"][-1]] if "container" in query else []
    for name, flag, unit in (("tail", "--tail", ""), ("since", "--since", "s")):
        if name not in query:
            continue
        value = query[name][-1]
        if not value.isdigit():
            raise Refused(400, f"{name} is a whole number")
        options.append(f"{flag}={value}{unit}")
    return options


def put_answer(text, count):
    lines = text.splitlines()
    ids = [line.strip() for line in lines[:count]]
    if len(ids) != count or not all(":" in blob for blob in ids):
        raise Refused(502, f"the blob put printed {text!r}, not {count} blob ids")
    answer = {"blobs": ids}
    for line in lines[count:]:
        put = re.fullmatch(r"bundle (\d+) put at block (\d+)", line.strip())
        held = re.fullmatch(r"bundle (\d+) held since block (\d+)", line.strip())
        if put:
            answer["bundle"], answer["put_at"] = int(put.group(1)), int(put.group(2))
        if held:
            answer["held_since"] = int(held.group(2))
    return answer


def quantity(text):
    found = re.fullmatch(r"([+-]?[0-9.]+)(?:[eE]([+-]?\d+))?([a-zA-Z]*)", str(text))
    if found is None or found.group(3) not in SCALE:
        raise Refused(502, f"{text} is not a Kubernetes quantity")
    return float(found.group(1)) * 10 ** int(found.group(2) or 0) * SCALE[found.group(3)]


def pod_limit(spec, resource):
    stated = ((spec.get("resources") or {}).get("limits") or {}).get(resource)
    if stated is not None:
        return quantity(stated)
    amount = lambda container: quantity(((container.get("resources") or {}).get("limits") or {}).get(resource, "0"))
    running = sum(amount(container) for container in spec.get("containers") or [])
    starting = max((amount(container) for container in spec.get("initContainers") or []), default=0)
    return max(running, starting)


def usage_of(pods, claims, summaries, quotas, namespace):
    measured = {}
    for summary in summaries:
        for entry in summary.get("pods", []):
            reference = entry.get("podRef", {})
            if reference.get("namespace") == namespace:
                measured[reference.get("name")] = entry
    rows = []
    for pod in pods:
        name = pod["metadata"]["name"]
        live = pod.get("status", {}).get("phase") not in ENDED
        entry = measured.get(name, {})
        rows.append(
            {
                "pod": name,
                "requester": requester_of(pod["metadata"].get("labels") or {}),
                "phase": pod.get("status", {}).get("phase"),
                "cpu_limit": pod_limit(pod.get("spec", {}), "cpu") if live else 0,
                "memory_limit_bytes": int(pod_limit(pod.get("spec", {}), "memory")) if live else 0,
                "cpu_nanocores": (entry.get("cpu") or {}).get("usageNanoCores") or 0,
                "memory_bytes": (entry.get("memory") or {}).get("workingSetBytes") or 0,
                "disk_bytes": (entry.get("ephemeral-storage") or {}).get("usedBytes") or 0,
            }
        )
    held = [
        {
            "claim": claim["metadata"]["name"],
            "requester": requester_of(claim["metadata"].get("labels") or {}),
            "storage_requested_bytes": int(quantity(((claim.get("spec") or {}).get("resources") or {}).get("requests", {}).get("storage", "0"))),
        }
        for claim in claims
    ]
    totals = {}
    fields = ("cpu_limit", "memory_limit_bytes", "cpu_nanocores", "memory_bytes", "disk_bytes", "storage_requested_bytes")
    for row in rows + held:
        total = totals.setdefault(row["requester"], {"requester": row["requester"], "pods": 0, "claims": 0, **{field: 0 for field in fields}})
        total["pods" if "pod" in row else "claims"] += 1
        for field in fields:
            total[field] += row.get(field, 0)
    ordered = sorted(totals.values(), key=lambda total: (total["requester"] is None, total["requester"] or 0))
    quota = {item["metadata"]["name"]: {"hard": item.get("status", {}).get("hard", {}), "used": item.get("status", {}).get("used", {})} for item in quotas}
    return {"quota": quota, "requesters": ordered, "pods": rows, "claims": held}


class Notifier:
    def __init__(self, kube, signer):
        self.kube = kube
        self.signer = signer

    def post(self, notes, requester, message_id, line):
        notify = notify_of(notes)
        if notify is None:
            return False
        if not self.signer.held():
            say("pods_unposted", message=message_id, reason="no_key")
            return False
        payload = post_message(notify["channel"], message_id, requester, line, notify["thread"])
        outcome, reason, sentence = receipt(self.signer.submit(CHAT, payload))
        posted = outcome == "applied" or (outcome == "rejected" and reason == "already_exists")
        if posted:
            say("pods_posted", message=message_id)
            return True
        say("pods_unposted", message=message_id, reason=reason or outcome, detail=json.dumps(sentence))
        return False

    def seen(self, pod):
        meta = pod["metadata"]
        notes = meta.get("annotations") or {}
        line = ended_line(pod)
        unposted = line is not None and NOTIFIED not in notes and NOTIFY_CHANNEL in notes
        if not unposted:
            return
        name = meta["name"]
        if self.post(notes, requester_of(meta.get("labels") or {}), f"pods-{name}-ended", line):
            self.kube.annotate(name, NOTIFIED, "ended")

    def watch(self):
        failures = 0
        while True:
            process = self.kube.watch()
            for event in json_stream(process.stdout):
                if event.get("type") in ("ADDED", "MODIFIED"):
                    self.seen(event["object"])
            code = process.wait()
            if code == 0:
                failures = 0
                continue
            failures += 1
            if failures & (failures - 1) == 0:
                detail = process.stderr.read().decode(errors="replace").strip()
                say("pods_unwatched", attempts=failures, reason="kubectl_failed", detail=json.dumps(detail))
            time.sleep(WATCH_PAUSE)


def json_stream(stream):
    decoder = json.JSONDecoder()
    buffer = ""
    while chunk := stream.read1(65536):
        buffer += chunk.decode(errors="replace")
        while True:
            buffer = buffer.lstrip()
            try:
                value, end = decoder.raw_decode(buffer)
            except json.JSONDecodeError:
                break
            yield value
            buffer = buffer[end:]


class Service:
    def __init__(self, kube, signer, fetching, stats):
        self.kube = kube
        self.signer = signer
        self.fetching = fetching
        self.stats = stats
        self.notifier = Notifier(kube, signer)

    def pod(self, name):
        ran = self.kube.get(name)
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name} was not read")
        return json.loads(ran.out)

    def keyed(self, doing):
        if not self.signer.held():
            raise Refused(503, f"this service holds no key to {doing} with")

    def output(self, name, asked):
        pod = self.pod(name)
        names = [container["name"] for container in pod["spec"]["containers"]]
        container = asked.get("container") or (OUT if OUT in names else None)
        ran = self.kube.exec(name, container, ["cat", "--", absolute(asked["path"], "path")])
        if ran.code != 0:
            raise kube_refusal(ran, f"{asked['path']} was not read from pod {name}")
        return pod, ran.out

    def requester(self, asked, pod):
        if "requester" in asked:
            return number(asked["requester"], "requester")
        return requester_of(pod["metadata"].get("labels") or {})

    def notes(self, asked, pod):
        return notify_notes(asked) or (pod["metadata"].get("annotations") or {})

    def create(self, asked, body):
        spec = checked(body, SPEC, "a pod")
        pod = pod_spec(asked, spec, self.fetching, self.kube.namespace)
        ran = self.kube.create(pod)
        if ran.code == 0:
            return 201, {"pod": view(json.loads(ran.out))}
        refusal = kube_refusal(ran, "the pod was not created")
        repeated = refusal.status == 409 and "AlreadyExists" in ran.err
        if not repeated:
            raise refusal
        return 200, {"pod": view(self.pod(pod["metadata"]["name"]))}

    def list(self):
        return 200, {"pods": [view(pod) for pod in self.items(self.kube.list(), "the pods were not listed")]}

    def status(self, name):
        return 200, {"pod": view(self.pod(name))}

    def logs(self, name, query):
        ran = self.kube.logs(name, log_options(query))
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name}'s logs were not read")
        return 200, ran.out

    def delete(self, name):
        ran = self.kube.delete(name)
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name} was not deleted")
        return 200, {"deleted": name}

    def exec(self, name, body):
        asked = checked(body, EXEC, "an exec")
        command = strings(asked.get("command", []), "command")
        ran = self.kube.exec(name, asked.get("container"), command)
        reached = re.search(r"command terminated with exit code (\d+)", ran.err)
        server_refused = ran.code != 0 and reached is None and "Error from server" in ran.err
        if server_refused:
            raise kube_refusal(ran, f"pod {name} was not reached")
        stderr = re.sub(r"\n?command terminated with exit code \d+\s*$", "", ran.err)
        return 200, {"exit_code": ran.code, **text_or_hex(ran.out, "stdout"), **text_or_hex(stderr.encode(), "stderr")}

    def blob(self, name, body):
        asked = named(checked(body, BLOB, "a blob"), ["path", "kind"], "a blob")
        self.keyed("put blobs")
        pod, data = self.output(name, asked)
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / "blob"
            file.write_bytes(data)
            put = self.signer.put([file], asked["kind"])
        if put.code != 0:
            raise Refused(502, f"the blob was not put: {put.err or put.out.decode(errors='replace')}")
        held = put_answer(put.out.decode(), 1)
        blob = held.pop("blobs")[0]
        answer = {"blob": blob, "size": len(data), "kind": asked["kind"], **held}
        since = f" since block {answer['held_since']}" if "held_since" in answer else ""
        digest = blob.rsplit(":", 1)[-1][:16]
        line = f"pod {name}: blob {blob} is held{since}"
        self.notifier.post(self.notes(asked, pod), self.requester(asked, pod), f"pods-{name}-blob-{digest}", line)
        return 200, answer

    def file(self, name, body):
        asked = named(checked(body, FILE, "a file"), ["path", "to"], "a file")
        to = absolute(asked["to"], "to")
        self.keyed("write files")
        pod, data = self.output(name, asked)
        pieces = [data[start : start + CHUNK_SIZE] for start in range(0, len(data), CHUNK_SIZE)]
        chunks = []
        with tempfile.TemporaryDirectory() as temp:
            paths = [Path(temp) / f"chunk-{index}" for index in range(len(pieces))]
            for path, piece in zip(paths, pieces):
                path.write_bytes(piece)
            if paths:
                put = self.signer.put(paths, CHUNK)
                if put.code != 0:
                    raise Refused(502, f"the file's chunks were not put: {put.err or put.out.decode(errors='replace')}")
                chunks = put_answer(put.out.decode(), len(paths))["blobs"]
        head = self.signer.query(FILES_PROGRAM, files_head())
        if head.code != 0:
            raise Refused(502, f"files' head was not read: {head.err}")
        base = head_of(head.out)
        message = asked.get("message") or f"pods: {name} wrote {to}"
        outcome, reason, sentence = receipt(self.signer.submit(FILES_PROGRAM, files_commit(base, message, to, len(data), chunks)))
        if outcome == "rejected":
            raise Refused(REJECTED.get(reason, 422), f"files refused the write: {reason}: {sentence}")
        if outcome == "failed":
            raise Refused(502, f"the write was not sent: {sentence}")
        digest = (chunks[0].rsplit(":", 1)[-1] if chunks else "empty")[:16]
        line = f"pod {name}: wrote {to} into files ({len(data)} bytes)"
        self.notifier.post(self.notes(asked, pod), self.requester(asked, pod), f"pods-{name}-file-{digest}", line)
        return 200, {"written": to, "size": len(data), "chunks": chunks, "base": base, "preconfirmed": True}

    def cluster_kinds(self):
        ran = self.kube.cluster_kinds()
        if ran.code != 0:
            raise Refused(502, f"the cluster's kinds were not read: {ran.err}")
        return {line.split()[-1] for line in ran.out.decode().splitlines() if line.strip()}

    def items(self, ran, doing):
        if ran.code != 0:
            raise kube_refusal(ran, doing)
        return json.loads(ran.out)["items"]

    def apply(self, asked, body):
        request = named(checked(body, APPLY, "an apply"), ["manifests"], "an apply")
        labels = labels_for(asked, request.get("requester"))
        kinds = self.cluster_kinds()
        namespace = self.kube.namespace
        objects = [confine_manifest(manifest, index, kinds, namespace, labels) for index, manifest in enumerate(request["manifests"])]
        for index, manifest in enumerate(objects):
            templated = manifest["kind"] != "Pod"
            for template, spec in pod_specs(manifest) if templated else []:
                tried = self.kube.trial(trial_pod(manifest, template, spec, namespace))
                if tried.code != 0:
                    raise kube_refusal(tried, f"manifest {index} ({manifest['kind']} {manifest['metadata'].get('name', '')})'s pods would be refused")
        tried = self.kube.apply(objects, dry=True)
        if tried.code != 0:
            raise kube_refusal(tried, "the manifests were not applied")
        ran = self.kube.apply(objects, dry=False)
        if ran.code != 0:
            raise kube_refusal(ran, "the manifests were not applied")
        applied = json.loads(ran.out)
        items = applied.get("items", [applied]) if applied.get("kind") == "List" else [applied]
        return 200, {"applied": [{"kind": item.get("kind"), "name": item["metadata"]["name"]} for item in items]}

    def readable(self, kind, name):
        if refused_kind(kind) in REFUSED:
            raise Refused(403, f"{kind} {name or ''} is refused: {REFUSED[refused_kind(kind)]}".replace("  ", " "))

    def read(self, kind, name):
        self.readable(kind, name)
        ran = self.kube.objects(kind, name)
        if ran.code != 0:
            raise kube_refusal(ran, f"{kind} {name or ''} was not read".replace("  ", " "))
        found = json.loads(ran.out)
        return 200, {"items": found["items"]} if name is None else {"object": found}

    def remove(self, body):
        asked = named(checked(body, DELETE, "a delete"), ["kind", "name"], "a delete")
        self.readable(asked["kind"], asked["name"])
        ran = self.kube.remove(asked["kind"], asked["name"])
        if ran.code != 0:
            raise kube_refusal(ran, f"{asked['kind']} {asked['name']} was not deleted")
        return 200, {"deleted": ran.out.decode().strip()}

    def usage(self):
        pods = self.items(self.kube.list(), "the pods were not listed")
        claims = self.items(self.kube.claims(), "the volume claims were not listed")
        quotas = self.items(self.kube.quotas(), "the quota was not read")
        hosts = sorted({pod.get("status", {}).get("hostIP") for pod in pods} - {None})
        try:
            summaries = [self.stats.summary(host) for host in hosts]
        except OSError as error:
            raise Refused(502, f"the kubelet's stats were not read: {error}") from error
        return 200, usage_of(pods, claims, summaries, quotas, self.kube.namespace)

    def answer(self, method, target, headers, body):
        parts = urlsplit(target)
        segments = [segment for segment in parts.path.split("/") if segment]
        query = parse_qs(parts.query, keep_blank_values=True)
        asked = asked_by(headers)
        if segments[:1] != ["v1"]:
            raise Refused(404, f"{parts.path} is not a route; routes are under /v1")
        match (method, *segments[1:]):
            case ("POST", "pods"):
                return self.create(asked, parsed(body))
            case ("GET", "pods"):
                return self.list()
            case ("GET", "pods", name):
                return self.status(name)
            case ("GET", "pods", name, "logs"):
                return self.logs(name, query)
            case ("POST", "pods", name, "exec"):
                return self.exec(name, parsed(body))
            case ("POST", "pods", name, "blob"):
                return self.blob(name, parsed(body))
            case ("POST", "pods", name, "file"):
                return self.file(name, parsed(body))
            case ("DELETE", "pods", name):
                return self.delete(name)
            case ("POST", "manifests"):
                return self.apply(asked, parsed(body))
            case ("DELETE", "manifests"):
                return self.remove(parsed(body))
            case ("GET", "manifests", kind):
                return self.read(kind, None)
            case ("GET", "manifests", kind, name):
                return self.read(kind, name)
            case ("GET", "usage"):
                return self.usage()
            case _:
                raise Refused(404, f"{method} {parts.path} is not a route")


def parsed(body):
    try:
        return json.loads(body or b"{}")
    except json.JSONDecodeError as error:
        raise Refused(400, f"the body is not JSON: {error}") from error


def handler(service):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def respond(self, method):
            length = int(self.headers.get("content-length") or 0)
            body = self.rfile.read(length) if length else b""
            headers = {name.lower(): value for name, value in self.headers.items()}
            try:
                status, answer = service.answer(method, self.path, headers, body)
            except Refused as refusal:
                status, answer = refusal.status, {"error": refusal.sentence}
            text = isinstance(answer, bytes)
            payload = answer if text else json.dumps(answer).encode()
            kind = "text/plain; charset=utf-8" if text else "application/json"
            self.send_response(status)
            self.send_header("content-type", kind)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):
            self.respond("GET")

        def do_POST(self):
            self.respond("POST")

        def do_DELETE(self):
            self.respond("DELETE")

        def log_message(self, form, *args):
            route = urlsplit(self.path).path
            status = args[1] if len(args) > 1 else ""
            say("pods_request", method=self.command, route=route, status=status)

    return Handler


class Fetcher:
    def __init__(self, ducktape, node, forge, home, runner=run):
        self.ducktape = ducktape
        self.node = node
        self.forge = forge
        self.home = home
        self.runner = runner
        self.password = secrets.token_hex(16)

    def env(self):
        return dict(os.environ, DUCKTAPE_PASSWORD=self.password, NO_COLOR="1", DUCKTAPE_DEVICE_KEY_STORE="file")

    def cli(self, *args, stdin=None):
        return self.runner([*self.ducktape, "--workspace", str(self.home), "--node", self.node, *args], stdin=stdin, env=self.env())

    def must(self, ran, doing):
        if ran.code != 0:
            raise Refused(502, f"{doing}: {ran.err or ran.out.decode(errors='replace')}")
        return ran.out

    def reader(self):
        keys = self.home / "keys"
        minted = keys.exists() and any(keys.iterdir())
        if not minted:
            self.must(self.cli("wallet", "new", "fetch"), "a reading key was not minted")

    def forge_input(self, forge, target):
        chain = urlsplit(self.forge).netloc
        remote = f"{self.forge.rstrip('/')}/{forge['repo']}"
        git = ["git", "-c", f"duck.{chain}.node={self.node}"]
        self.must(self.runner([*git, "clone", "--no-checkout", remote, str(target)], env=self.env()), f"{remote} was not cloned")
        checkout = self.runner([*git, "-C", str(target), "checkout", "--detach", forge["commit"]], env=self.env())
        self.must(checkout, f"{forge['repo']} has no commit {forge['commit']}")
        print(f"cloned {remote} at {forge['commit']}", flush=True)

    def files_input(self, files, target):
        self.reader()
        snapshot = files.get("snapshot")
        offset, data, size = 0, b"", 0
        while True:
            request = files_read(files["path"], snapshot, offset, CHUNK_SIZE)
            reply = self.must(self.cli("query", FILES_PROGRAM, stdin=request), f"files did not read {files['path']}")
            read_at, size, _, piece, eof = contents_of(reply)
            snapshot = snapshot or read_at
            data += piece
            offset += len(piece)
            if eof or not piece:
                break
        target.write_bytes(data)
        print(f"read {files['path']} at snapshot {snapshot}, {len(data)} of {size} bytes", flush=True)

    def blob_input(self, blob, target):
        framed = self.must(self.cli("blob", "get", blob), f"blob {blob} was not fetched")
        header, nul, body = framed.partition(b"\0")
        if not nul:
            raise Refused(502, f"blob {blob} came back without its header")
        target.write_bytes(body)
        print(f"fetched blob {blob}, {header.decode(errors='replace')}", flush=True)

    def fetch(self, inputs, root):
        for index, given in enumerate(inputs):
            target = root / str(index) / "data"
            match [source for source in SOURCES if source in given]:
                case ["forge"]:
                    self.forge_input(given["forge"], target)
                case ["files"]:
                    self.files_input(given["files"], target)
                case ["blob"]:
                    self.blob_input(given["blob"], target)
                case other:
                    raise Refused(400, f"input {index} names {other}, not one source")
            shared(target)


def shared(target):
    for path in [target, *(target.rglob("*") if target.is_dir() else [])]:
        if path.is_symlink():
            continue
        mode = path.stat().st_mode
        runnable = path.is_dir() or mode & 0o100
        path.chmod(mode | (0o777 if runnable else 0o666))


def fetch_main():
    ducktape = shlex.split(os.environ.get("PODS_DUCKTAPE_CLI", "ducktape"))
    fetcher = Fetcher(ducktape, os.environ["PODS_NODE"], os.environ["PODS_FORGE"], Path(tempfile.mkdtemp()))
    try:
        fetcher.fetch(json.loads(os.environ["PODS_INPUTS"]), Path(IN_ROOT))
    except Refused as refusal:
        print(refusal.sentence, file=sys.stderr, flush=True)
        Path("/dev/termination-log").write_text(refusal.sentence)
        sys.exit(1)


def main():
    fetching = Fetching(
        image=os.environ.get("PODS_OUT_IMAGE", ""),
        node=os.environ.get("PODS_NODE", ""),
        forge=os.environ.get("PODS_FORGE", ""),
    )
    named = {"PODS_OUT_IMAGE": fetching.image, "PODS_NODE": fetching.node, "PODS_FORGE": fetching.forge}
    unset = [name for name, value in named.items() if not value]
    if unset:
        sys.exit(f"{', '.join(unset)} must be set: the image of each pod's fetch and out containers, the node they read inputs from, and its forge")
    kube = Kube(shlex.split(os.environ.get("PODS_KUBECTL", "kubectl")), os.environ.get("PODS_NAMESPACE", "agents"))
    signer = Signer(shlex.split(os.environ.get("PODS_DUCKTAPE", "ducktape")), Path(os.environ.get("PODS_KEYS", "/keys")))
    service = Service(kube, signer, fetching, Stats())
    threading.Thread(target=service.notifier.watch, daemon=True).start()
    host, port = os.environ.get("PODS_LISTEN", "0.0.0.0:8080").rsplit(":", 1)
    server = ThreadingHTTPServer((host, int(port)), handler(service))
    say("pods_serving", namespace=kube.namespace, listen=f"{host}:{port}", keyed=signer.held())
    server.serve_forever()


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["fetch"]:
            fetch_main()
        case []:
            main()
        case other:
            sys.exit(f"usage: server.py [fetch], not {' '.join(other)}")
