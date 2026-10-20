#!/usr/bin/env python3
# The pods service: agents' computing on the cluster through a network's egress, in one namespace: pods run with fetched inputs (tried again while the pod lives when a fetch loses its connection), exec answered whole or streamed while it runs, logs, a wait that streams until a pod ends, manifests applied and deleted, Kubernetes API calls passed through inside the namespace, outputs put as blobs or files, a pod's commit packed for a forge push, chat notices to the requester when work ends or a notified pod prints a `ducktape-notify` line, and usage; the reserved pod `desktop` is orthory's desktop build runner, reached by ssh through its `gate` command.
# ops/k8s/pods/pods.yaml runs it on fabrics-1, server-test.py tests it, and `server.py fetch` is each pod's input fetcher.
import base64
import binascii
import copy
import datetime
import decimal
import hashlib
import itertools
import json
import math
import os
import queue
import re
import secrets
import shlex
import shutil
import signal
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from decimal import Decimal
from fractions import Fraction
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

RUN = "run"
OUT = "out"
FETCH = "fetch"
OUT_PATH = "/out"
IN_ROOT = "/in"
CLAIM_CLASS = "local-path"
CLAIM_STORAGE = "1"
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
NOTIFY_LINE = "ducktape-notify "
RUN_LINE = "ducktape-run "
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
INPUT = {"path": str, "forge": dict, "files": dict, "blob": str, "scratch": dict}
SOURCES = ("forge", "files", "blob", "scratch")
SCRATCH = "scratch"
SCRATCH_SHAPE = {"medium": str, "size": str}
SCRATCH_MEDIUM = "Memory"
FORGE = {"repo": str, "commit": str}
FILES = {"path": str, "snapshot": str}
BLOB = {"path": str, "paths": list, "kind": str, "container": str}
FILE = {"path": str, "to": str, "message": str, "container": str}
EXEC = {"command": list, "container": str, "every": int}
PUSH = {"path": str, "ref": str, "have": list, "container": str}
APPLY = {"manifests": list, "requester": int}
DELETE = {"kind": str, "name": str}
LOGS = {"tail", "since", "container"}
WAIT = {"every"}
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
    "": Decimal(1),
    "n": Decimal("1e-9"),
    "u": Decimal("1e-6"),
    "m": Decimal("1e-3"),
    "k": Decimal("1e3"),
    "M": Decimal("1e6"),
    "G": Decimal("1e9"),
    "T": Decimal("1e12"),
    "P": Decimal("1e15"),
    "E": Decimal("1e18"),
    "Ki": Decimal(2**10),
    "Mi": Decimal(2**20),
    "Gi": Decimal(2**30),
    "Ti": Decimal(2**40),
    "Pi": Decimal(2**50),
    "Ei": Decimal(2**60),
}
NANO = SCALE["n"]
MOST = Decimal(2**63 - 1)
INT32 = 2**31
INT64 = 2**63
ENDED = ("Succeeded", "Failed")
REJECTED = {"unauthorized": 403, "not_found": 404, "already_exists": 409, "wrong_state": 409}
UNLANDED = "no block carried the frame before its anchor aged out"
CHAT = "chat"
FILES_PROGRAM = "files"
CHUNK = "chunk"
CHUNK_SIZE = 1 << 20
EGRESS = "egress"
PACK = "pack"
DROPPED = (
    "connection closed before message completed",
    "connection reset",
    "connection refused",
    "broken pipe",
    "error sending request",
    "error trying to connect",
    "error dialing backend",
    "unexpected eof",
    "i/o timeout",
    "tls handshake timeout",
    "use of closed network connection",
    "http2: client connection lost",
)
UNREACHED = (
    "the server is currently unable to handle the request",
    "apiserver not ready",
    "unable to connect to the server",
    "did you specify the right host or port",
    "error from server (serviceunavailable)",
    "error from server (internalerror)",
    "error from server (timeout)",
)
CUT = ("websocket: close 1006", "abnormal closure")
ENDED_ALREADY = "ended already"
UNSTARTED = re.compile(r'container not found \("([^"]+)"\)')
UNPLACED = "does not have a host assigned"
REQUEST_NUMBER = re.compile(r"[0-9]+")
KUBERNETES_QUANTITY = re.compile(r"([+-]?)([0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE]([+-]?[0-9]+)|(Ki|Mi|Gi|Ti|Pi|Ei|n|u|m|k|M|G|T|P|E))?")
OID = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
SAFE_GIT = "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'\n"
HAVING = """having() {
  echo "$1"
  shift
  for have in "$@"; do
    if git cat-file -e "$have^{commit}" 2>/dev/null; then echo "^$have"; fi
  done
  git for-each-ref --format='^%(objectname)' refs/remotes
}
"""
RESOLVING = (
    SAFE_GIT
    + HAVING
    + """cd "$1" || exit
commit=$(git rev-parse --verify --quiet "$2^{commit}") || exit
shift 2
git rev-list --parents -n 1 "$commit"
having "$commit" "$@" | git rev-list --stdin --count
"""
)
PACKING = (
    SAFE_GIT
    + HAVING
    + """cd "$1" || exit
new=$2
shift 2
having "$new" "$@" | git pack-objects --revs --stdout -q
"""
)
WATCH_PAUSE = 1
SERVICE_ACCOUNT = Path("/var/run/secrets/kubernetes.io/serviceaccount")
DESKTOP = "desktop"
BUILDS = "builds"
ADDRESSED = frozenset({"status", "log", "stop", "wait", "requeue"})
MACHINE_WIDE = frozenset({"busy", "bump", "prune", "jobs", "heavy-runs", "disk-targets", "queue-out", "queue-in", "self-update", "worktree", "measure", "claim", "ended"})
WEIGHTS = frozenset({"heavy", "light", "quiet", "lane", "quick"})
READ_ONLY = frozenset({"gates", "lint", "check", "lane", "lane-test", "test", "show", "grep", "schema-diff"})
STARTED = re.compile(rb"^run (\S+)$", re.M)
GO_TIME = re.compile(r"(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))? ([+-]\d{4})(?: \S+)?")
AHEAD = re.compile(rb"ahead (heavy|lane|quick|light) ([0-9]+)")
MOVABLE_RUN = re.compile(r"move (\S+) (heavy|lane|quick:\S+) ([0-9]+) (\S+)")
LANE_WAIT = re.compile(r"(heavy|lane|quick:\S+) ([0-9]+)")
REROUTE = "reroute"
DESKTOP_KEYS = "/desktop"
DESKTOP_FILES = ("id_ed25519", "known_hosts", "hosts", "user")
BUILD_SH = "/var/tmp/warm/build.sh"
SETTING = re.compile(r"[A-Z_][A-Za-z0-9_]*=.*", re.S)
UNPLAIN = frozenset("$`;|&<>()*?[~#\n")
RUNNER_DOWN = 69
SSH_FAILED = 255
UNCONNECTED = "ssh: connect to host "


class Unanswered(Exception):
    pass


def unanswered(why):
    return dropped(why) or re.search(r"\(5\d\d\)", why) is not None


class Refused(Exception):
    def __init__(self, status, sentence):
        super().__init__(sentence)
        self.status = status
        self.sentence = sentence


class Cut(Refused):
    def __init__(self, sentence, out):
        super().__init__(502, sentence)
        self.out = out


@dataclass
class Ran:
    code: int
    out: bytes
    err: str


@dataclass
class Answered:
    status: int
    kind: str
    body: bytes


@dataclass
class Stream:
    events: object


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


def dropped(text):
    lowered = text.lower()
    return any(words in lowered for words in DROPPED)


def ran_its_command(ran):
    return re.search(r"command terminated with exit code \d+", ran.err) is not None


def unreached(ran):
    lowered = ran.err.lower()
    return ran.code != 0 and not ran_its_command(ran) and any(words in lowered for words in UNREACHED)


def cut(ran):
    lowered = ran.err.lower()
    return ran.code != 0 and not ran_its_command(ran) and any(words in lowered for words in CUT)


# files refused a commit because its head moved after it was read: the base it named is a
# snapshot files has not minted on the node that applied it, or a later commit touched the path.
# Made again onto files' head as read then, it lands.
def moved_under(outcome, reason, sentence):
    if outcome != "rejected":
        return False
    return reason == "stale" or (reason == "not_found" and "minted no snapshot" in sentence)


def failed(ran):
    if ran.code == 0:
        return None
    return ran.err or ran.out.decode(errors="replace").strip() or f"exited {ran.code}"


@dataclass
class Unstarted:
    container: str | None


def unstarted(ran, container):
    if ran.code == 0:
        return None
    found = UNSTARTED.search(ran.err)
    if found:
        return Unstarted(found.group(1))
    if UNPLACED in ran.err:
        return Unstarted(container)
    return None


def default_container(pod):
    notes = pod.get("metadata", {}).get("annotations") or {}
    containers = pod.get("spec", {}).get("containers") or [{}]
    return notes.get(DEFAULT_CONTAINER) or containers[0].get("name")


def start_of(event, container):
    status = (event.get("object") or {}).get("status", {})
    state = state_of(status.get("containerStatuses", []), container)
    ended = event.get("type") == "DELETED" or status.get("phase") in ("Succeeded", "Failed")
    if ended or (state is not None and state["state"] == "terminated"):
        return "ended"
    return "running" if state is not None and state["state"] == "running" else "waiting"


def outcome(attempt):
    try:
        return attempt()
    except Refused as refusal:
        return refusal
    except Exception as error:
        return Refused(500, f"the pods service failed: {error!r}")


def exec_key(asked):
    return asked["chain"], asked["program"], asked["request"]


class Execution:
    def __init__(self, asked):
        self.asked = asked
        self.ended = threading.Event()
        self.result = None
        self.unswept = 0

    def end(self, result):
        self.result = result
        self.ended.set()

    def get(self, timeout=None):
        if not self.ended.wait(timeout):
            raise queue.Empty
        return self.result


def answered(result):
    if isinstance(result, Refused):
        raise result
    return result


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


CHAT_NOTIFY = 26
NOTICE_KIND = "pod"
STATUSES = {"note": 0, "running": 1, "passed": 2, "failed": 3}
RUN_ENDED = re.compile(r"\bended: (?:sh |shot )?exit (\d+)")
RUN_LOST = " lost with pod "
RUN_NAMED = re.compile(rf"(?:run )?(\S+)(?: ended: |{RUN_LOST})")


def concerning(account):
    return [] if account is None else [account]


def notice_message(channel, message_id, thread, said):
    concerns = [u64(account) for account in said["concerns"]]
    notice = (
        borsh_text(NOTICE_KIND)
        + borsh_text(said["group"])
        + borsh_text(said["subject"])
        + borsh_option(None, borsh_text)
        + u8(STATUSES[said["status"]])
        + borsh_text(said["detail"])
        + borsh_vec(concerns)
    )
    return u8(CHAT_NOTIFY) + borsh_text(channel) + borsh_text(message_id) + borsh_option(thread, u64) + notice


def line_status(text):
    ended = RUN_ENDED.search(text)
    if ended is not None:
        return "passed" if ended.group(1) == "0" else "failed"
    return "failed" if RUN_LOST in text else "note"


def line_group(name, text):
    run = RUN_NAMED.match(text)
    return name if run is None else f"{name}:{run.group(1)}"


def line_said(name, text, concerned):
    return {"group": line_group(name, text), "subject": name, "status": line_status(text), "detail": text, "concerns": concerning(concerned)}


def chat_channel(channel):
    return u8(1) + borsh_text(channel)


HELD = "held"


def chat_message(message_id):
    return u8(2) + borsh_text(message_id)


def message_held(reply):
    reader = Reader(reply)
    if reader.u8() != 2:
        raise Refused(502, "chat answered a message id with something else")
    return reader.u8() == 1


def chat_members(channel, after):
    return u8(7) + borsh_text(channel) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def principal_of(reader):
    return reader.u64() if reader.u8() == 0 else None


def room_of(reply):
    reader = Reader(reply)
    if reader.u8() != 1:
        raise Refused(502, "chat answered a channel with something else")
    if reader.u8() == 0:
        return None
    reader.bytes()
    reader.bytes()
    reader.u64()
    members_only = reader.u8() == 1
    owner = principal_of(reader)
    return {"members_only": members_only, "owner": owner, "archived": reader.u8() == 1}


def members_of(reply):
    reader = Reader(reply)
    if reader.u8() != 7:
        raise Refused(502, "chat answered a channel's members with something else")
    reader.u64()
    accounts = []
    for _ in range(int.from_bytes(reader.take(4), "little")):
        accounts.append(principal_of(reader))
        reader.u64()
        reader.u64()
    return accounts, reader.bytes() if reader.u8() == 1 else None


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


def files_commit_of(message, path, size, chunks):
    return lambda base: files_commit(base, message, path, size, chunks)


def files_commit(base, message, path, size, chunks):
    content = u8(1) + u64(size) + borsh_vec([blob_id(chunk) for chunk in chunks])
    put = u8(0) + borsh_text(path) + u8(0) + (0).to_bytes(4, "little") + content
    return u8(1) + borsh_option(base, blob_id) + borsh_text(message) + borsh_vec([put])


def egress_request(request):
    return u8(3) + u64(request)


def open_of(reply):
    reader = Reader(reply)
    if reader.u8() != 3:
        raise Refused(502, "egress answered a request with something else")
    return reader.u8() == 1


def objects_of(pack):
    if pack[:4] != b"PACK" or len(pack) < 12:
        raise Refused(502, "git pack-objects wrote no pack")
    return int.from_bytes(pack[8:12], "big")


def receipt(ran):
    text = ran.out.decode(errors="replace").strip()
    pooled = re.match(r"pooled ([0-9a-f]+)\n?", text)
    verdict = text[pooled.end():] if pooled else text
    applied = re.match(r"landed (\d+) applied ?([0-9a-f]*)", verdict)
    rejected = re.match(r"landed (\d+) rejected (\w+): (.*)", verdict, re.S)
    unlanded = re.fullmatch(r"unlanded", verdict)
    why = ran.err or text or f"ducktape exited {ran.code}"
    if ran.code == 0 and applied:
        return "applied", applied.group(2), "", int(applied.group(1))
    if ran.code == 0 and rejected:
        return "rejected", rejected.group(2), rejected.group(3).strip(), int(rejected.group(1))
    if ran.code == 0 and unlanded:
        return "unlanded", "", UNLANDED, None
    if pooled:
        return "unknown", "", f"the node took it as {pooled.group(1)}, but whether a block carried it is not known: {why}", None
    return "failed", "", why, None


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

    def watch_pod(self, name):
        argv = self.argv("get", "pod", name, "--watch", "--output-watch-events", "--output", "json")
        return subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def follow(self, name):
        argv = self.argv("logs", name, "--container", RUN, "--follow")
        return subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)


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
            return run([*self.ducktape, "submit", program, str(file), "--land"], env=self.env())

    def query(self, program, request):
        return run([*self.ducktape, "query", program, "--raw"], stdin=request, env=self.env())


def plain_words(script):
    quote = None
    for char in script:
        if quote is not None:
            if char == quote:
                quote = None
            elif quote == '"' and char in '$`\\':
                return None
        elif char in "'\"":
            quote = char
        elif char in UNPLAIN:
            return None
    try:
        return shlex.split(script)
    except ValueError:
        return None


def desktop_words(command, name=DESKTOP):
    words = plain_words(command[2]) if len(command) == 3 and command[:2] == ["sh", "-c"] else command
    settings = list(itertools.takewhile(SETTING.fullmatch, words or []))
    called = (words or [])[len(settings) :]
    if called[:2] != ["sh", BUILD_SH]:
        raise Refused(
            400,
            f'pod {name} runs build.sh alone: the command ["sh", "{BUILD_SH}", ...], or a script of plain words that calls it (KEY=VALUE words first; no expansion, operator or comment outside quotes), not {shlex.join(command)}',
        )
    return settings + called[2:]


def desktop_view(said):
    running = {"state": "running", "reason": None, "message": None, "exit_code": None, "started": said.get("started"), "finished": None}
    container = {"name": RUN, "image": None, "command": ["sh", BUILD_SH], "args": [], "workdir": None, "resources": {}, "status": running}
    return {
        "name": DESKTOP,
        "created": said.get("started"),
        "phase": "Running",
        "reason": None,
        "message": None,
        "unscheduled": None,
        "resources": {},
        "containers": [container],
        "init": [],
        "inputs": [],
        "asked": {name: None for name in ASKED},
        "requester": None,
        "notify": None,
        "runner": said,
    }


def desktop_free(said):
    busy, ran = said.get("busy"), said.get("busy_exit")
    if not isinstance(busy, list) or not all(isinstance(line, str) for line in busy):
        return False, f"the desktop's ping has busy as a {type(busy).__name__}, not a list of lines"
    if type(ran) is not int:
        return False, f"the desktop's ping has busy_exit as a {type(ran).__name__}, not a number"
    if ran != 0:
        return False, f"the desktop's build.sh busy exited {ran}"
    kinds = ["stray" if line.startswith("stray ") else (line.split()[2:3] or [""])[0] for line in busy]
    if any(kind not in WEIGHTS | {"stray"} for kind in kinds):
        return False, "a line of the desktop's busy names no weight build.sh prints"
    if "stray" in kinds:
        return False, "a process no run started holds one of the desktop's build locks"
    if "quiet" in kinds:
        return False, "a quiet run holds every heavy slot of the desktop"
    return True, None


def notifies(words):
    try:
        said = base64.b64decode(words, validate=True).split(b"\0")
    except (binascii.Error, ValueError):
        return False
    return any(word.startswith(b"NOTIFY=") and word != b"NOTIFY=" for word in said)


def go_time(text):
    found = GO_TIME.fullmatch(text) if isinstance(text, str) else None
    if found is None:
        return None
    clock, fraction, offset = found.groups()
    whole = datetime.datetime.strptime(f"{clock} {offset}", "%Y-%m-%d %H:%M:%S %z").timestamp()
    return whole + int((fraction or "0").ljust(9, "0")) / 1e9


def on_machine(machine, ran, started=True):
    out = STARTED.sub(lambda found: b"run " + machine.encode() + b":" + found.group(1), ran.out) if started else ran.out
    return Ran(ran.code, out, f"{BUILDS}: on {machine}\n{ran.err}".rstrip("\n"))


def gate_run(argv):
    done = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True)
    return Ran(done.returncode, done.stdout, done.stderr.decode(errors="replace").strip())


def gate_spawn(argv):
    return subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


class Desktop:
    def __init__(self, keys, home=None, runner=gate_run, spawn=gate_spawn):
        self.keys = keys
        self.home = home
        self.runner = runner
        self.spawn = spawn
        self.lock = threading.Lock()
        self.reached = None
        self.key = None

    def held(self):
        return all((self.keys / name).is_file() for name in DESKTOP_FILES)

    def hosts(self):
        listed = (self.keys / "hosts").read_text().split()
        with self.lock:
            first = self.reached
        return sorted(listed, key=lambda host: host != first)

    def identity(self):
        with self.lock:
            if self.key is None:
                home = self.home or Path(tempfile.mkdtemp(prefix="desktop-"))
                home.mkdir(parents=True, exist_ok=True)
                key = home / "id_ed25519"
                with os.fdopen(os.open(key, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "wb") as file:
                    file.write((self.keys / "id_ed25519").read_bytes())
                self.key = key
            return self.key

    def argv(self, host, words, alive):
        user = (self.keys / "user").read_text().strip()
        options = ["IdentitiesOnly=yes", "BatchMode=yes", "StrictHostKeyChecking=yes", f"UserKnownHostsFile={self.keys / 'known_hosts'}"]
        if alive is not None:
            options.append(f"ServerAliveInterval={alive}")
        flags = [part for option in options for part in ("-o", option)]
        return ["ssh", "-T", "-F", "/dev/null", *flags, "-i", str(self.identity()), f"{user}@{host}", json.dumps(words)]

    def call(self, words, alive=None, again=True):
        if not self.held():
            raise Refused(503, f"this service holds no key for pod {DESKTOP}, so the desktop runner is not reachable")
        failures = []
        for host in self.hosts():
            ran = self.runner(self.argv(host, words, alive))
            if ran.code == SSH_FAILED and not again and not ran.err.startswith(UNCONNECTED):
                raise Cut(f"the exec's connection to pod {DESKTOP} at {host} was cut while it ran ({ran.err or 'ssh exited 255'}); its command may still be running there, so it was not run again", ran.out)
            if ran.code == SSH_FAILED:
                failures.append(f"{host}: {ran.err or 'ssh exited 255'}")
                continue
            with self.lock:
                self.reached = host
            if ran.code == RUNNER_DOWN:
                raise Refused(503, f"the desktop's runner is down: {ran.err}")
            return ran
        raise Refused(503, f"the desktop is unreachable: {'; '.join(failures) or 'its hosts file names no host'}")

    def ran(self, command, alive=None):
        match command:
            case ["cat", "--", path]:
                return self.call(["get", path])
            case ["sh", "-c", script, "resolve", *words] if script == RESOLVING:
                return self.call(["resolve", *words])
            case ["sh", "-c", script, "pack", *words] if script == PACKING:
                return self.call(["pack", *words])
            case _:
                return self.call(["exec", *desktop_words(command)], alive, again=False)

    def state(self):
        ran = self.call(["ping"])
        try:
            said = json.loads(ran.out)
        except json.JSONDecodeError as error:
            raise Refused(502, f"the desktop's ping answered {ran.out.decode(errors='replace').strip()!r}, not JSON") from error
        if ran.code != 0 or not said.get("up"):
            raise Refused(503, f"the desktop's runner is not up: {said.get('state') or ran.err or f'ping exited {ran.code}'}")
        return said

    def logs(self, options):
        kept = [option for option in options if option.startswith(("--tail=", "--since="))]
        ran = self.call(["logs", *kept])
        if ran.code != 0:
            raise Refused(502, f"the desktop's logs were not read: {ran.err or f'gate exited {ran.code}'}")
        return ran.out

    def follow(self, alive):
        self.state()
        with self.lock:
            host = self.reached
        return self.spawn(self.argv(host, ["logs", "--tail=0", "--follow"], alive))

    def tail(self, since):
        self.state()
        with self.lock:
            host = self.reached
        return self.spawn(self.argv(host, ["logs", f"--since={since}s", "--follow"], None))


def heard_in(stream, heard):
    for raw in stream:
        heard[:] = [raw]
        yield raw


def block_ms_of(data):
    reader = Reader(data)
    reader.bytes()
    reader.u64()
    return reader.u64()


def node_block_ms(node):
    asked = urllib.request.Request(f"{node.rstrip('/')}/v1/status", headers={"User-Agent": "pods"})
    with urllib.request.urlopen(asked) as answer:
        return block_ms_of(answer.read())


class Stats:
    def __init__(self, mount=SERVICE_ACCOUNT):
        self.mount = mount

    def summary(self, host):
        token = (self.mount / "token").read_text().strip()
        context = ssl.create_default_context(cafile=str(self.mount / "ca.crt"))
        request = urllib.request.Request(f"https://{host}:10250/stats/summary", headers={"Authorization": f"Bearer {token}"})
        with urllib.request.urlopen(request, context=context) as answer:
            return json.load(answer)


class Api:
    def __init__(self, mount=SERVICE_ACCOUNT, environ=os.environ):
        self.mount = mount
        self.environ = environ

    def call(self, method, path, query, body, kind):
        token = (self.mount / "token").read_text().strip()
        context = ssl.create_default_context(cafile=str(self.mount / "ca.crt"))
        host = self.environ["KUBERNETES_SERVICE_HOST"]
        bracketed = f"[{host}]" if ":" in host else host
        target = f"https://{bracketed}:{self.environ['KUBERNETES_SERVICE_PORT']}{path}" + (f"?{query}" if query else "")
        headers = {"Authorization": f"Bearer {token}", **({"Content-Type": kind} if kind else {})}
        request = urllib.request.Request(target, data=body or None, method=method, headers=headers)
        try:
            with urllib.request.urlopen(request, context=context) as answer:
                return Answered(answer.status, answer.headers.get("content-type") or "application/json", answer.read())
        except urllib.error.HTTPError as refusal:
            return Answered(refusal.code, refusal.headers.get("content-type") or "application/json", refusal.read())
        except urllib.error.URLError as error:
            raise Refused(502, f"the Kubernetes API was not reached: {error.reason}") from error


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


def request_number(text):
    if not REQUEST_NUMBER.fullmatch(text):
        return None
    number = int(text)
    return number if number < 1 << 64 else None


def asked_by(headers):
    asked = {name: headers.get(f"ducktape-{name}") for name in ASKED}
    missing = [f"ducktape-{name}" for name, value in asked.items() if not value]
    if missing:
        raise Refused(400, f"this service answers a network's egress calls only, and {', '.join(missing)} is missing")
    if request_number(asked["request"]) is None:
        raise Refused(400, f"ducktape-request {asked['request']!r} is not an egress request number, so this service could not ask egress about it")
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
        case "scratch":
            scratch = named(checked(given["scratch"], SCRATCH_SHAPE, f"{what}'s scratch"), ["medium"], f"{what}'s scratch")
            if scratch["medium"] != SCRATCH_MEDIUM:
                raise Refused(400, f"{what}'s scratch medium is {SCRATCH_MEDIUM}: a scratch input lives in the pod's memory, never on the node's disk")
            if "size" in scratch:
                scratch_size(scratch["size"], f"{what}'s scratch size")
    return given


def scratch_size(text, what):
    found = KUBERNETES_QUANTITY.fullmatch(text.strip())
    if found is None:
        raise Refused(400, f"{what} {text!r} is not a Kubernetes quantity such as 48Gi")
    exponent = found.group(3)
    power = None if exponent is None else parsed_exponent(exponent)
    if exponent is not None and (power is None or not -INT32 <= power < INT32):
        raise Refused(400, f"{what} {text!r} has an exponent outside -2^31..2^31-1, the int32 Kubernetes reads it into")
    sign, digits = found.group(1), found.group(2)
    above_zero = sign != "-" and digits.strip("0.") != ""
    if not above_zero:
        raise Refused(400, f"{what} {text!r} is not above 0: Kubernetes refuses a negative sizeLimit and bounds nothing by one of 0")
    return text


def scratch_warning(given, requested):
    risk = "memory scratch counts as pod memory, so the pod can be evicted."
    scratch = given[SCRATCH]
    if "size" not in scratch:
        return f"scratch {given['path']} has no size, so it can grow past the {requested} memory request; {risk}"
    if quantity(scratch["size"]) > quantity(requested):
        return f"scratch {given['path']} is {scratch['size']} on a {requested} memory request; {risk}"
    return None


def limit_warnings(scratches, limited):
    starved = f"the kubelet sizes a memory scratch to the {limited} limit at most, and a full one leaves the pod's processes no memory, so they are OOM-killed."
    unsized = [f"scratch {given['path']} has no size, so it can fill the {limited} memory limit; {starved}" for given in scratches if "size" not in given[SCRATCH]]
    sized = [given for given in scratches if "size" in given[SCRATCH]]
    total = sum(quantity(given[SCRATCH]["size"]) for given in sized)
    if not sized or total < quantity(limited):
        return unsized
    held = " + ".join(f"{given['path']} {given[SCRATCH]['size']}" for given in sized)
    return unsized + [f"scratch {held} is not below the {limited} memory limit; {starved}"]


def scratch_warnings(spec):
    resources = spec.get("resources") or {}
    requested = (resources.get("requests") or {}).get("memory")
    limited = (resources.get("limits") or {}).get("memory")
    scratches = [given for given in spec.get("inputs", []) if SCRATCH in given]
    warnings = [scratch_warning(given, requested) for given in scratches] if requested is not None else []
    warnings += limit_warnings(scratches, limited) if limited is not None else []
    return [warning for warning in warnings if warning is not None]


def warned(answer, spec):
    warnings = scratch_warnings(spec)
    if not warnings:
        return answer
    return {**answer, "warnings": warnings}


def confined():
    return {"allowPrivilegeEscalation": False, "seccompProfile": {"type": "RuntimeDefault"}}


@dataclass
class Fetching:
    image: str
    node: str
    forge: str
    block_ms: int


def resources_of(given):
    checked(given, RESOURCES, "resources")
    return {group: dict(checked(given[group], AMOUNTS, f"resources' {group}")) for group in RESOURCES if group in given}


def claimed(name, labels):
    template = {
        "metadata": {"labels": labels},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": CLAIM_CLASS,
            "resources": {"requests": {"storage": CLAIM_STORAGE}},
        },
    }
    return {"name": name, "ephemeral": {"volumeClaimTemplate": template}}


def input_mount(index, given):
    if SCRATCH in given:
        return {"name": f"in-{index}", "mountPath": given["path"]}
    return {"name": f"in-{index}", "mountPath": given["path"], "subPath": "data"}


def input_volume(index, given, labels):
    if SCRATCH in given:
        scratch = given[SCRATCH]
        sized = {"sizeLimit": scratch["size"]} if "size" in scratch else {}
        return {"name": f"in-{index}", "emptyDir": {"medium": scratch["medium"], **sized}}
    return claimed(f"in-{index}", labels)


def pod_spec(asked, spec, fetching, namespace):
    named(spec, ["image"], "a pod")
    env = spec.get("env", {})
    if not all(isinstance(value, str) for value in env.values()):
        raise Refused(400, "env maps names to strings")
    inputs = [input_of(given, index) for index, given in enumerate(spec.get("inputs", []))]
    mounts = [{"name": OUT, "mountPath": OUT_PATH}] + [input_mount(index, given) for index, given in enumerate(inputs)]
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
        "volumeMounts": mounts,
        "securityContext": confined(),
    }
    body = {
        "restartPolicy": "Never",
        "serviceAccountName": AGENT_ACCOUNT,
        "automountServiceAccountToken": False,
        "enableServiceLinks": False,
        "hostUsers": False,
        "priorityClassName": PRIORITY,
        "shareProcessNamespace": True,
        "containers": [run_container, out_container],
    }
    if "resources" in spec:
        body["resources"] = resources_of(spec["resources"])
    labels = {**dict([MANAGED]), **labels_for(asked, spec.get("requester"))}
    body["volumes"] = [claimed(OUT, labels)] + [input_volume(index, given, labels) for index, given in enumerate(inputs)]
    if inputs:
        body["initContainers"] = [fetch_container(inputs, fetching)]
    return {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": f"{asked['chain']}-{asked['request']}",
            "namespace": namespace,
            "labels": labels,
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
            {"name": "PODS_BLOCK_MS", "value": str(fetching.block_ms)},
        ],
        "volumeMounts": [{"name": f"in-{index}", "mountPath": f"{IN_ROOT}/{index}"} for index, given in enumerate(inputs) if SCRATCH not in given],
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


def confine_manifest(manifest, what, cluster_kinds, namespace, labels):
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


@dataclass
class Target:
    path: str
    resource: str
    subresource: str | None


def target_of(segments, namespace):
    path = "/" + "/".join(segments)
    outside = Refused(
        403,
        f"{path} is not inside namespace {namespace}: /v1/k8s/ passes through api/v1/namespaces/{namespace}/... and apis/GROUP/VERSION/namespaces/{namespace}/...",
    )
    escaping = any(segment in (".", "..") or "%" in segment for segment in segments)
    match segments:
        case ["api", _, "namespaces", named, resource, *rest] | ["apis", _, _, "namespaces", named, resource, *rest]:
            inside = named == namespace and not escaping
        case _:
            raise outside
    if not inside:
        raise outside
    return Target(path, resource, rest[1] if len(rest) > 1 else None)


def forced_holds(manifest):
    specs = [spec for _, spec in pod_specs(manifest)]
    pods_hold = all(spec.get("priorityClassName") == PRIORITY and spec.get("automountServiceAccountToken") is False for spec in specs)
    account_holds = manifest.get("kind") != "ServiceAccount" or manifest.get("automountServiceAccountToken") is False
    return pods_hold and account_holds


def dry_run(query):
    return "&".join([*([query] if query else []), "dryRun=All"])


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


def notify_lines(name, stream):
    for raw in stream:
        line = raw.decode(errors="replace").rstrip("\r\n")
        if not line.startswith(NOTIFY_LINE):
            continue
        channel, _, text = line[len(NOTIFY_LINE):].partition(" ")
        if channel and text.strip():
            yield channel, text.strip()


def lost_runs(name, stream):
    armed, done = {}, set()
    for raw in stream:
        line = raw.decode(errors="replace").rstrip("\r\n")
        if not line.startswith(RUN_LINE):
            continue
        words = line[len(RUN_LINE):].split(" ", 3)
        if words[0] == "armed" and len(words) >= 3 and words[1] and words[2]:
            armed[words[2]] = (words[1], words[3].strip() if len(words) > 3 else "")
        elif words[0] == "done" and len(words) >= 2:
            done.add(words[1].strip())
    for run, (channel, call) in armed.items():
        if run not in done:
            again = f"; call it again: {call}" if call else "; call it again"
            yield channel, f"run {run}{RUN_LOST}{name}: it ended before the run did, so nothing of it runs now{again}"


def line_id(name, channel, text):
    return notify_id(name, channel, f"{name} \u00b7 {text}")


def notify_id(name, channel, line):
    digest = hashlib.sha256(f"{channel}\n{line}".encode()).hexdigest()
    return f"pods-{name}-notify-{digest}"


def ended_said(pod):
    line = ended_line(pod)
    if line is None:
        return None
    name = pod["metadata"]["name"]
    ran = state_of(pod.get("status", {}).get("containerStatuses", []), RUN)
    passed = ran is not None and ran["state"] == "terminated" and ran["exit_code"] == 0
    detail = line.removeprefix(f"pod {name} ")
    requester = requester_of(pod["metadata"].get("labels") or {})
    return {"group": name, "subject": f"pod {name}", "status": "passed" if passed else "failed", "detail": detail, "concerns": concerning(requester)}


def ended_event(pod):
    line = ended_line(pod)
    if line is None:
        return None
    ran = state_of(pod.get("status", {}).get("containerStatuses", []), RUN)
    run_ended = ran is not None and ran["state"] == "terminated"
    return {"line": line, "exit_code": ran["exit_code"] if run_ended else None, "pod": view(pod)}


def watch_ended(name, event):
    pod = event.get("object") or {}
    match event.get("type"):
        case "DELETED":
            return ended_event(pod) or {"line": f"pod {name} was deleted", "exit_code": None, "pod": view(pod)}
        case "ADDED" | "MODIFIED":
            return ended_event(pod)
        case _:
            return None


def from_one(value, field):
    if value < 1:
        raise Refused(400, f"{field} is a whole number of seconds from 1")
    return value


def wait_every(query):
    unknown = sorted(set(query) - WAIT)
    if unknown:
        raise Refused(400, f"a wait takes no {', '.join(unknown)}; it takes every")
    if "every" not in query:
        raise Refused(400, "a wait names every: the seconds between the alive events that keep the caller's request from expiring")
    value = query["every"][-1]
    if not value.isdigit():
        raise Refused(400, "every is a whole number of seconds from 1")
    return from_one(int(value), "every")


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


def streamed(events):
    try:
        yield from events
    except Exception as error:
        say("pods_failed", reason="stream", detail=json.dumps(repr(error)))
        yield sse("refused", {"status": 500, "error": f"the pods service failed while it streamed: {error!r}"})


def chunk(data):
    return f"{len(data):x}\r\n".encode() + data + b"\r\n"


LAST_CHUNK = b"0\r\n\r\n"


class Beat:
    def __init__(self, every):
        self.every = every
        self.due = time.monotonic() + every

    def heard(self, events):
        while True:
            try:
                return events.get(timeout=max(self.due - time.monotonic(), 0))
            except queue.Empty:
                self.due += self.every
                yield sse("alive", {})


def relay(process, events):
    with process.stdout:
        for event in json_stream(process.stdout):
            events.put(("event", event))
    events.put(("closed", process.wait()))


def reached(name, ran):
    answered = re.search(r"command terminated with exit code (\d+)", ran.err)
    server_refused = ran.code != 0 and answered is None and "Error from server" in ran.err
    if server_refused:
        raise kube_refusal(ran, f"pod {name} was not reached")
    return re.sub(r"\n?command terminated with exit code \d+\s*$", "", ran.err)


def git_output(name, ran, doing):
    stderr = reached(name, ran)
    if ran.code != 0:
        raise Refused(422, f"{doing}: {stderr.strip() or f'git exited {ran.code}'}")
    return ran.out


def exec_answer(name, ran):
    stderr = reached(name, ran)
    return {"exit_code": ran.code, **text_or_hex(ran.out, "stdout"), **text_or_hex(stderr.encode(), "stderr")}


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


def parsed_exponent(text):
    digits = text.lstrip("+-").lstrip("0") or "0"
    if len(digits) > len(str(INT64)):
        return None
    value = -int(digits) if text.startswith("-") else int(digits)
    return value if -INT64 <= value < INT64 else None


def quantity(text):
    found = KUBERNETES_QUANTITY.fullmatch(str(text).strip())
    if found is None:
        raise Refused(502, f"{text} is not a Kubernetes quantity")
    sign, digits, exponent, suffix = found.groups()
    power = parsed_exponent(exponent or "0")
    if power is None:
        raise Refused(502, f"{text} is not a Kubernetes quantity: its exponent is past int64")
    power = (power + INT32) % (2 * INT32) - INT32
    amount = Decimal(f"{digits}e{power}")
    scale = SCALE[suffix or ""]
    if amount.is_zero():
        return Fraction(0)
    if amount.adjusted() + min(SCALE.values()).adjusted() > MOST.adjusted():
        value = MOST
    elif amount.adjusted() + max(SCALE.values()).adjusted() + 2 <= NANO.adjusted():
        value = NANO
    else:
        exact = decimal.Context(prec=decimal.MAX_PREC, Emax=decimal.MAX_EMAX, Emin=decimal.MIN_EMIN, rounding=decimal.ROUND_UP)
        value = min(exact.multiply(amount, scale).quantize(NANO, context=exact), MOST)
    return Fraction(-value if sign == "-" else value)


def pod_limit(spec, resource):
    stated = ((spec.get("resources") or {}).get("limits") or {}).get(resource)
    if stated is not None:
        return quantity(stated)
    amount = lambda container: quantity(((container.get("resources") or {}).get("limits") or {}).get(resource, "0"))
    running = sum(amount(container) for container in spec.get("containers") or [])
    starting = max((amount(container) for container in spec.get("initContainers") or []), default=0)
    return max(running, starting)


def volume_disks(pods, summaries, namespace):
    ephemeral = {pod["metadata"]["name"]: {volume["name"] for volume in (pod.get("spec") or {}).get("volumes") or [] if "ephemeral" in volume} for pod in pods}
    disks = []
    for summary in summaries:
        held = [
            volume
            for entry in summary.get("pods", [])
            if entry.get("podRef", {}).get("namespace") == namespace
            for volume in entry.get("volume") or []
            if volume.get("name") in ephemeral.get(entry.get("podRef", {}).get("name"), set())
        ]
        if not held:
            continue
        node = (summary.get("node") or {}).get("nodeName")
        disks.append({"node": node, "used_bytes": held[0].get("usedBytes") or 0, "capacity_bytes": held[0].get("capacityBytes") or 0})
    return disks


def made_for_a_pod(claim):
    owners = (claim.get("metadata") or {}).get("ownerReferences") or []
    return any(owner.get("kind") == "Pod" and owner.get("controller") for owner in owners)


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
                "cpu_limit": float(pod_limit(pod.get("spec", {}), "cpu")) if live else 0,
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
        if not made_for_a_pod(claim)
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
    return {"quota": quota, "requesters": ordered, "pods": rows, "claims": held, "volume_disks": volume_disks(pods, summaries, namespace)}


class Notifier:
    def __init__(self, kube, signer, retry_pause):
        self.kube = kube
        self.signer = signer
        self.following = set()
        self.posted = {}
        self.lock = threading.Lock()
        self.spawn = lambda target, *args: threading.Thread(target=target, args=args, daemon=True).start()
        self.pause = time.sleep
        self.retry_pause = retry_pause

    def post(self, notes, requester, message_id, said, unvouched=False):
        notify = notify_of(notes)
        if notify is None:
            return False
        if not self.signer.held():
            say("pods_unposted", message=message_id, reason="no_key")
            return False
        pause = self.retry_pause
        attempts = 0
        while True:
            try:
                return self.post_once(notify, requester, message_id, said, unvouched)
            except Unanswered as why:
                attempts += 1
                if attempts & (attempts - 1) == 0:
                    say("pods_retried", attempts=attempts, reason="node_unanswered", message=message_id, detail=json.dumps(str(why)))
                self.pause(pause)
                pause *= 2

    def post_once(self, notify, requester, message_id, said, unvouched):
        try:
            held = message_held(self.asked(chat_message(message_id)))
        except Refused:
            held = False
        if held:
            return HELD
        refused = self.refusal(notify["channel"], requester, unvouched)
        if refused is not None:
            reason, detail = refused
            fields = {} if detail is None else {"detail": json.dumps(detail)}
            say("pods_unposted", message=message_id, reason=reason, **fields)
            return False
        payload = notice_message(notify["channel"], message_id, notify["thread"], said)
        outcome, reason, sentence, _ = receipt(self.signer.submit(CHAT, payload))
        posted = outcome == "applied" or (outcome == "rejected" and reason == "already_exists")
        if posted:
            say("pods_posted", message=message_id)
            return True
        if outcome in ("unknown", "unlanded") or (outcome == "failed" and unanswered(sentence)):
            raise Unanswered(sentence)
        say("pods_unposted", message=message_id, reason=reason or outcome, detail=json.dumps(sentence))
        return False

    def asked(self, request):
        ran = self.signer.query(CHAT, request)
        if ran.code != 0:
            why = ran.err or ran.out.decode(errors="replace") or f"ducktape exited {ran.code}"
            if unanswered(why):
                raise Unanswered(why)
            raise Refused(502, why)
        return ran.out

    def refusal(self, channel, requester, unvouched=False):
        if requester is None and not unvouched:
            return "no_requester", None
        try:
            room = room_of(self.asked(chat_channel(channel)))
            if room is None:
                return "no_channel", None
            if room["archived"]:
                return "archived", None
            if not room["members_only"] or (requester is not None and room["owner"] == requester):
                return None
            if requester is None:
                return "members_only", None
            after = None
            while True:
                accounts, after = members_of(self.asked(chat_members(channel, after)))
                if requester in accounts:
                    return None
                if after is None:
                    return "not_a_member", None
        except Refused as refused:
            return "chat_unasked", refused.sentence

    def passed(self, name, said):
        asked = 0
        for newest in range(len(said) - 1, -1, -1):
            channel, text = said[newest]
            asked += 1
            try:
                held = message_held(self.asked(chat_message(line_id(name, channel, text))))
            except Refused as refused:
                return 0, asked, refused.sentence
            except Unanswered as why:
                return 0, asked, str(why)
            if held:
                with self.lock:
                    self.posted.setdefault(name, set()).update(line_id(name, channel, text) for channel, text in said[: newest + 1])
                return newest + 1, asked, None
        return 0, asked, None

    def lines(self, name, requester, stream, unvouched=False, concerned=None):
        return self.each(name, requester, notify_lines(name, stream), unvouched, concerned)

    def each(self, name, requester, said, unvouched=False, concerned=None):
        concerned = requester if concerned is None else concerned
        held = 0
        for channel, text in said:
            message_id = line_id(name, channel, text)
            with self.lock:
                if message_id in self.posted.get(name, ()):
                    continue
            posted = self.post({NOTIFY_CHANNEL: channel}, requester, message_id, line_said(name, text, concerned), unvouched)
            if posted:
                held += posted is HELD
                with self.lock:
                    self.posted.setdefault(name, set()).add(message_id)
        return held

    def follow(self, name, requester):
        try:
            process = self.kube.follow(name)
            self.lines(name, requester, process.stdout)
            process.wait()
        finally:
            with self.lock:
                self.following.discard(name)

    def read(self, name, requester):
        ran = self.kube.logs(name, ["--container", RUN])
        if ran.code == 0:
            logged = ran.out.splitlines(keepends=True)
            self.lines(name, requester, logged)
            self.each(name, requester, lost_runs(name, logged))

    def followed(self, pod):
        meta = pod["metadata"]
        if NOTIFY_CHANNEL not in (meta.get("annotations") or {}):
            return
        ran = state_of(pod.get("status", {}).get("containerStatuses", []), RUN)
        if ran is None or ran["state"] != "running":
            return
        name = meta["name"]
        with self.lock:
            if name in self.following:
                return
            self.following.add(name)
        self.spawn(self.follow, name, requester_of(meta.get("labels") or {}))

    def seen(self, pod):
        self.followed(pod)
        meta = pod["metadata"]
        notes = meta.get("annotations") or {}
        ended = ended_said(pod)
        unposted = ended is not None and NOTIFIED not in notes and NOTIFY_CHANNEL in notes
        if not unposted:
            return
        name = meta["name"]
        requester = requester_of(meta.get("labels") or {})
        self.read(name, requester)
        if self.post(notes, requester, f"pods-{name}-ended", ended):
            self.kube.annotate(name, NOTIFIED, "ended")
            with self.lock:
                self.posted.pop(name, None)

    def event(self, event):
        kind = event.get("type")
        if kind in ("ADDED", "MODIFIED"):
            self.seen(event["object"])
        elif kind == "DELETED":
            with self.lock:
                self.posted.pop(event["object"]["metadata"]["name"], None)

    def watch(self):
        failures = 0
        while True:
            process = self.kube.watch()
            for event in json_stream(process.stdout):
                self.event(event)
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
    def __init__(self, kube, signer, fetching, stats, api, desktop=None, warm_pod=None):
        self.kube = kube
        self.warm_pod = warm_pod
        self.desktop = desktop or Desktop(Path(DESKTOP_KEYS))
        self.desktop_wanted = threading.Event()
        self.desktop_follower = None
        self.rest = self.desktop_wanted.wait
        self.retry_pause = fetching.block_ms / 1000
        self.clock = time.monotonic
        self.wall = time.time
        self.signer = signer
        self.fetching = fetching
        self.stats = stats
        self.api = api
        self.notifier = Notifier(kube, signer, self.retry_pause)
        self.pause = time.sleep
        self.execs = {}
        self.lock = threading.Lock()
        self.swept = threading.Event()
        self.rerouting = threading.Lock()
        self.launched = {}

    def pod(self, name):
        ran = self.kube.get(name)
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name} was not read")
        return json.loads(ran.out)

    def keyed(self, doing):
        if not self.signer.held():
            raise Refused(503, f"this service holds no key to {doing} with")

    def waiting(self, asked):
        request = int(asked["request"])
        reply = self.signer.query(EGRESS, egress_request(request))
        why = failed(reply)
        if why is None:
            return open_of(reply.out)
        if dropped(why):
            return True
        raise Refused(502, f"egress was not asked whether request {request} still waits: {why}")

    def retried(self, asked, attempt, failure):
        pause = self.retry_pause
        attempts = 0
        while True:
            answer = attempt()
            why = failure(answer)
            if why is None or not dropped(why):
                return answer
            attempts += 1
            if attempts & (attempts - 1) == 0:
                say("pods_retried", attempts=attempts, reason="connection_dropped", detail=json.dumps(why))
            if not self.waiting(asked):
                raise Refused(502, f"{why}; request {asked['request']}, which asked for this, is gone, so it was not tried again")
            self.pause(pause)
            pause *= 2

    def execution(self, asked, name, container, command, alive=None):
        key = exec_key(asked)
        with self.lock:
            known = self.execs.get(key)
            if known is not None:
                return known
            running = self.execs[key] = Execution(asked)

        def run():
            running.end(outcome(lambda: self.executed(asked, name, container, command, alive=alive)))
            self.swept.set()

        threading.Thread(target=run, daemon=True).start()
        self.swept.set()
        return running

    def sweep(self):
        with self.lock:
            ended = [(key, run) for key, run in self.execs.items() if run.ended.is_set()]
        for key, run in ended:
            if not self.still_waits(run):
                with self.lock:
                    self.execs.pop(key, None)
        self.sweep_started()

    def sweep_started(self):
        with self.lock:
            places = {}
            for machine, run in self.launched.values():
                places.setdefault(machine, set()).add(run)
        for machine, runs in places.items():
            command = ["sh", BUILD_SH, "ended", *sorted(runs)]
            try:
                ran = self.desktop.ran(command, None) if machine == DESKTOP else self.kube.exec(machine, None, command)
            except Refused:
                continue
            gone = ran.code != 0 and "(NotFound)" in ran.err
            if ran.code != 0 and not gone:
                continue
            ended, moved = set(), {}
            for line in ran.out.decode(errors="replace").splitlines():
                said = line.split()
                if len(said) == 2 and said[0] == "ended":
                    ended.add(said[1])
                elif len(said) == 3 and said[0] == "moved" and ":" in said[2]:
                    moved[said[1]] = tuple(said[2].split(":", 1))
            with self.lock:
                for key, place in list(self.launched.items()):
                    if place[0] != machine:
                        continue
                    if gone or place[1] in ended:
                        del self.launched[key]
                    elif place[1] in moved:
                        self.launched[key] = moved[place[1]]

    def still_waits(self, run):
        try:
            return self.waiting(run.asked)
        except Exception as error:
            run.unswept += 1
            if run.unswept & (run.unswept - 1) == 0:
                reason = "egress_unanswered" if isinstance(error, Refused) else "request_unreadable"
                say("pods_unswept", attempts=run.unswept, reason=reason, detail=json.dumps(repr(error)))
            return True

    def sweeper(self):
        while True:
            self.swept.wait()
            self.swept.clear()
            try:
                self.sweep()
            except Exception as error:
                say("pods_sweep_failed", detail=json.dumps(repr(error)))

    def executed(self, asked, name, container, command, again=False, alive=None):
        if name == DESKTOP:
            return self.desktop.ran(command, alive)
        if name == BUILDS:
            return self.routed(asked, command, alive)

        def attempt():
            return self.kube.exec(name, container, command)

        while True:
            ran = self.through_apiserver(asked, name, lambda: self.retried(asked, attempt, failed) if again else attempt())
            waited = unstarted(ran, container)
            if waited is None or not self.started(asked, name, waited.container):
                return ran

    def through_apiserver(self, asked, name, attempt):
        pause = self.retry_pause
        attempts = 0
        while True:
            ran = attempt()
            if cut(ran):
                raise Refused(502, f"apiserver unavailable: the exec's connection to pod {name} was cut while it ran ({ran.err}); its command may still be running there, so it was not run again")
            if not unreached(ran):
                return ran
            attempts += 1
            if attempts & (attempts - 1) == 0:
                say("pods_retried", attempts=attempts, reason="apiserver_unreachable", detail=json.dumps(ran.err))
            if not self.waiting(asked):
                raise Refused(503, f"apiserver unavailable: {ran.err}; request {asked['request']}, which asked for this, is gone, so nothing ran")
            self.pause(pause)
            pause *= 2

    def started(self, asked, name, container):
        process = None
        try:
            while True:
                if process is None:
                    process, events = self.kube.watch_pod(name), queue.Queue()
                    threading.Thread(target=relay, args=(process, events), daemon=True).start()
                kind, item = events.get()
                if kind == "closed":
                    process = None
                    if item != 0:
                        self.pause(WATCH_PAUSE)
                    ran = self.kube.get(name)
                    if ran.code != 0 and "(NotFound)" in ran.err:
                        return False
                    if ran.code != 0:
                        continue
                    item = {"type": "MODIFIED", "object": json.loads(ran.out)}
                watched = container or default_container(item.get("object") or {})
                start = start_of(item, watched)
                if start == "ended":
                    return False
                if not self.waiting(asked):
                    raise Refused(502, f"container {watched} of pod {name} had not started when request {asked['request']}, which asked for this, was gone, so nothing ran")
                if start == "running":
                    return True
        finally:
            if process is not None:
                process.kill()

    def output(self, name, asked_by, asked):
        container = asked.get("container")
        if name != DESKTOP:
            pod = self.pod(name)
            names = [held["name"] for held in pod["spec"]["containers"]]
            container = container or (OUT if OUT in names else None)
        command = ["cat", "--", absolute(asked["path"], "path")]
        ran = self.executed(asked_by, name, container, command, again=True)
        if ran.code != 0:
            raise kube_refusal(ran, f"{asked['path']} was not read from pod {name}")
        return ran.out

    def create(self, asked, body):
        spec = checked(body, SPEC, "a pod")
        pod = pod_spec(asked, spec, self.fetching, self.kube.namespace)
        ran = self.kube.create(pod)
        if ran.code == 0:
            return 201, warned({"pod": view(json.loads(ran.out))}, spec)
        refusal = kube_refusal(ran, "the pod was not created")
        repeated = refusal.status == 409 and "AlreadyExists" in ran.err
        if not repeated:
            raise refusal
        return 200, warned({"pod": view(self.pod(pod["metadata"]["name"]))}, spec)

    def list(self):
        return 200, {"pods": [view(pod) for pod in self.items(self.kube.list(), "the pods were not listed")]}

    def warm(self):
        if not self.warm_pod:
            raise Refused(503, f"pod {DESKTOP} is not free and this service names no warm pod (PODS_WARM_POD), so pod {BUILDS} has nowhere to run it")
        reply = self.signer.query(FILES_PROGRAM, files_read(self.warm_pod, None, 0, CHUNK_SIZE))
        if reply.code != 0:
            raise Refused(502, f"the warm pod's name was not read from {self.warm_pod}: {failed(reply)}")
        name = contents_of(reply.out)[3].decode(errors="replace").strip()
        if not name:
            raise Refused(502, f"{self.warm_pod} names no warm pod")
        return name

    def pick(self, asked, words, alive):
        try:
            said = self.desktop.state()
        except Refused as refused:
            say("pods_routed", to="warm", reason="desktop_unready", detail=json.dumps(refused.sentence))
            return self.warm(), False
        free, why = desktop_free(said)
        if not free:
            try:
                warm = self.warm()
            except Refused as refused:
                say("pods_routed", to=DESKTOP, reason="warm_unasked", detail=json.dumps(f"{why}; {refused.sentence}"))
                return DESKTOP, False
            there, unasked = self.ahead(asked, warm, words, alive)
            if there is None:
                say("pods_routed", to=DESKTOP, reason="warm_unasked", detail=json.dumps(f"{why}; {unasked}"))
                return DESKTOP, False
            say("pods_routed", to="warm", reason="desktop_busy", detail=json.dumps(why))
            return warm, False
        here, why = self.ahead(asked, DESKTOP, words, alive)
        if here is None:
            try:
                warm = self.warm()
            except Refused as refused:
                say("pods_routed", to=DESKTOP, reason="warm_unasked", detail=json.dumps(refused.sentence))
                return DESKTOP, False
            say("pods_routed", to="warm", reason="desktop_unasked", detail=json.dumps(why))
            return warm, False
        if here[1] == 0:
            return DESKTOP, True
        try:
            warm = self.warm()
        except Refused as refused:
            say("pods_routed", to=DESKTOP, reason="warm_unasked", detail=json.dumps(refused.sentence))
            return DESKTOP, False
        there, why = self.ahead(asked, warm, words, alive)
        if there is None:
            say("pods_routed", to=DESKTOP, reason="warm_unasked", detail=json.dumps(why))
            return DESKTOP, False
        if there[1] >= here[1]:
            return DESKTOP, False
        say("pods_routed", to="warm", reason="shorter_wait", detail=json.dumps(f"{here[0]}: {here[1]} ahead on pod {DESKTOP}, {there[1]} on pod {warm}"))
        return warm, False

    def reroute(self, asked, alive):
        with self.rerouting:
            return self.rerouted(asked, alive)

    def reroute_after(self, asked):
        if not self.rerouting.acquire(blocking=False):
            return

        def run():
            try:
                self.rerouted(asked, None)
            except Exception as error:
                say("pods_reroute_failed", detail=json.dumps(error.sentence if isinstance(error, Refused) else repr(error)))
            finally:
                self.rerouting.release()

        threading.Thread(target=run, daemon=True).start()

    def rerouted(self, asked, alive):
        warm = self.warm()
        listed = self.executed(asked, warm, None, ["sh", BUILD_SH, "movable"], alive=alive)
        if listed.code != 0:
            raise Refused(502, f"build.sh movable on pod {warm} answered exit {listed.code}")
        lanes = self.desktop.ran(["sh", BUILD_SH, "lanes"], alive)
        if lanes.code != 0:
            raise Refused(502, f"build.sh lanes on pod {DESKTOP} answered exit {lanes.code}")
        waits = {found.group(1): int(found.group(2)) for found in map(LANE_WAIT.fullmatch, lanes.out.decode(errors="replace").splitlines()) if found}
        said, chosen, moved_ahead = [], [], {}
        candidates = []
        for line in listed.out.decode(errors="replace").splitlines():
            found = MOVABLE_RUN.fullmatch(line)
            if found:
                candidates.append((found.group(2), int(found.group(3)), found.group(1), found.group(4)))
            elif line.startswith("keep "):
                said.append(line)
        for lane, ahead, run, words in sorted(candidates):
            here, there = waits.get(lane, 0), ahead - moved_ahead.get(lane, 0)
            if here < there:
                chosen.append((run, words))
                waits[lane] = here + 1
                moved_ahead[lane] = moved_ahead.get(lane, 0) + 1
            else:
                said.append(f"keep {run} the {lane} lane waits for {here} runs on pod {DESKTOP} and {there} here")
        if chosen:
            said += self.carried(asked, warm, chosen, alive)
        moved = sum(1 for line in said if ": moved to run " in line)
        say("pods_rerouted", moved=moved, kept=len(said) - moved)
        return "\n".join(said) + "\n" if said else f"nothing queued on pod {warm} to move\n"

    def carried(self, asked, warm, chosen, alive):
        if any(notifies(words) for _, words in chosen):
            self.follow_desktop()
        try:
            took = self.desktop.ran(["sh", BUILD_SH, "take", *[words for _, words in chosen]], alive)
        except Refused as refused:
            raise Refused(502, f"{refused.sentence}; build.sh take on pod {DESKTOP} may have started some of these runs, each still queued on pod {warm} ({', '.join(run for run, _ in chosen)}): read build.sh busy on pod {DESKTOP} before calling reroute again") from refused
        lines = took.out.decode(errors="replace").splitlines()
        said, pairs = [], {}
        for index, (run, _) in enumerate(chosen):
            line = lines[index] if index < len(lines) else f"failed: build.sh take answered exit {took.code} and said nothing of it"
            if line.startswith("run "):
                pairs[run] = f"{DESKTOP}:{line[4:]}"
            else:
                said.append(f"keep {run} pod {DESKTOP} {line}")
        if not pairs:
            return said
        answers, why = self.warm_moved(asked, warm, pairs, alive)
        unread = {run: copy for run, copy in pairs.items() if run not in answers}
        if unread:
            more, why = self.warm_moved(asked, warm, unread, alive)
            answers.update(more)
        stays = {}
        for run, copy in pairs.items():
            line = answers.get(run)
            if line is None:
                said.append(f"unknown {run}: pod {warm} never said whether it ended ({why}), so {copy} may run beside it")
            elif "so it stays here" in line:
                stays[run] = copy
            else:
                said.append(line)
        if stays:
            said += self.yielded(warm, stays, alive)
        return said

    def warm_moved(self, asked, warm, pairs, alive):
        try:
            ran = self.executed(asked, warm, None, ["sh", BUILD_SH, "moved", *itertools.chain.from_iterable(pairs.items())], alive=alive)
        except Refused as refused:
            return {}, refused.sentence
        lines = (line.split(": ", 1) for line in ran.out.decode(errors="replace").splitlines() if ": " in line)
        return {run: f"{run}: {rest}" for run, rest in lines if run in pairs}, f"build.sh moved on pod {warm} answered exit {ran.code} without a line for it"

    def yielded(self, warm, stays, alive):
        words = [word for run, copy in stays.items() for word in (copy.partition(":")[2], f"{warm}:{run}")]
        try:
            ran = self.desktop.ran(["sh", BUILD_SH, "yield", *words], alive)
            answers = dict(line.split(": ", 1) for line in ran.out.decode(errors="replace").splitlines() if ": " in line)
            why = f"build.sh yield answered exit {ran.code}"
        except Refused as refused:
            answers, why = {}, refused.sentence
        said = []
        for run, copy in stays.items():
            answer = answers.get(copy.partition(":")[2], "")
            if answer.startswith("yielded to run "):
                said.append(f"{run}: started before it moved, so it stays on pod {warm}; its copy {copy} yielded to it")
            elif answer.startswith(ENDED_ALREADY):
                said.append(f"{run}: started before it moved, so it stays on pod {warm}; its copy {copy} had already ended {answer[len(ENDED_ALREADY):].strip()}, so each of the two posts its own end")
            else:
                said.append(f"unstopped {copy}: it runs beside {warm}:{run}, which stays ({why}); stop it")
        return said

    def asked_on(self, asked, machine, words, alive):
        command = ["sh", BUILD_SH, *words]
        return self.desktop.ran(command, alive) if machine == DESKTOP else self.executed(asked, machine, None, command, alive=alive)

    def ahead(self, asked, machine, words, alive):
        try:
            ran = self.asked_on(asked, machine, ["ahead", *words], alive)
        except Refused as refused:
            return None, refused.sentence
        found = AHEAD.fullmatch(ran.out.strip()) if ran.code == 0 else None
        if found is None:
            return None, f"build.sh ahead on pod {machine} answered exit {ran.code} with no ahead line"
        return (found.group(1).decode(), int(found.group(2))), None

    def on(self, asked, machine, words, alive):
        if machine == DESKTOP and any(word.startswith("NOTIFY=") and word != "NOTIFY=" for word in words):
            self.follow_desktop()
        command = ["sh", BUILD_SH, *words]
        if machine == DESKTOP:
            return self.desktop.ran(command, alive)
        return self.executed(asked, machine, None, command, alive=alive)

    def routed(self, asked, command, alive):
        words = desktop_words(command, BUILDS)
        settings = list(itertools.takewhile(SETTING.fullmatch, words))
        called = words[len(settings) :]
        first = called[0] if called else ""
        if first == REROUTE and len(called) == 1:
            return Ran(0, self.reroute(asked, alive).encode(), f"{BUILDS}: reroute")
        if first in MACHINE_WIDE:
            raise Refused(400, f"build.sh {first} acts on one machine, so name it (pod {DESKTOP}, or the warm pod) rather than pod {BUILDS}")
        if first in ADDRESSED:
            named = called[1] if len(called) > 1 else ""
            if not named:
                raise Refused(400, f"build.sh {first} on pod {BUILDS} names its run, as <machine>:<run> or the bare run id")
            machine, _, run = named.partition(":")
            if not run:
                run = named
                machine, looked = self.holder(asked, run, alive)
                if first == "status" and len(called) == 2 and not settings:
                    return on_machine(machine, looked, started=False)
            return on_machine(machine, self.on(asked, machine, [*settings, first, run, *called[2:]], alive), started=first == "requeue")
        verb = first if first == "apply" else (called[2] if len(called) > 2 else first)
        claimed = self.claim(asked, settings, words, alive)
        if claimed is not None:
            return claimed
        machine, idle = self.pick(asked, words, alive)
        if not self.still_open(asked):
            raise Refused(502, f"request {asked['request']}, which asked for this, is gone, so build.sh was not called on pod {machine}: nothing ran")
        try:
            ran = self.launched_on(asked, machine, settings, words, alive)
            if idle:
                self.reroute_after(asked)
        except Refused as refused:
            if machine != DESKTOP:
                raise
            if isinstance(refused, Cut):
                self.record(words, DESKTOP, refused.out)
            if isinstance(refused, Cut) and verb not in READ_ONLY:
                started = STARTED.search(refused.out)
                run = f"{DESKTOP}:{started.group(1).decode(errors='replace')}" if started else None
                say("pods_parked", verb=json.dumps(verb), run=run or "unstarted", detail=json.dumps(refused.sentence))
                where = f"build.sh status {run}" if run else f"build.sh busy on pod {DESKTOP}"
                raise Refused(502, f"{refused.sentence}; build.sh {verb} changes things, so it is parked rather than run on another machine{f' as run {run}' if run else ''}: once pod {DESKTOP} answers, read {where} before calling it again") from refused
            try:
                machine = self.warm()
            except Refused as unnamed:
                started = STARTED.search(refused.out) if isinstance(refused, Cut) else None
                run = f" as run {DESKTOP}:{started.group(1).decode(errors='replace')}, which may still run there" if started else ""
                say("pods_unrouted", reason="desktop_lost", detail=json.dumps(f"{refused.sentence}; {unnamed.sentence}"))
                raise Refused(502, f"{refused.sentence}{run}; it was not run elsewhere, since {unnamed.sentence}: once pod {DESKTOP} answers, read build.sh {'status ' + DESKTOP + ':' + started.group(1).decode(errors='replace') if started else 'busy on pod ' + DESKTOP} before calling it again") from refused
            say("pods_routed", to="warm", reason="desktop_lost", detail=json.dumps(refused.sentence))
            ran = self.launched_on(asked, machine, settings, words, alive)
        return on_machine(machine, ran)

    def launched_on(self, asked, machine, settings, words, alive):
        if "WAIT=1" not in settings:
            ran = self.on(asked, machine, words, alive)
            self.record(words, machine, ran.out)
            return ran
        unheld = [word for word in settings if word != "WAIT=1"] + words[len(settings) :]
        ran = self.on(asked, machine, unheld, alive)
        self.record(words, machine, ran.out)
        started = STARTED.search(ran.out)
        if started is None:
            return ran
        run = started.group(1).decode(errors="replace")
        try:
            held = self.asked_on(asked, machine, ["wait", run], alive)
        except Cut as cut:
            raise Cut(cut.sentence, f"run {run}\n".encode() + cut.out) from cut
        return Ran(held.code, f"run {run}\n".encode() + held.out, held.err)

    def still_open(self, asked):
        try:
            return self.waiting(asked)
        except Refused as refused:
            say("pods_request_unchecked", request=asked["request"], detail=json.dumps(refused.sentence))
            return True

    def record(self, words, machine, out):
        started = STARTED.search(out)
        if started is not None:
            with self.lock:
                self.launched[tuple(words)] = (machine, started.group(1).decode(errors="replace"))

    def claim(self, asked, settings, words, alive):
        key = tuple(words)
        with self.lock:
            place = self.launched.get(key)
        seen = []
        while place is not None and place not in seen:
            seen.append(place)
            machine, run = place
            try:
                ran = self.asked_on(asked, machine, ["claim", run, *words], alive)
            except Refused as refused:
                return self.claimed(asked, settings, key, place, f"it was not checked there ({refused.sentence})", alive)
            said = ran.out.decode(errors="replace").strip()
            if ran.code == 0 and said.startswith("same "):
                return self.claimed(asked, settings, key, (machine, said[5:].strip()), "it is still queued or running there", alive)
            if ran.code == 0 and said.startswith("moved ") and ":" in said:
                place = tuple(said[6:].strip().split(":", 1))
                continue
            if ran.code == 3 and said.startswith("unknown "):
                return self.claimed(asked, settings, key, place, f"it was not checked there ({said[8:]})", alive)
            with self.lock:
                if self.launched.get(key) in seen:
                    del self.launched[key]
            say("pods_run_unclaimed", run=json.dumps(f"{machine}:{run}"), reason=json.dumps(said.split()[0] if said else f"exit {ran.code}"))
            return None
        return None

    def claimed(self, asked, settings, key, place, why, alive):
        machine, run = place
        with self.lock:
            self.launched[key] = place
        say("pods_run_claimed", run=json.dumps(f"{machine}:{run}"), request=asked["request"])
        note = f"the same call started run {machine}:{run} for an earlier request, and {why}, so this call gets that run rather than a second one"
        if "WAIT=1" in settings:
            try:
                ran = self.asked_on(asked, machine, ["wait", run], alive)
                return on_machine(machine, Ran(ran.code, f"run {run}\n".encode() + ran.out, f"{note}\n{ran.err}".rstrip("\n")))
            except Refused as refused:
                note = f"{note}; its wait was not held ({refused.sentence})"
        return on_machine(machine, Ran(0, f"run {run}\nread it with: build.sh status {run}\n".encode(), note))

    def holder(self, asked, run, alive):
        unasked, lacking = [], []
        for machine in (DESKTOP, None):
            where = f"pod {machine}" if machine else "the warm pod"
            try:
                if machine is None and not self.warm_pod:
                    raise Refused(503, "this service names no warm pod (PODS_WARM_POD)")
                machine = machine or self.warm()
                where = f"pod {machine}"
                ran = self.on(asked, machine, ["status", run], alive)
            except Refused as refused:
                unasked.append(f"{where} could not be asked: {refused.sentence}")
                continue
            if ran.out.startswith(b"job "):
                return machine, ran
            if ran.code == 2 and f"no run {run} in pod" in ran.err:
                lacking.append(f"{where} has no run {run}")
            else:
                unasked.append(f"{where} answered build.sh status {run} with exit {ran.code} and no job line: {ran.err}")
        said = "; ".join(unasked + lacking)
        if unasked:
            raise Refused(503, f"run {run} was not found, and not every machine of pod {BUILDS} could be asked ({said}): ask again, or name it as <machine>:<run>")
        raise Refused(404, f"no machine of pod {BUILDS} holds run {run}: {said}")

    def status(self, name):
        if name == BUILDS:
            raise Refused(400, f"pod {BUILDS} routes build.sh to the machine where it waits least and is no pod itself: read pod {DESKTOP} or the warm pod")
        if name == DESKTOP:
            return 200, {"pod": desktop_view(self.desktop.state())}
        return 200, {"pod": view(self.pod(name))}

    def logs(self, name, query):
        if name == DESKTOP:
            return 200, self.desktop.logs(log_options(query))
        ran = self.kube.logs(name, log_options(query))
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name}'s logs were not read")
        return 200, ran.out

    def wait(self, name, query):
        every = wait_every(query)
        if name == DESKTOP:
            return 200, Stream(self.desktop_waited(self.desktop.follow(every), every))
        return 200, Stream(self.waited(name, self.pod(name), every))

    def follow_desktop(self):
        with self.lock:
            if self.desktop_follower is None:
                self.desktop_follower = threading.Thread(target=self.followed_desktop, args=(self.clock(), self.wall()), daemon=True)
                self.desktop_follower.start()
        self.desktop_wanted.set()

    def followed_desktop(self, begun, wall):
        since = begun
        reached = False
        pause = self.retry_pause
        while True:
            asked = self.clock()
            if not reached:
                since, reached = self.runner_began(since, begun, wall)
                if reached:
                    self.caught_up(math.ceil(asked - since))
            heard = []
            try:
                process = self.desktop.tail(math.ceil(asked - since))
            except Refused as refused:
                say("pods_unfollowed", pod=DESKTOP, detail=json.dumps(refused.sentence))
            else:
                concerned = self.warm_requester()
                with process.stdout:
                    held = self.notifier.lines(DESKTOP, None, heard_in(process.stdout, heard), unvouched=True, concerned=concerned)
                say("pods_unfollowed", pod=DESKTOP, code=process.wait(), already_posted=held)
            if heard:
                since, pause = asked, self.retry_pause
            self.rest(pause)
            if not heard:
                pause *= 2
            self.desktop_wanted.clear()

    def caught_up(self, window):
        try:
            logged = self.desktop.logs([f"--since={window}s"])
        except Refused as refused:
            say("pods_uncaught", pod=DESKTOP, detail=json.dumps(refused.sentence))
            return
        passed, asked, why = self.notifier.passed(DESKTOP, list(notify_lines(DESKTOP, logged.splitlines(keepends=True))))
        if why is not None:
            say("pods_uncaught", pod=DESKTOP, asked=asked, detail=json.dumps(why))
            return
        say("pods_caught_up", pod=DESKTOP, already_posted=passed, asked=asked)

    def warm_requester(self):
        if not self.warm_pod:
            return None
        try:
            return requester_of(self.pod(self.warm()).get("metadata", {}).get("labels") or {})
        except Refused as refused:
            say("pods_unconcerned", pod=DESKTOP, detail=json.dumps(refused.sentence))
            return None

    def runner_began(self, since, begun, wall):
        try:
            said = self.desktop.state()
        except Refused as refused:
            say("pods_unresumed", pod=DESKTOP, detail=json.dumps(refused.sentence))
            return since, False
        if said.get("started") is None:
            say("pods_unresumed", pod=DESKTOP, detail=json.dumps("the desktop's ping names no start time"))
            return since, False
        started = go_time(said["started"])
        if started is None:
            say("pods_unresumed", pod=DESKTOP, detail=json.dumps(f"the desktop's ping names its start as {said['started']!r}, not as podman's StartedAt (date, time, offset, zone)"))
            return since, False
        return min(since, begun - max(0.0, wall - started)), True

    def desktop_waited(self, process, every):
        ended = queue.Queue()

        def drained():
            with process.stdout:
                for _ in process.stdout:
                    pass
            ended.put((process.wait(), process.stderr.read().decode(errors="replace").strip()))

        threading.Thread(target=drained, daemon=True).start()
        try:
            code, why = yield from Beat(every).heard(ended)
        finally:
            if process.poll() is None:
                process.kill()
        line = f"pod {DESKTOP}'s log stream ended (ssh exit {code}): {why or 'it said nothing'}"
        yield sse("ended", {"line": line, "exit_code": None, "pod": None})

    def waited(self, name, pod, every):
        ended = ended_event(pod)
        beat = Beat(every)
        process = None
        try:
            while ended is None:
                if process is None:
                    process, events = self.kube.watch_pod(name), queue.Queue()
                    threading.Thread(target=relay, args=(process, events), daemon=True).start()
                kind, item = yield from beat.heard(events)
                match kind:
                    case "event":
                        ended = watch_ended(name, item)
                    case "closed":
                        process = None
                        ended = self.reread(name, item)
        finally:
            if process is not None:
                process.kill()
        yield sse("ended", ended)

    def reread(self, name, code):
        if code != 0:
            time.sleep(WATCH_PAUSE)
        ran = self.kube.get(name)
        gone = ran.code != 0 and "(NotFound)" in ran.err
        if gone:
            return {"line": f"pod {name} is gone", "exit_code": None, "pod": None}
        if ran.code != 0:
            return None
        return ended_event(json.loads(ran.out))

    def delete(self, name):
        if name == DESKTOP:
            raise Refused(403, f"pod {DESKTOP} is orthory's desktop build runner, a machine and not a pod, so nothing deletes it")
        ran = self.kube.delete(name)
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name} was not deleted")
        return 200, {"deleted": name}

    def exec(self, name, asked_by, body):
        asked = checked(body, EXEC, "an exec")
        command = strings(asked.get("command", []), "command")
        container = asked.get("container")
        if name == DESKTOP and any(word.startswith("NOTIFY=") and word != "NOTIFY=" for word in desktop_words(command)):
            self.follow_desktop()
        if "every" not in asked:
            return 200, exec_answer(name, answered(self.execution(asked_by, name, container, command).get()))
        every = from_one(asked["every"], "every")
        if name not in (DESKTOP, BUILDS):
            self.pod(name)
        return 200, Stream(self.exec_waited(asked_by, name, container, command, every))

    def exec_waited(self, asked, name, container, command, every):
        ran = yield from Beat(every).heard(self.execution(asked, name, container, command, alive=every))
        try:
            answer = "ended", exec_answer(name, answered(ran))
        except Refused as refusal:
            answer = "refused", {"status": refusal.status, "error": refusal.sentence}
        yield sse(*answer)

    def blob(self, name, asked_by, body):
        asked = checked(body, BLOB, "a blob")
        if ("path" in asked) == ("paths" in asked):
            raise Refused(400, "a blob names its path, or its paths to put several in one bundle")
        named(asked, ["kind"], "a blob")
        paths = [asked["path"]] if "path" in asked else strings(asked["paths"], "paths")
        self.keyed("put blobs")
        datas = [self.output(name, asked_by, {**asked, "path": path}) for path in paths]
        with tempfile.TemporaryDirectory() as temp:
            files = [Path(temp) / f"blob-{index}" for index in range(len(datas))]
            for file, data in zip(files, datas):
                file.write_bytes(data)
            put = self.retried(asked_by, lambda: self.signer.put(files, asked["kind"]), failed)
        if put.code != 0:
            raise Refused(502, f"the blob was not put: {put.err or put.out.decode(errors='replace')}")
        held = put_answer(put.out.decode(), len(files))
        blobs = held.pop("blobs")
        if "path" in asked:
            return 200, {"blob": blobs[0], "size": len(datas[0]), "kind": asked["kind"], **held}
        listed = [{"path": path, "blob": blob, "size": len(data)} for path, blob, data in zip(paths, blobs, datas)]
        return 200, {"blobs": listed, "kind": asked["kind"], **held}

    def file(self, name, asked_by, body):
        asked = named(checked(body, FILE, "a file"), ["path", "to"], "a file")
        to = absolute(asked["to"], "to")
        self.keyed("write files")
        data = self.output(name, asked_by, asked)
        pieces = [data[start : start + CHUNK_SIZE] for start in range(0, len(data), CHUNK_SIZE)]
        chunks = []
        with tempfile.TemporaryDirectory() as temp:
            paths = [Path(temp) / f"chunk-{index}" for index in range(len(pieces))]
            for path, piece in zip(paths, pieces):
                path.write_bytes(piece)
            if paths:
                put = self.retried(asked_by, lambda: self.signer.put(paths, CHUNK), failed)
                if put.code != 0:
                    raise Refused(502, f"the file's chunks were not put: {put.err or put.out.decode(errors='replace')}")
                chunks = put_answer(put.out.decode(), len(paths))["blobs"]
        message = asked.get("message") or f"pods: {name} wrote {to}"
        base, (outcome, reason, sentence, height) = self.committed(asked_by, files_commit_of(message, to, len(data), chunks))
        match outcome:
            case "applied":
                return 200, {"written": to, "size": len(data), "chunks": chunks, "base": base, "height": height}
            case "rejected":
                raise Refused(REJECTED.get(reason, 422), f"files refused the write at block {height}: {reason}: {sentence}")
            case "unlanded":
                raise Refused(502, f"the write never landed: {sentence}")
            case "unknown":
                raise Refused(502, f"the write was sent: {sentence}")
            case "failed":
                raise Refused(502, f"the write was not sent: {sentence}")

    def committed(self, asked_by, commit):
        def attempt():
            head = self.retried(asked_by, lambda: self.signer.query(FILES_PROGRAM, files_head()), failed)
            if head.code != 0:
                raise Refused(502, f"files' head was not read: {head.err}")
            base = head_of(head.out)
            return base, receipt(self.signer.submit(FILES_PROGRAM, commit(base)))

        unsent = lambda tried: tried[1][2] if tried[1][0] == "failed" else None
        pause = self.retry_pause
        while True:
            tried = self.retried(asked_by, attempt, unsent)
            outcome, reason, sentence, _ = tried[1]
            if not moved_under(outcome, reason, sentence):
                return tried
            say("pods_retried", reason="files_moved", detail=json.dumps(f"{reason}: {sentence}"))
            if not self.waiting(asked_by):
                raise Refused(
                    502,
                    f"files moved under the write ({reason}: {sentence}); request {asked_by['request']}, which asked for this, is gone, so it was not tried again",
                )
            self.pause(pause)
            pause *= 2

    def push(self, name, asked_by, body):
        asked = named(checked(body, PUSH, "a push"), ["path"], "a push")
        path = absolute(asked["path"], "path")
        rev = asked.get("ref") or "HEAD"
        if rev.startswith("-"):
            raise Refused(400, f"ref {rev} names a revision, not an option")
        have = asked.get("have", [])
        unnamed = [oid for oid in have if not isinstance(oid, str) or not OID.fullmatch(oid)]
        if unnamed:
            raise Refused(400, f"have holds commit ids, 40 or 64 lowercase hex digits, not {unnamed[0]!r}")
        self.keyed("put the pack")
        container = asked.get("container")
        resolve = ["sh", "-c", RESOLVING, "resolve", path, rev, *have]
        resolved = self.executed(asked_by, name, container, resolve, again=True)
        lines = git_output(name, resolved, f"{rev} names no commit in {path}").decode().splitlines()
        commit, *parents = lines[0].split()
        commits = int(lines[1])
        packing = ["sh", "-c", PACKING, "pack", path, commit, *have]
        packed = self.executed(asked_by, name, container, packing, again=True)
        pack = git_output(name, packed, f"{commit} was not packed in {path}")
        objects = objects_of(pack)
        answer = {"commit": commit, "parents": parents, "commits": commits, "objects": objects, "size": len(pack), "kind": PACK}
        if objects == 0:
            return 200, {**answer, "pack": None}
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / "pack"
            file.write_bytes(pack)
            put = self.retried(asked_by, lambda: self.signer.put([file], PACK), failed)
        if put.code != 0:
            raise Refused(502, f"the pack was not put: {put.err or put.out.decode(errors='replace')}")
        held = put_answer(put.out.decode(), 1)
        return 200, {**answer, "pack": held.pop("blobs")[0], **held}

    def cluster_kinds(self):
        ran = self.kube.cluster_kinds()
        if ran.code != 0:
            raise Refused(502, f"the cluster's kinds were not read: {ran.err}")
        return {line.split()[-1] for line in ran.out.decode().splitlines() if line.strip()}

    def items(self, ran, doing):
        if ran.code != 0:
            raise kube_refusal(ran, doing)
        return json.loads(ran.out)["items"]

    def tried(self, manifest, what):
        templated = manifest["kind"] != "Pod"
        for template, spec in pod_specs(manifest) if templated else []:
            tried = self.kube.trial(trial_pod(manifest, template, spec, self.kube.namespace))
            if tried.code != 0:
                raise kube_refusal(tried, f"{what} ({manifest['kind']} {manifest['metadata'].get('name', '')})'s pods would be refused")

    def apply(self, asked, body):
        request = named(checked(body, APPLY, "an apply"), ["manifests"], "an apply")
        labels = labels_for(asked, request.get("requester"))
        kinds = self.cluster_kinds()
        namespace = self.kube.namespace
        objects = [confine_manifest(manifest, f"manifest {index}", kinds, namespace, labels) for index, manifest in enumerate(request["manifests"])]
        for index, manifest in enumerate(objects):
            self.tried(manifest, f"manifest {index}")
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

    def passthrough(self, method, segments, query, asked, headers, body):
        target = target_of(segments, self.kube.namespace)
        self.readable(target.resource, None)
        kind = headers.get("content-type")
        whole = target.subresource in (None, "ephemeralcontainers")
        match (method, whole):
            case ("POST" | "PUT", True):
                body, kind = self.confined(asked, parsed(body)), "application/json"
            case ("PATCH", True):
                tried = self.api.call(method, target.path, dry_run(query), body, kind)
                refused = not 200 <= tried.status < 300
                if refused:
                    return tried.status, tried
                self.patchable(json.loads(tried.body))
        answered = self.api.call(method, target.path, query, body, kind)
        return answered.status, answered

    def confined(self, asked, manifest):
        what = "the object"
        confined = confine_manifest(manifest, what, self.cluster_kinds(), self.kube.namespace, labels_for(asked, None))
        self.tried(confined, what)
        return json.dumps(confined).encode()

    def patchable(self, result):
        what = "the patched object"
        confine_manifest(copy.deepcopy(result), what, self.cluster_kinds(), self.kube.namespace, {})
        if not forced_holds(result):
            name = (result.get("metadata") or {}).get("name", "")
            raise Refused(
                403,
                f"{what} ({result.get('kind')} {name}) would run outside priority class {PRIORITY} or mount a service account token: a patch can't be confined, only refused, so send the whole object with PUT",
            )

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
            case ("GET", "pods", name, "wait"):
                return self.wait(name, query)
            case ("POST", "pods", name, "exec"):
                return self.exec(name, asked, parsed(body))
            case ("POST", "pods", name, "blob"):
                return self.blob(name, asked, parsed(body))
            case ("POST", "pods", name, "file"):
                return self.file(name, asked, parsed(body))
            case ("POST", "pods", name, "push"):
                return self.push(name, asked, parsed(body))
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
            case (_, "k8s", *path):
                return self.passthrough(method, path, parts.query, asked, headers, body)
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
            except Exception as error:
                say("pods_failed", reason="unexpected", detail=json.dumps(repr(error)))
                status, answer = 500, {"error": f"the pods service failed: {error!r}"}
            if isinstance(answer, Stream):
                self.stream(status, answer.events)
                return
            match answer:
                case Answered(kind=kind, body=payload):
                    pass
                case bytes():
                    payload, kind = answer, "text/plain; charset=utf-8"
                case _:
                    payload, kind = json.dumps(answer).encode(), "application/json"
            self.send_response(status)
            self.send_header("content-type", kind)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def stream(self, status, events):
            self.send_response(status)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.send_header("transfer-encoding", "chunked")
            self.send_header("connection", "close")
            self.end_headers()
            said = streamed(events)
            try:
                for event in said:
                    self.wfile.write(chunk(event))
                    self.wfile.flush()
                self.wfile.write(LAST_CHUNK)
                self.wfile.flush()
            except OSError:
                say("pods_stream_unheard", reason="caller_gone")
            finally:
                said.close()
                events.close()
                self.close_connection = True

        def do_GET(self):
            self.respond("GET")

        def do_POST(self):
            self.respond("POST")

        def do_DELETE(self):
            self.respond("DELETE")

        def do_PUT(self):
            self.respond("PUT")

        def do_PATCH(self):
            self.respond("PATCH")

        def log_message(self, form, *args):
            route = urlsplit(self.path).path
            status = args[1] if len(args) > 1 else ""
            say("pods_request", method=self.command, route=route, status=status)

    return Handler


class Fetcher:
    def __init__(self, ducktape, node, forge, home, runner=run, block_ms=None):
        self.retry_pause = (node_block_ms(node) if block_ms is None else block_ms) / 1000
        self.ducktape = ducktape
        self.node = node
        self.forge = forge
        self.home = home
        self.runner = runner
        self.password = secrets.token_hex(16)
        self.pause = time.sleep

    def env(self):
        return dict(os.environ, DUCKTAPE_PASSWORD=self.password, NO_COLOR="1", DUCKTAPE_DEVICE_KEY_STORE="file")

    def cli(self, *args, stdin=None):
        return self.runner([*self.ducktape, "--workspace", str(self.home), "--node", self.node, *args], stdin=stdin, env=self.env())

    def tried(self, attempt, doing):
        pause = self.retry_pause
        while True:
            ran = attempt()
            why = failed(ran)
            if why is None or not dropped(why):
                return ran
            print(f"{doing}, it lost its connection, so it is tried again in {pause:g} s: {why}", flush=True)
            self.pause(pause)
            pause *= 2

    def must(self, attempt, doing):
        ran = self.tried(attempt, doing)
        if ran.code != 0:
            raise Refused(502, f"{doing}: {ran.err or ran.out.decode(errors='replace')}")
        return ran.out

    def reader(self):
        keys = self.home / "keys"
        minted = keys.exists() and any(keys.iterdir())
        if not minted:
            self.must(lambda: self.cli("wallet", "new", "fetch"), "a reading key was not minted")

    def forge_input(self, forge, target):
        chain = urlsplit(self.forge).netloc
        remote = f"{self.forge.rstrip('/')}/{forge['repo']}"
        commit = forge["commit"]
        git = ["git", "-c", f"duck.{chain}.node={self.node}"]
        inside = [*git, "-C", str(target)]

        def clone():
            if target.exists():
                shutil.rmtree(target)
            return self.runner([*git, "clone", "--no-checkout", remote, str(target)], env=self.env())

        self.must(clone, f"{remote} was not cloned")
        on_a_branch = self.runner([*inside, "cat-file", "-e", f"{commit}^{{commit}}"], env=self.env()).code == 0
        if not on_a_branch:
            fetched = lambda: self.runner([*inside, "fetch", "origin", commit], env=self.env())
            self.must(fetched, f"{forge['repo']} has no commit {commit} on any ref")
            print(f"{commit} is on no branch of {remote}, so it was fetched by its id", flush=True)
        checkout = lambda: self.runner([*inside, "checkout", "--detach", commit], env=self.env())
        self.must(checkout, f"{forge['repo']} has no commit {commit}")
        print(f"cloned {remote} at {commit}", flush=True)

    def files_input(self, files, target):
        self.reader()
        snapshot = files.get("snapshot")
        offset, data, size = 0, b"", 0
        while True:
            request = files_read(files["path"], snapshot, offset, CHUNK_SIZE)
            reply = self.must(lambda: self.cli("query", FILES_PROGRAM, "--raw", stdin=request), f"files did not read {files['path']}")
            read_at, size, _, piece, eof = contents_of(reply)
            snapshot = snapshot or read_at
            data += piece
            offset += len(piece)
            if eof or not piece:
                break
        target.write_bytes(data)
        print(f"read {files['path']} at snapshot {snapshot}, {len(data)} of {size} bytes", flush=True)

    def blob_input(self, blob, target):
        framed = self.must(lambda: self.cli("blob", "get", blob), f"blob {blob} was not fetched")
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
                case ["scratch"]:
                    continue
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
    fetcher = Fetcher(ducktape, os.environ["PODS_NODE"], os.environ["PODS_FORGE"], Path(tempfile.mkdtemp()), block_ms=int(os.environ["PODS_BLOCK_MS"]))
    try:
        fetcher.fetch(json.loads(os.environ["PODS_INPUTS"]), Path(IN_ROOT))
    except Refused as refusal:
        print(refusal.sentence, file=sys.stderr, flush=True)
        Path("/dev/termination-log").write_text(refusal.sentence)
        sys.exit(1)


class Stopping(BaseException):
    pass


def exit_on_signal(number, frame):
    raise Stopping(signal.Signals(number).name)


def main():
    named = {name: os.environ.get(name, "") for name in ("PODS_OUT_IMAGE", "PODS_NODE", "PODS_FORGE")}
    unset = [name for name, value in named.items() if not value]
    if unset:
        sys.exit(f"{', '.join(unset)} must be set: the image of each pod's fetch and out containers, the node they read inputs from, and its forge")
    try:
        block_ms = node_block_ms(named["PODS_NODE"])
    except (OSError, Refused) as error:
        sys.exit(f"the network's block time was not read from {named['PODS_NODE']}/v1/status, and every retry pauses from it: {error}")
    fetching = Fetching(image=named["PODS_OUT_IMAGE"], node=named["PODS_NODE"], forge=named["PODS_FORGE"], block_ms=block_ms)
    kube = Kube(shlex.split(os.environ.get("PODS_KUBECTL", "kubectl")), os.environ.get("PODS_NAMESPACE", "agents"))
    signer = Signer(shlex.split(os.environ.get("PODS_DUCKTAPE", "ducktape")), Path(os.environ.get("PODS_KEYS", "/keys")))
    desktop = Desktop(Path(os.environ.get("PODS_DESKTOP", DESKTOP_KEYS)))
    service = Service(kube, signer, fetching, Stats(), Api(), desktop, os.environ.get("PODS_WARM_POD"))
    threading.Thread(target=service.notifier.watch, daemon=True).start()
    threading.Thread(target=service.sweeper, daemon=True).start()
    if desktop.held():
        service.follow_desktop()
    host, port = os.environ.get("PODS_LISTEN", "0.0.0.0:8080").rsplit(":", 1)
    server = ThreadingHTTPServer((host, int(port)), handler(service))
    try:
        signal.signal(signal.SIGTERM, exit_on_signal)
        bound, at = server.server_address[:2]
        say("pods_serving", namespace=kube.namespace, listen=f"{bound}:{at}", keyed=signer.held())
        server.serve_forever()
    except Stopping as stopping:
        say("pods_stopping", signal=stopping.args[0])
        os._exit(0)


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["fetch"]:
            fetch_main()
        case []:
            main()
        case other:
            sys.exit(f"usage: server.py [fetch], not {' '.join(other)}")
