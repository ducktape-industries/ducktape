#!/usr/bin/env python3
# The pods service: agents' computing on the cluster through a network's egress, in one namespace: pods run with fetched inputs (tried again while the pod lives when a fetch loses its connection), exec answered whole or streamed while it runs, logs, a wait that streams until a pod ends, manifests applied and deleted, Kubernetes API calls passed through inside the namespace, outputs put as blobs or files, a pod's commit packed for a forge push, chat notices to the requester when work ends, when the scheduler first can't place a notified pod, or when a notified pod prints a `ducktape-notify` line, forge-reach reports of each `ducktape-report` line a lane prints (whether each landed said in that run's end notice), the shots each `ducktape-files` line names written into files in one commit (said there too), a lane started by itself on each merge queue front's Preview that waits on the service's account, and usage; the build runners PODS_RUNNERS names (`desktop` unless it names others) are machines reserved as pods of their names, reached by ssh through their `gate` command, straight or through a member node's halo forward.
# ops/k8s/pods/pods.yaml runs it on fabrics-1, server-test.py tests it, `server.py fetch` is each pod's input fetcher, and `server.py hardcap` prints the agents quota k8s/pods/deploy applies, refusing one below what agents' pods already hold.
import base64
import binascii
import copy
import datetime
import decimal
import hashlib
import io
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
import socket
import ssl
import subprocess
import tarfile
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
from pathlib import Path, PurePosixPath
from urllib.parse import parse_qs, parse_qsl, quote, urlencode, urlsplit

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
FLEET_PROGRAM = "fleet"
FLEET_ASKER = "fleet-asker"
ASKER = "asker"
FLEET_RUN = "fleet-run"
FLEET_ATTEMPT = "fleet-attempt"
RUN_LABEL = NOTE + FLEET_RUN
ATTEMPT_LABEL = NOTE + FLEET_ATTEMPT
NOTIFY_CHANNEL = NOTE + "notify-channel"
NOTIFY_THREAD = NOTE + "notify-thread"
NOTIFIED = NOTE + "notified"
NOTIFIED_PENDING = NOTE + "notified-pending"
UNSCHEDULABLE = "Unschedulable"
REATTACH = "ducktape-reattach"
REATTACH_BY = "request"
NOTIFY_LINE = "ducktape-notify "
NOTIFY_FOR = "ducktape-notify-for "
CALLER_WORD = "CALLER="
PINNED_WORD = "PINNED=1"
ACCOUNT = re.compile(r"[0-9]{1,20}")
U64_MAX = (1 << 64) - 1
EGRESS_SENDER = 7
RUN_LINE = "ducktape-run "
KEPT = ("pods", "runs")
SPEC = {
    "image": str,
    "command": list,
    "args": list,
    "env": dict,
    "workdir": str,
    "resources": dict,
    "inputs": list,
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
EXEC = {"command": list, "script": str, "container": str, "every": int}
PUSH = {"path": str, "ref": str, "have": list, "container": str, "branch": str, "heads": list, "allow_stack": bool}
READ = {"path": str, "offset": int, "length": int, "container": str}
REPORT = {"report": str, "push": dict}
APPLY = {"manifests": list}
DELETE = {"kind": str, "name": str}
LOGS = {"tail", "since", "container"}
WAIT = {"every", "log"}
LOG_READ = 'wc -c < "$1" && tail -c 2048 "$1" | tail -n 1'
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
FORGE_REACH = "forge-reach"
IDENTITY = "identity"
USER_KEY = "ducktape-user-key-v1:"
USER_KEY_LEN = 16 + 12 + 24 + 32 + 32 + 16
PUBKEY_AT = 16 + 12 + 24
LANE_CONTEXT = "build.sh#lane"
RERUN = "rerun-"
FORGE_PROGRAM = "forge"
FRONT_KEYS = tuple(table.hex() for table in (b"e/", b"s/", b"m/", b"q/"))
CHECK_STATES = ("pending", "passed", "failed", "errored")
REPORT_LINE = "ducktape-report "
FILES_LINE = "ducktape-files "
REPORT_STATES = {"passed": 1, "failed": 2}
FULL_COMMIT = re.compile(r"[0-9a-f]{40}")
MOVED_ELSEWHERE = re.compile(r"moved to run (\S+?):(\S+) on another machine")
RUN_HAS_ENDED = re.compile(r"\(job [^)\n]*\) has ended$", re.M)
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
QUANTITY_SCHEMA = "io.k8s.apimachinery.pkg.api.resource.Quantity"
PATCH_SCHEMA = "io.k8s.apimachinery.pkg.apis.meta.v1.Patch"
SCHEMA_REF = "#/components/schemas/"
OPENAPI = "/openapi/v3"
WRITES = ("POST", "PUT", "PATCH")
CHANGES = ("PUT", "PATCH")
CONNECTS = ("attach", "exec", "portforward", "proxy")
PROXY_SCHEMES = ("", "http", "https")
JSON_PATCH = "application/json-patch+json"
MERGE_PATCH = "application/merge-patch+json"
STRATEGIC_PATCH = "application/strategic-merge-patch+json"
APPLY_PATCH = "application/apply-patch+yaml"
JSON_BODIES = ("application/json", MERGE_PATCH, STRATEGIC_PATCH, JSON_PATCH, APPLY_PATCH)
PATCHED_VALUES = ("add", "replace")
COPIED_VALUES = ("copy", "move")
DIRECTIVE = "$"
GO_SPACE = "\t\n\v\f\r \x85\xa0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a\u2028\u2029\u202f\u205f\u3000"
TYPE_FIELDS = {"apiversion": "apiVersion", "kind": "kind"}
BINARY_SI = ("", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei")
DECIMAL_SI = {-9: "n", -6: "u", -3: "m", 0: "", 3: "k", 6: "M", 9: "G", 12: "T", 15: "P", 18: "E"}
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
READING = r"""size=$(wc -c < "$1") || exit
printf '%s\n' "$size"
if [ -n "$3" ]; then tail -c +$(($2 + 1)) -- "$1" | head -c "$3"; else tail -c +$(($2 + 1)) -- "$1"; fi
"""
FLUSH = b"0000"
WATCH_PAUSE = 1
SERVICE_ACCOUNT = Path("/var/run/secrets/kubernetes.io/serviceaccount")
DESKTOP = "desktop"
BUILDS = "builds"
ADDRESSED = frozenset({"status", "log", "stop", "wait", "requeue", "failures", "diag"})
MACHINE_WIDE = frozenset({"busy", "bump", "prune", "jobs", "heavy-runs", "disk-targets", "queue-out", "queue-in", "self-update", "worktree", "measure", "claim", "ended", "holds"})
COMMIT_ID = re.compile(r"[0-9a-f]{7,40}")
APPLY_VALUED = frozenset({"-m", "-F", "--onto", "--base"})
READ_ONLY = frozenset({"gates", "lint", "check", "lane", "lane-test", "test", "show", "grep", "schema-diff"})
STARTED = re.compile(rb"^run (\S+)$", re.M)
GO_TIME = re.compile(r"(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))? ([+-]\d{4})(?: \S+)?")
AHEAD = re.compile(rb"ahead (heavy|lane|quick|light) ([0-9]+)(?: ([0-9]+|-))?")
MOVABLE_RUN = re.compile(r"move (\S+) (heavy|lane|quick:\S+) ([0-9]+) (\S+)")
LANE_WAIT = re.compile(r"(heavy|lane|quick:\S+) ([0-9]+)")
REROUTE = "reroute"
DESKTOP_KEYS = "/desktop"
RUNNER_FILES = ("id_ed25519", "known_hosts", "user")
HOSTS_FILE = "hosts"
FORWARD_FILE = "forward"
KINDS_FILE = "kinds"
MOVABLE_WORD = "MOVABLE=1"
UNSTARTING = frozenset({"ahead", "lanes", "movable", "take", "moved", "yield"})
RUNNER_NAME = re.compile(r"[a-z0-9]([a-z0-9-]*[a-z0-9])?")
AHEAD_PAUSED = re.compile(rb"ahead (heavy|lane|quick|light) paused")
LANE_PAUSED = re.compile(r"(heavy|lane|quick:\S+) paused")
BUILD_SH = "/var/tmp/warm/build.sh"
LANE_FIRST_LIMIT = os.environ.get("PODS_LANE_FIRST_LIMIT", "")
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
    reattach: bool = False


@dataclass(frozen=True)
class Numeral:
    text: str


class Pairs(dict):
    def __init__(self, pairs):
        super().__init__(pairs)
        self.typing = [(key, value) for key, value in pairs if key.casefold() in TYPE_FIELDS]


def kept(body):
    try:
        return json.loads(body or b"{}", parse_int=Numeral, parse_float=Numeral, parse_constant=Numeral, object_pairs_hook=Pairs)
    except ValueError as error:
        raise Refused(400, f"the body is not JSON: {error}") from error


def written(value):
    match value:
        case Numeral(text=text):
            return text
        case dict():
            return "{" + ",".join(f"{json.dumps(key)}:{written(item)}" for key, item in value.items()) + "}"
        case list():
            return "[" + ",".join(written(item) for item in value) + "]"
        case _:
            return json.dumps(value)


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


class Hearing:
    def __init__(self, execution):
        self.execution = execution
        self.told = False

    def fresh(self):
        return self.execution.noted is not None and not self.told

    def get(self, timeout=None):
        running = self.execution
        with running.changed:
            if not running.changed.wait_for(lambda: self.fresh() or running.ended.is_set(), timeout):
                raise queue.Empty
            if self.fresh():
                self.told = True
                return "note", running.noted
            return "ended", running.result


class Execution:
    def __init__(self, asked):
        self.asked = asked
        self.ended = threading.Event()
        self.result = None
        self.unswept = 0
        self.noted = None
        self.changed = threading.Condition()

    def end(self, result):
        with self.changed:
            self.result = result
            self.ended.set()
            self.changed.notify_all()

    def note(self, line):
        with self.changed:
            if self.noted is None:
                self.noted = line
                self.changed.notify_all()

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
SHOTS_UNWRITTEN = "shots not written"


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
        return "passed" if ended.group(1) == "0" and f" \u00b7 {SHOTS_UNWRITTEN}: " not in text else "failed"
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
    return files_commit_many(base, message, [(path, size, chunks)])


def files_put(path, size, chunks):
    content = u8(1) + u64(size) + borsh_vec([blob_id(chunk) for chunk in chunks])
    return u8(0) + borsh_text(path) + u8(0) + (0).to_bytes(4, "little") + content


def files_commit_many(base, message, items):
    return u8(1) + borsh_option(base, blob_id) + borsh_text(message) + borsh_vec([files_put(*item) for item in items])


def egress_request(request):
    return u8(3) + u64(request)


def open_of(reply):
    reader = Reader(reply)
    if reader.u8() != 3:
        raise Refused(502, "egress answered a request with something else")
    return reader.u8() == 1


def egress_sender(request):
    return u8(EGRESS_SENDER) + u64(request)


def sender_of(reply):
    reader = Reader(reply)
    if reader.u8() != EGRESS_SENDER:
        raise Refused(502, "egress answered a sender with something else")
    return reader.u64() if reader.u8() == 1 else None


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
    def __init__(self, kubectl, namespace, kept=KEPT):
        self.kubectl = kubectl
        self.namespace = namespace
        self.kept_namespace, self.kept_name = kept

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

    def exec(self, name, container, command):
        chosen = ["--container", container] if container else []
        return self.call("exec", name, *chosen, "--", *command)

    def annotate(self, name, key, value):
        return self.call("annotate", "pod", name, f"{key}={value}", "--overwrite")

    def apply(self, objects, dry):
        listed = {"apiVersion": "v1", "kind": "List", "items": objects}
        tried = ["--dry-run=server"] if dry else []
        return self.call("apply", *tried, "--filename", "-", "--output", "json", stdin=written(listed).encode())

    def held(self, objects):
        listed = {"apiVersion": "v1", "kind": "List", "items": objects}
        return self.call("get", "--ignore-not-found", "--filename", "-", "--output", "json", stdin=written(listed).encode())

    def trial(self, pod):
        return self.call("create", "--dry-run=server", "--filename", "-", "--output", "name", stdin=written(pod).encode())

    def objects(self, kind, name):
        return self.call("get", kind, *([name] if name else []), "--output", "json")

    def claims(self):
        return self.call("get", "persistentvolumeclaims", "--output", "json")

    def quotas(self):
        return self.call("get", "resourcequotas", "--output", "json")

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

    def kept_call(self, *args):
        return run([*self.kubectl, "--namespace", self.kept_namespace, *args])

    def kept_load(self):
        ran = self.kept_call("get", "configmap", self.kept_name, "--output", "json")
        absent = ran.code != 0 and "(NotFound)" in ran.err
        if absent:
            ran = self.kept_call("create", "configmap", self.kept_name, "--output", "json")
        if ran.code != 0:
            raise kube_refusal(ran, f"configmap {self.kept_name} in namespace {self.kept_namespace} was not read")
        return json.loads(ran.out).get("data") or {}

    def kept_change(self, data):
        patch = json.dumps({"data": data})
        return self.kept_call("patch", "configmap", self.kept_name, "--type", "merge", "--patch", patch)


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

    def key(self):
        return public_key((self.keys / "key").read_text())

    def follow(self, program):
        return subprocess.Popen([*self.ducktape, "changes", program], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=self.env())


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


def runner_words(command, name):
    words = plain_words(command[2]) if len(command) == 3 and command[:2] == ["sh", "-c"] else command
    settings = list(itertools.takewhile(SETTING.fullmatch, words or []))
    called = (words or [])[len(settings) :]
    if called[:2] != ["sh", BUILD_SH]:
        raise Refused(
            400,
            f'pod {name} runs build.sh alone: the command ["sh", "{BUILD_SH}", ...], or a script of plain words that calls it (KEY=VALUE words first; no expansion, operator or comment outside quotes), not {shlex.join(command)}',
        )
    return settings + called[2:]


def build_settings(command, scripted):
    words = command
    if scripted and len(command) == 3 and command[:2] == ["sh", "-c"]:
        words = plain_words(command[2]) or []
    before = list(itertools.takewhile(SETTING.fullmatch, words))
    called = words[len(before) :]
    if called[:2] != ["sh", BUILD_SH]:
        return None
    return before + list(itertools.takewhile(SETTING.fullmatch, called[2:]))


def with_setting(command, word):
    if command[:2] == ["sh", BUILD_SH]:
        return [*command[:2], word, *command[2:]]
    return [*command[:2], f"{word} {command[2]}"]


def with_caller(command, caller):
    return with_setting(command, f"{CALLER_WORD}{caller}")


def runner_view(name, said):
    running = {"state": "running", "reason": None, "message": None, "exit_code": None, "started": said.get("started"), "finished": None}
    container = {"name": RUN, "image": None, "command": ["sh", BUILD_SH], "args": [], "workdir": None, "resources": {}, "status": running}
    return {
        "name": name,
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


def runner_free(name, said):
    busy, ran = said.get("busy"), said.get("busy_exit")
    if not isinstance(busy, list) or not all(isinstance(line, str) for line in busy):
        return False, f"the {name}'s ping has busy as a {type(busy).__name__}, not a list of lines"
    if type(ran) is not int:
        return False, f"the {name}'s ping has busy_exit as a {type(ran).__name__}, not a number"
    if ran != 0:
        return False, f"the {name}'s build.sh busy exited {ran}"
    kinds = ["stray" if line.startswith("stray ") else (line.split()[2:3] or [""])[0] for line in busy]
    if "stray" in kinds:
        return False, f"a process no run started holds one of the {name}'s build locks"
    if "quiet" in kinds:
        return False, f"a quiet run holds every heavy slot of the {name}"
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


def named_commit(called):
    if called[:1] == ["apply"]:
        rest, places = called[1:], []
        while rest:
            word, rest = rest[0], rest[1:]
            if word in APPLY_VALUED:
                rest = rest[1:]
            elif not word.startswith("-"):
                places.append(word)
        repo, ref = (places + ["", ""])[:2]
    else:
        repo, ref = (called + ["", ""])[:2]
    return (repo, ref) if repo and COMMIT_ID.fullmatch(ref) else None


def on_machine(machine, ran, started=True):
    out = STARTED.sub(lambda found: b"run " + machine.encode() + b":" + found.group(1), ran.out) if started else ran.out
    return Ran(ran.code, out, f"{BUILDS}: on {machine}\n{ran.err}".rstrip("\n"))


def run_kind(words):
    settings = list(itertools.takewhile(SETTING.fullmatch, words))
    called = words[len(settings) :]
    first = called[0] if called else ""
    if first == "apply":
        return first
    if first in ADDRESSED or first in MACHINE_WIDE or first in UNSTARTING or first == REROUTE or len(called) < 3:
        return None
    return called[2]


def unpacked(words):
    try:
        said = base64.b64decode(words, validate=True).split(b"\0")
    except (binascii.Error, ValueError):
        return []
    return [word.decode(errors="replace") for word in said if word]


def soonest(machines):
    timed = all(here[2] is not None for _, here in machines)
    return min(machines, key=lambda machine: machine[1][2] if timed else machine[1][1])[0], timed


def runners_of(text, signer):
    runners, names = [], set()
    for word in text.split():
        name, equals, keys = word.partition("=")
        if not equals or not RUNNER_NAME.fullmatch(name) or name == BUILDS or name in names or not keys.startswith("/"):
            raise ValueError(f"PODS_RUNNERS names each build runner as <name>=<the absolute path of its keys>, split by whitespace, each name once and none of them {BUILDS}, not {word!r}")
        names.add(name)
        runners.append(Runner(name, Path(keys), signer=signer))
    return runners


def gate_run(argv, env=None):
    done = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, env=env)
    return Ran(done.returncode, done.stdout, done.stderr.decode(errors="replace").strip())


def gate_spawn(argv, env=None):
    return subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)


class Runner:
    def __init__(self, name, keys, home=None, runner=gate_run, spawn=gate_spawn, signer=None):
        self.name = name
        self.keys = keys
        self.home = home
        self.runner = runner
        self.spawn = spawn
        self.signer = signer
        self.lock = threading.Lock()
        self.reached = None
        self.key = None

    def forwarded(self):
        return (self.keys / FORWARD_FILE).is_file()

    def held(self):
        if not all((self.keys / name).is_file() for name in RUNNER_FILES):
            return False
        if self.forwarded():
            return self.signer is not None and self.signer.held()
        return (self.keys / HOSTS_FILE).is_file()

    def takes(self, words):
        kind = run_kind(words)
        listed = self.keys / KINDS_FILE
        if kind is None or not listed.is_file():
            return True
        movable = MOVABLE_WORD in itertools.takewhile(SETTING.fullmatch, words)
        return kind in listed.read_text().split() and (kind != "sh" or movable)

    def hosts(self):
        if self.forwarded():
            return [self.name]
        listed = (self.keys / HOSTS_FILE).read_text().split()
        with self.lock:
            first = self.reached
        return sorted(listed, key=lambda host: host != first)

    def proxy(self):
        said = (self.keys / FORWARD_FILE).read_text().split()
        if len(said) != 2:
            raise Refused(503, f"the {self.name}'s forward file holds {len(said)} words, not a member node's key and the service it forwards, so the {self.name} runner is not reachable")
        return shlex.join([*self.signer.ducktape, "forward", *said]).replace("%", "%%")

    def environ(self):
        return self.signer.env() if self.forwarded() else None

    def identity(self):
        with self.lock:
            if self.key is None:
                home = self.home or Path(tempfile.mkdtemp(prefix=f"{self.name}-"))
                home.mkdir(parents=True, exist_ok=True)
                key = home / "id_ed25519"
                with os.fdopen(os.open(key, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "wb") as file:
                    file.write((self.keys / "id_ed25519").read_bytes())
                self.key = key
            return self.key

    def argv(self, host, words, alive):
        user = (self.keys / "user").read_text().strip()
        options = ["IdentitiesOnly=yes", "BatchMode=yes", "StrictHostKeyChecking=yes", f"UserKnownHostsFile={self.keys / 'known_hosts'}"]
        if self.forwarded():
            options.append(f"ProxyCommand={self.proxy()}")
        if alive is not None:
            options.append(f"ServerAliveInterval={alive}")
        flags = [part for option in options for part in ("-o", option)]
        return ["ssh", "-T", "-F", "/dev/null", *flags, "-i", str(self.identity()), f"{user}@{host}", json.dumps(words)]

    def call(self, words, alive=None, again=True):
        if not self.held():
            raise Refused(503, f"this service holds no key for pod {self.name}, so the {self.name} runner is not reachable")
        failures = []
        for host in self.hosts():
            ran = self.runner(self.argv(host, words, alive), self.environ())
            if ran.code == SSH_FAILED and not again and not ran.err.startswith(UNCONNECTED):
                raise Cut(f"the exec's connection to pod {self.name} at {host} was cut while it ran ({ran.err or 'ssh exited 255'}); its command may still be running there, so it was not run again", ran.out)
            if ran.code == SSH_FAILED:
                failures.append(f"{host}: {ran.err or 'ssh exited 255'}")
                continue
            with self.lock:
                self.reached = host
            if ran.code == RUNNER_DOWN:
                raise Refused(503, f"the {self.name}'s runner is down: {ran.err}")
            return ran
        raise Refused(503, f"the {self.name} is unreachable: {'; '.join(failures) or 'its hosts file names no host'}")

    def ran(self, command, alive=None):
        match command:
            case ["cat", "--", path]:
                return self.call(["get", path])
            case ["sh", "-c", script, "resolve", *words] if script == RESOLVING:
                return self.call(["resolve", *words])
            case ["sh", "-c", script, "pack", *words] if script == PACKING:
                return self.call(["pack", *words])
            case _:
                words = runner_words(command, self.name)
                if not self.takes(words):
                    listed = " ".join((self.keys / KINDS_FILE).read_text().split())
                    raise Refused(400, f"pod {self.name} takes only the run kinds its kinds file lists ({listed}; an sh only with {MOVABLE_WORD}), not build.sh {run_kind(words)}")
                return self.call(["exec", *words], alive, again=False)

    def state(self):
        ran = self.call(["ping"])
        try:
            said = json.loads(ran.out)
        except json.JSONDecodeError as error:
            raise Refused(502, f"the {self.name}'s ping answered {ran.out.decode(errors='replace').strip()!r}, not JSON") from error
        if ran.code != 0 or not said.get("up"):
            raise Refused(503, f"the {self.name}'s runner is not up: {said.get('state') or ran.err or f'ping exited {ran.code}'}")
        return said

    def logs(self, options):
        kept = [option for option in options if option.startswith(("--tail=", "--since="))]
        ran = self.call(["logs", *kept])
        if ran.code != 0:
            raise Refused(502, f"the {self.name}'s logs were not read: {ran.err or f'gate exited {ran.code}'}")
        return ran.out

    def follow(self, alive):
        self.state()
        with self.lock:
            host = self.reached
        return self.spawn(self.argv(host, ["logs", "--tail=0", "--follow"], alive), self.environ())

    def tail(self, since):
        self.state()
        with self.lock:
            host = self.reached
        return self.spawn(self.argv(host, ["logs", f"--since={since}s", "--follow"], None), self.environ())


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
        value = body.get(field)
        wrong_kind = not isinstance(value, kind)
        bool_for_number = kind is not bool and isinstance(value, bool)
        mistyped = field in body and (wrong_kind or bool_for_number)
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
    if asked["program"] == FLEET_PROGRAM:
        stamped = headers.get(FLEET_ASKER)
        asker = account_number(stamped or "")
        if asker is None:
            raise Refused(400, f"a call {FLEET_PROGRAM} relays names its run's asker in {FLEET_ASKER}, which {FLEET_PROGRAM} stamps on every call it relays, and {FLEET_ASKER} {stamped!r} is no account number")
        asked[ASKER] = asker
        for header in (FLEET_RUN, FLEET_ATTEMPT):
            stamp = headers.get(header)
            stamped = account_number(stamp or "")
            if stamped is None:
                raise Refused(400, f"a call {FLEET_PROGRAM} relays names its run in {FLEET_RUN} and {FLEET_ATTEMPT}, which {FLEET_PROGRAM} stamps on every call it relays, and {header} {stamp!r} is no number")
            asked[header] = str(stamped)
    return asked


def unrequested(body, what):
    if isinstance(body, dict) and "requester" in body:
        raise Refused(400, f"{what} names no requester: pods takes the requester from the account egress says sent the request (a call {FLEET_PROGRAM} relays: its {FLEET_ASKER}), so drop the field")
    return body


def labels_for(asked, requester):
    labels = {NOTE + name: asked[name] for name in ASKED}
    if ASKER in asked:
        labels[RUN_LABEL] = asked[FLEET_RUN]
        labels[ATTEMPT_LABEL] = asked[FLEET_ATTEMPT]
    if requester is not None:
        labels[REQUESTER] = str(requester)
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
    found = KUBERNETES_QUANTITY.fullmatch(text.strip(GO_SPACE))
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
    amounts = {group: checked(given[group], AMOUNTS, f"resources' {group}") for group in RESOURCES if group in given}
    return {group: {name: canonical_quantity(amount, f"resources' {group} {name}") for name, amount in held.items()} for group, held in amounts.items()}


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
        sized = {"sizeLimit": str(math.ceil(quantity(scratch["size"])))} if "size" in scratch else {}
        return {"name": f"in-{index}", "emptyDir": {"medium": scratch["medium"], **sized}}
    return claimed(f"in-{index}", labels)


def pod_spec(asked, requester, spec, fetching, namespace):
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
    labels = {**dict([MANAGED]), **labels_for(asked, requester)}
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
    if not isinstance(manifest, dict) or not all(isinstance(manifest.get(field), str) and manifest[field] for field in ("apiVersion", "kind")):
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
    named: str | None


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
    named = "/" + "/".join(segments[: len(segments) - len(rest) + 1]) if rest else None
    subresource = rest[1] if len(rest) > 1 else None
    if subresource == "proxy":
        named = named.rsplit("/", 1)[0] + "/" + proxied_name(rest[0], path)
    return Target(path, resource, subresource, named)


def proxied_name(segment, path):
    parts = segment.split(":")
    scheme, name = (parts[0], parts[1]) if len(parts) == 3 else ("", parts[0])
    valid = len(parts) <= 3 and bool(name) and scheme in PROXY_SCHEMES
    if not valid:
        raise Refused(400, f"{path} names its object as {segment}, and a proxy names it NAME, NAME:PORT or SCHEME:NAME:PORT with scheme http or https, as Kubernetes reads it")
    return name


def object_named(target):
    return f"{target.resource} {target.named.rsplit('/', 1)[-1]}"


def forced_holds(manifest):
    specs = [spec for _, spec in pod_specs(manifest)]
    pods_hold = all(spec.get("priorityClassName") == PRIORITY and spec.get("automountServiceAccountToken") is False for spec in specs)
    account_holds = manifest.get("kind") != "ServiceAccount" or manifest.get("automountServiceAccountToken") is False
    return pods_hold and account_holds


def dry_run(query):
    return "&".join([*([query] if query else []), "dryRun=All"])


def mapping(value):
    return value if isinstance(value, dict) else {}


def listing(value):
    return value if isinstance(value, list) else []


def stamped_places(manifest):
    places = [("the object", manifest)]
    for template, spec in pod_specs(manifest) if isinstance(manifest.get("kind", ""), str) else []:
        if template is not manifest:
            places.append(("its pod template", template))
        for volume in listing(mapping(spec).get("volumes")):
            claim = mapping(mapping(mapping(volume).get("ephemeral")).get("volumeClaimTemplate"))
            places.append((f"volume {mapping(volume).get('name')}'s claim template", claim))
    for claim in listing(mapping(manifest.get("spec")).get("volumeClaimTemplates")):
        places.append((f"claim template {mapping(mapping(claim).get('metadata')).get('name')}", mapping(claim)))
    return places


def stamps(manifest):
    if not isinstance(manifest, dict):
        return {}
    return {
        (where, field, key): value
        for where, held in stamped_places(manifest)
        for field in ("labels", "annotations")
        for key, value in mapping(mapping(held.get("metadata")).get(field)).items()
        if key.startswith(NOTE)
    }


def unmoved(before, after, what):
    moved = sorted(place for place in before.keys() | after.keys() if place not in before or place not in after or before[place] != after[place])
    if moved:
        named = ", ".join(f"{field.removesuffix('s')} {key} on {where}" for where, field, key in moved)
        raise Refused(403, f"{what} would set, change or remove {named}: only the pods service writes {NOTE}* labels and annotations, which say whose object it is")


def held_to(body, version, media, what):
    if version is None:
        return body
    if media == JSON_PATCH:
        return [{"op": "test", "path": "/metadata/resourceVersion", "value": version}, *body]
    meta = body.get("metadata", {}) if isinstance(body, dict) else None
    if not isinstance(meta, dict):
        raise Refused(400, f"{what} or its metadata is not a JSON object, so it could not be held to the object's resourceVersion")
    named = meta.get("resourceVersion", version)
    if named != version:
        shown = named if isinstance(named, str) else written(named)
        raise Refused(409, f"{what} names resourceVersion {shown}, and the object is at {version}: read it again")
    return {**body, "metadata": {**meta, "resourceVersion": version}}


def preconditioned(options, uid, what):
    if not isinstance(options, dict):
        raise Refused(400, f"{what} is not a JSON object, so the delete could not be held to the object's uid")
    held = mapping(options.get("preconditions"))
    named = held.get("uid", uid)
    if named != uid:
        shown = named if isinstance(named, str) else written(named)
        raise Refused(409, f"{what} names uid {shown}, and the object's uid is {uid}: read it again")
    return {**options, "preconditions": {**held, "uid": uid}}


def evicting(eviction, uid):
    if not isinstance(eviction, dict):
        raise Refused(400, "an eviction is a JSON object, so it could not be held to the pod's uid")
    options = eviction.get("deleteOptions")
    return {**eviction, "deleteOptions": preconditioned({} if options is None else options, uid, "the eviction's deleteOptions")}


def owned_only(query, caller):
    pairs = parse_qsl(query, keep_blank_values=True)
    chosen = [value for key, value in pairs if key == "labelSelector" and value]
    kept_pairs = [(key, value) for key, value in pairs if key != "labelSelector"]
    return urlencode([*kept_pairs, ("labelSelector", ",".join([*chosen, f"{REQUESTER}={caller}"]))])


def listed_items(answer):
    return [mapping(item) for item in listing(answer.get("items"))] if answer.get("kind") == "List" else [answer]


def object_key(manifest):
    group = str(manifest.get("apiVersion", "")).rpartition("/")[0]
    return group, manifest.get("kind"), mapping(manifest.get("metadata")).get("name")


def api_refusal(answered, what):
    try:
        said = mapping(json.loads(answered.body)).get("message")
    except ValueError:
        said = None
    return Refused(answered.status, f"{what}: Kubernetes answered {answered.status}" + (f": {said}" if said else ""))


def confined_as_given(manifest, what, cluster_kinds, namespace, labels):
    decoded = as_decoded(manifest, what)[0]
    given = stamps(decoded)
    return confine_manifest(decoded, what, cluster_kinds, namespace, labels), given


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


def unplaced(pod):
    status = pod.get("status", {})
    held = [entry for entry in status.get("conditions", []) if entry.get("type") == "PodScheduled" and entry.get("status") == "False" and entry.get("reason") == UNSCHEDULABLE]
    return held[0].get("message") if held and status.get("phase") == "Pending" else None


def pending_line(name, pod):
    message = unplaced(pod)
    return None if message is None else f"pod {name} is pending, waiting to be placed: {message}"


def pending_said(pod):
    message = unplaced(pod)
    if message is None:
        return None
    meta = pod["metadata"]
    name = meta["name"]
    detail = f"pending, waiting to be placed: {message}"
    return {"group": name, "subject": f"pod {name}", "status": "note", "detail": detail, "concerns": concerning(requester_of(meta.get("labels") or {}))}


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


def account_number(word):
    if not ACCOUNT.fullmatch(word) or int(word) > U64_MAX:
        return None
    return int(word)


def notify_lines(name, stream):
    for raw in stream:
        line = raw.decode(errors="replace").rstrip("\r\n")
        caller = None
        if line.startswith(NOTIFY_FOR):
            named, _, rest = line[len(NOTIFY_FOR):].partition(" ")
            caller = account_number(named)
            if caller is None:
                continue
            line = NOTIFY_LINE + rest
        if not line.startswith(NOTIFY_LINE):
            continue
        channel, _, text = line[len(NOTIFY_LINE):].partition(" ")
        if channel and text.strip():
            yield channel, text.strip(), caller


def report_lines(name, stream):
    for raw in stream:
        line = raw.decode(errors="replace").rstrip("\r\n")
        if not line.startswith(REPORT_LINE):
            continue
        words = line[len(REPORT_LINE):].split()
        if len(words) < 5 or not FULL_COMMIT.fullmatch(words[1]) or words[3] not in REPORT_STATES:
            continue
        repo, commit, context, state, run, *reach = words
        yield {"repo": repo, "commit": commit, "context": context, "state": state, "run": run, "url": f"{name}:{run}", "reach": reach or [""]}


def files_lines(stream):
    for raw in stream:
        line = raw.decode(errors="replace").rstrip("\r\n")
        if not line.startswith(FILES_LINE):
            continue
        words = line[len(FILES_LINE):].split(" ", 2)
        if len(words) == 3 and words[0] and words[1].startswith("/") and words[2].startswith("/"):
            yield {"run": words[0], "tar": words[1], "to": words[2]}


def shot_files(data, to):
    listed = []
    try:
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            for member in archive.getmembers():
                if not member.isfile():
                    continue
                parts = [part for part in PurePosixPath(member.name).parts if part != "."]
                if not parts or parts[0] == "/" or ".." in parts:
                    raise Refused(400, f"the shots' tar holds {member.name!r}, which is no path under {to}")
                listed.append((f"{to.rstrip('/')}/{'/'.join(parts)}", archive.extractfile(member).read()))
    except tarfile.TarError as error:
        raise Refused(400, f"the shots are no tar: {error}") from error
    return sorted(listed)


def lane_record_holds(said, record):
    lines = record.splitlines()
    named = f"run {said['run']}: {said['repo']} {said['commit']}, slot "
    lane = any(line.startswith(named) and line.endswith(", lane") for line in lines)
    if said["state"] == "passed":
        ended = "GATES ok" in lines
    else:
        ended = any(line.startswith("GATES FAIL:") for line in lines)
    reach = " ".join(path for path in said["reach"] if path) or "the whole tree"
    recorded = any(f"{lead} build.sh#lane {said['state']}, reach: {reach}" in lines for lead in ("report sent:", "report:"))
    return lane and ended and recorded


def reach_report(said):
    reach = borsh_vec([borsh_text(path) for path in said["reach"]])
    head = borsh_text(said["repo"]) + borsh_text(said["commit"]) + borsh_text(said["context"])
    return u8(1) + head + u8(REPORT_STATES[said["state"]]) + borsh_option(said["url"], borsh_text) + reach + borsh_option(None, u64)


def reach_reports(repo, commit, after):
    return u8(0) + borsh_text(repo) + borsh_text(commit) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def reports_of(reply):
    reader = Reader(reply)
    if reader.u8() != 0:
        raise Refused(502, "forge-reach answered its reports with something else")
    reader.u64()
    found = []
    for _ in range(int.from_bytes(reader.take(4), "little")):
        reader.u64()
        context = reader.bytes().decode(errors="replace")
        reader.u8()
        url = reader.bytes().decode(errors="replace") if reader.u8() == 1 else None
        reader.u64()
        for _ in range(int.from_bytes(reader.take(4), "little")):
            reader.bytes()
        found.append((context, url))
    return found, reader.bytes() if reader.u8() == 1 else None


def public_key(line):
    text = line.strip()
    body = text.removeprefix(USER_KEY)
    try:
        blob = base64.b64decode(body + "=" * (-len(body) % 4), altchars=b"-_", validate=True) if text.startswith(USER_KEY) else b""
    except (binascii.Error, ValueError):
        blob = b""
    if len(blob) != USER_KEY_LEN:
        raise Refused(500, f"the service's key is not a {USER_KEY} key")
    return blob[PUBKEY_AT : PUBKEY_AT + 32]


def identity_of_key(key):
    return u8(0) + borsh_bytes(key)


def account_of(reply):
    reader = Reader(reply)
    if reader.u8() != 0:
        raise Refused(502, "identity answered OfKey with something else")
    return reader.u64() if reader.u8() == 1 else None


def forge_repos(after):
    return u8(0) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def forge_required(repo, after):
    return u8(20) + borsh_text(repo) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def forge_queue_front(repo, branch):
    return u8(21) + borsh_text(repo) + borsh_bytes(branch) + borsh_option(None, borsh_bytes) + borsh_option(1, u64)


def forge_preview(repo, n):
    return u8(18) + borsh_text(repo) + u64(n)


def forge_statuses(repo, commit, after):
    return u8(19) + borsh_text(repo) + borsh_text(commit) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def forge_answer(reply, tag, query, paged):
    reader = Reader(reply)
    if reader.u8() != tag:
        raise Refused(502, f"forge answered {query} with something else")
    reader.u64()
    if paged:
        reader.u64()
    return reader


def page_count(reader):
    return int.from_bytes(reader.take(4), "little")


def page_next(reader):
    return reader.bytes() if reader.u8() == 1 else None


def text_at(reader):
    return reader.bytes().decode(errors="replace")


def forge_principal(reader):
    match reader.u8():
        case 0:
            return ("account", reader.u64())
        case 1:
            return ("root",)
        case 2:
            return ("foreign", reader.take(32).hex(), reader.u64())
        case other:
            raise Refused(502, f"a reply names principal kind {other}")


def repos_of(reply):
    reader = forge_answer(reply, 0, "Repos", True)
    names = []
    for _ in range(page_count(reader)):
        names.append(text_at(reader))
        reader.u8()
        forge_principal(reader)
        reader.bytes()
        reader.take(2)
        reader.u64()
        reader.u64()
    return names, page_next(reader)


def required_of(reply):
    reader = forge_answer(reply, 18, "Required", True)
    branches = []
    for _ in range(page_count(reader)):
        branch = reader.bytes()
        reporter = forge_principal(reader)
        contexts = [text_at(reader) for _ in range(page_count(reader))]
        branches.append({"branch": branch, "reporter": reporter, "contexts": contexts})
    return branches, page_next(reader)


def queue_front_of(reply):
    reader = forge_answer(reply, 19, "Queue", True)
    return reader.u64() if page_count(reader) > 0 else None


def preview_of(reply):
    reader = forge_answer(reply, 16, "Preview", False)
    if reader.u8() != 1:
        return None
    reader.u64()
    text_at(reader)
    text_at(reader)
    return text_at(reader)


def statuses_of(reply):
    reader = forge_answer(reply, 17, "Statuses", True)
    found = []
    for _ in range(page_count(reader)):
        reporter = forge_principal(reader)
        context = text_at(reader)
        state = reader.u8()
        if state >= len(CHECK_STATES):
            raise Refused(502, f"forge names check state {state}")
        if reader.u8() == 1:
            reader.bytes()
        reader.u64()
        found.append({"reporter": reporter, "context": context, "state": CHECK_STATES[state]})
    return found, page_next(reader)


def repo_of_key(word):
    try:
        key = bytes.fromhex(word)
    except ValueError:
        return None
    repo, end, _ = key[2:].partition(b"\x00\x00")
    return repo.decode(errors="replace") if end and repo else None


def rerun_key(repo, commit):
    return RERUN + hashlib.sha256(f"{repo}\n{commit}".encode()).hexdigest()


def rerun_command(row):
    room = f"forge:{row['repo']}:{row['n']}"
    first_limit = [f"FIRST_LIMIT={LANE_FIRST_LIMIT}"] if LANE_FIRST_LIMIT else []
    return ["sh", BUILD_SH, f"JOB={room}", f"NOTIFY={room}", *first_limit, row["repo"], row["commit"], "lane"]


def front_touched(line):
    words = line.decode(errors="replace").rstrip("\r\n").split("\t")
    if len(words) != 3 or not words[1].startswith(FRONT_KEYS):
        return None
    repo = repo_of_key(words[1])
    return None if repo is None else (words[0], repo)


def run_lines(stream):
    for raw in stream:
        line = raw.decode(errors="replace").rstrip("\r\n")
        if not line.startswith(RUN_LINE):
            continue
        words = line[len(RUN_LINE):].split(" ", 3)
        if words[0] == "armed" and len(words) >= 3 and words[1] and words[2]:
            yield "armed", words[2], words[1], words[3].strip() if len(words) > 3 else ""
        elif words[0] == "done" and len(words) >= 2:
            yield "done", words[1].strip(), None, None
        elif words[0] == "argv" and len(words) >= 3:
            yield "argv", words[1], None, words[2].strip()


def pending_key(name, run):
    return f"pending-{kept_key(name, run)}"


def recalled_id(name, channel, run):
    digest = hashlib.sha256(f"{channel}\n{run}".encode()).hexdigest()
    return f"pods-{name}-recalled-{digest}"


def kept_key(name, run):
    return hashlib.sha256(f"{name}\n{run}".encode()).hexdigest()


def lost_id(name, channel, run):
    digest = hashlib.sha256(f"{channel}\n{run}".encode()).hexdigest()
    return f"pods-{name}-lost-{digest}"


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


def wait_every(query, headers):
    unknown = sorted(set(query) - WAIT)
    if unknown:
        raise Refused(400, f"a wait takes no {', '.join(unknown)}; it takes every and log")
    if "every" in query:
        return seconds(query["every"][-1], "every")
    every = header_every(headers)
    if every is None:
        raise Refused(400, "a wait names every, in its query or as its every header: the seconds between the alive events that keep the caller's request from expiring")
    return every


def seconds(value, field):
    if not value.isdigit():
        raise Refused(400, f"{field} is a whole number of seconds from 1")
    return from_one(int(value), field)


def header_every(headers):
    if "every" not in headers:
        return None
    return seconds(headers["every"], "the every header")


def wait_log(query, headers):
    if "log" in query:
        return query["log"][-1]
    return headers.get("log")


def exec_every(asked, headers):
    if "every" in asked:
        return from_one(asked["every"], "every")
    return header_every(headers)


def exec_command(asked):
    match asked:
        case {"command": _, "script": _}:
            raise Refused(400, "an exec names its command or its script, not both")
        case {"script": ""}:
            raise Refused(400, "script holds shell text")
        case {"script": script}:
            return ["sh", "-c", script]
        case _:
            return strings(asked.get("command", []), "command")


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
    def __init__(self, every, said=dict):
        self.every = every
        self.due = time.monotonic() + every
        self.said = said

    def heard(self, events):
        while True:
            try:
                return events.get(timeout=max(self.due - time.monotonic(), 0))
            except queue.Empty:
                self.due += self.every
                yield sse("alive", self.said())


class LogReads:
    def __init__(self, read):
        self.read = read
        self.lock = threading.Lock()
        self.latest = {}
        self.reading = False
        self.said()

    def said(self):
        with self.lock:
            latest, idle = self.latest, not self.reading
            self.reading = True
        if idle:
            threading.Thread(target=self.reread, daemon=True).start()
        return latest

    def reread(self):
        answer = self.read()
        with self.lock:
            self.latest, self.reading = answer, False


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


def read_answer(name, path, offset, ran):
    stderr = reached(name, ran).strip()
    if ran.code != 0:
        status = 404 if "No such file" in stderr else 400
        raise Refused(status, f"{path} was not read: {stderr or f'exit code {ran.code}'}")
    line, newline, data = ran.out.partition(b"\n")
    sized = bool(newline) and line.strip().isdigit()
    if not sized:
        raise Refused(502, f"reading {path} printed no size line first: {ran.out[:80]!r}")
    size = int(line)
    return {"path": path, "size": size, "offset": offset, "length": len(data), "eof": offset + len(data) >= size, **text_or_hex(data, "text")}


def pkt_line(data):
    return f"{len(data) + 4:04x}".encode() + data


def pkt_lines(data):
    lines, at = [], 0
    while at < len(data):
        head = data[at : at + 4]
        size = int(head, 16) if re.fullmatch(rb"[0-9a-f]{4}", head) else None
        framed = size is not None and (size == 0 or 4 <= size <= len(data) - at)
        if not framed:
            raise Refused(400, f"the report is no git report: {data[:80]!r}")
        if size:
            lines.append(data[at + 4 : at + size].rstrip(b"\n").decode(errors="replace"))
        at += size or 4
    return lines


def forge_blob(blob):
    if blob is None:
        return None
    kind, _, digest = blob.partition(":")
    return {kind: digest}


def branch_head(ref, heads):
    for head in heads:
        listed = isinstance(head, dict) and isinstance(head.get("name"), str) and isinstance(head.get("target"), str)
        if not listed:
            raise Refused(400, "heads holds forge's RefNames items, each {name, target}")
        if head["name"] != ref:
            continue
        if not OID.fullmatch(head["target"]):
            raise Refused(400, f"heads names {ref} at {head['target']!r}, not a commit id")
        return head["target"]
    return None


class Onto:
    def __init__(self, branch, heads, allow_stack):
        self.ref = f"refs/heads/{branch}"
        self.old = branch_head(self.ref, heads)
        self.allow_stack = allow_stack

    def have(self):
        return [] if self.old is None else [self.old]

    def lands(self, commit, parents, commits):
        unchanged = commit == self.old
        lands_as_asked = self.allow_stack or unchanged
        if lands_as_asked:
            return
        parent = parents[0] if parents else "no parent"
        asked = "allow_stack: true is for a push like that made on purpose, such as carrying someone's commits"
        moved_on = self.old is not None and parent != self.old
        if moved_on:
            raise Refused(409, f"{commit} is not a child of {self.ref}'s head {self.old}: its parent is {parent}. Rebase it onto {self.old}, or push {parent} first if it belongs there; {asked}")
        if commits > 1:
            raise Refused(409, f"{commit} stacks on {parent}, which forge does not hold yet: the push would land {commits} commits on the new branch {self.ref}. Push {parent} first, or rebase {commit} onto a commit forge holds, like dev's head; {asked}")

    def answered(self, answer):
        commit = answer["commit"]
        old = self.old or "0" * len(commit)
        request = pkt_line(f"{old} {commit} {self.ref}".encode() + b"\0report-status") + FLUSH
        forge = {"request": request.hex(), "pack": forge_blob(answer["pack"])}
        return {**answer, "branch": self.ref, "old": self.old, "unchanged": commit == self.old, "forge": forge}


def pushed_onto(asked):
    if "branch" not in asked:
        unbranched = [field for field in ("heads", "allow_stack") if field in asked]
        if unbranched:
            raise Refused(400, f"a push names {unbranched[0]} only with its branch")
        return None
    branch = asked["branch"]
    if "have" in asked:
        raise Refused(400, "a push names its branch or its have, not both: with a branch, the branch's head in heads is what forge has")
    plain = bool(branch) and not branch.startswith(("refs/", "-"))
    if not plain:
        raise Refused(400, f"branch {branch!r} is a branch's name under refs/heads/, like j-1/x")
    if "heads" not in asked:
        raise Refused(400, f"a push to a branch names heads: the items of forge's RefNames for the prefix refs/heads/{branch}")
    return Onto(branch, asked["heads"], asked.get("allow_stack", False))


def onto_answer(onto, answer):
    return answer if onto is None else onto.answered(answer)


def report_answer(report, push):
    said = {"pushed": push.get("branch"), "head": push.get("commit"), "old": push.get("old"), "objects": push.get("objects")}
    if push.get("unchanged") is True:
        return {**said, "unchanged": True}
    try:
        data = bytes.fromhex(report)
    except ValueError as error:
        raise Refused(400, "report is forge's Push answer as hex, as an op step reads it") from error
    for line in pkt_lines(data):
        match line.split(" ", 1):
            case ["unpack", "ok"]:
                continue
            case ["unpack", why]:
                raise Refused(422, f"forge took none of the pack: {why}")
            case ["ok", ref]:
                return {**said, "pushed": ref}
            case _:
                raise Refused(409, f"forge did not move the branch: {line}")
    raise Refused(502, f"forge's report names no ref: {report}")


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


def reading(text):
    found = KUBERNETES_QUANTITY.fullmatch(text.strip(GO_SPACE))
    if found is None:
        return None
    sign, digits, exponent, suffix = found.groups()
    power = parsed_exponent(exponent or "0")
    if power is None:
        return None
    power = (power + INT32) % (2 * INT32) - INT32
    amount = Decimal(f"{digits}e{power}")
    scale = SCALE[suffix or ""]
    form = "exponent" if exponent is not None else "binary" if (suffix or "").endswith("i") else "decimal"
    if amount.is_zero():
        return Fraction(0), form
    if amount.adjusted() + min(SCALE.values()).adjusted() > MOST.adjusted():
        value = MOST
    elif amount.adjusted() + max(SCALE.values()).adjusted() + 2 <= NANO.adjusted():
        value = NANO
    else:
        exact = decimal.Context(prec=decimal.MAX_PREC, Emax=decimal.MAX_EMAX, Emin=decimal.MIN_EMIN, rounding=decimal.ROUND_UP)
        value = min(exact.multiply(amount, scale).quantize(NANO, context=exact), MOST)
    return Fraction(-value if sign == "-" else value), form


def quantity(text):
    read = reading(str(text))
    if read is None:
        raise Refused(502, f"{text} is not a Kubernetes quantity")
    return read[0]


def canonical(value, form):
    if value == 0:
        return "0"
    sign, size = "-" if value < 0 else "", abs(value)
    if form == "binary" and size >= 1024 and size.denominator == 1:
        amount, power = size.numerator, 0
        while amount % 1024 == 0:
            amount, power = amount // 1024, power + 1
        return f"{sign}{amount}{BINARY_SI[power]}"
    amount, power = int(size / Fraction(NANO)), -9
    while amount % 10 == 0:
        amount, power = amount // 10, power + 1
    while power % 3:
        amount, power = amount * 10, power - 1
    suffix = DECIMAL_SI[power] if form != "exponent" else f"e{power}" if power else ""
    return f"{sign}{amount}{suffix}"


def canonical_quantity(text, what):
    read = reading(text)
    if read is None:
        raise Refused(400, f"{what} is not a Kubernetes quantity such as 48Gi")
    return canonical(*read)


def field_of(where):
    what, *steps = where
    path = "".join(f"[{step}]" if isinstance(step, int) else f".{step}" for step in steps).lstrip(".")
    return f"{what}'s {path}" if path else what


def canonical_at(value, where):
    match value:
        case Numeral(text=text) | str(text):
            return canonical_quantity(text, field_of(where))
        case _:
            return value


def referred(node):
    refs = [node["$ref"]] if "$ref" in node else [part["$ref"] for part in node.get("allOf") or [] if isinstance(part, dict) and "$ref" in part]
    return refs[0].removeprefix(SCHEMA_REF) if len(refs) == 1 else None


def pruned(node, reaching):
    if not isinstance(node, dict):
        return None
    name = referred(node)
    if name is not None:
        return name if name in reaching else None
    declared = node.get("properties") or {}
    properties = {key: shape for key, child in declared.items() if (shape := pruned(child, reaching)) is not None}
    items, values = pruned(node.get("items"), reaching), pruned(node.get("additionalProperties"), reaching)
    shape = {"properties": properties} if properties else {}
    if items is not None:
        shape["items"] = items
    if values is not None:
        shape |= {"values": values, "declared": frozenset(declared)}
    return shape or None


def step(shape, key):
    properties = shape.get("properties") or {}
    if key in properties:
        return properties[key]
    if key in shape.get("declared", ()):
        return None
    return shape.get("values")


def content_schema(content):
    names = {referred(media.get("schema") or {}) for media in (content or {}).values() if isinstance(media, dict)} - {None}
    return names.pop() if len(names) == 1 else None


@dataclass(frozen=True)
class Operation:
    body: bool
    kind: tuple | None


def gvk_of(held):
    return (held.get("group", ""), held.get("version"), held.get("kind"))


def operations_of(item, schema_kinds):
    found = {}
    for method in WRITES:
        operation = item.get(method.lower()) if isinstance(item, dict) else None
        if not isinstance(operation, dict):
            continue
        content = (operation.get("requestBody") or {}).get("content")
        name = content_schema(content)
        if name == PATCH_SCHEMA:
            name = content_schema(((operation.get("responses") or {}).get("200") or {}).get("content"))
        stated = operation.get("x-kubernetes-group-version-kind")
        found[method] = Operation(bool(content), gvk_of(stated) if isinstance(stated, dict) else schema_kinds.get(name))
    return found


def fitted(template, segments):
    if template and template[-1] == "{path}" and len(segments) > len(template):
        segments = [*segments[: len(template) - 1], "/".join(segments[len(template) - 1 :])]
    if len(template) != len(segments) or not all(part == segment or part.startswith("{") for part, segment in zip(template, segments)):
        return None
    return sum(part == segment for part, segment in zip(template, segments))


def pointer(path):
    if not isinstance(path, str) or (path and not path.startswith("/")):
        return None
    return [token.replace("~1", "/").replace("~0", "~") for token in path.split("/")[1:]]


class Published:
    def __init__(self, document):
        schemas = {name: schema for name, schema in ((document.get("components") or {}).get("schemas") or {}).items() if isinstance(schema, dict)}
        reaching = {QUANTITY_SCHEMA}
        while more := {name for name, schema in schemas.items() if name not in reaching and pruned(schema, reaching) is not None}:
            reaching |= more
        self.shapes = {name: pruned(schemas[name], reaching) for name in reaching if name in schemas and name != QUANTITY_SCHEMA}
        named = [
            (gvk_of(held), name)
            for name, schema in schemas.items()
            for held in schema.get("x-kubernetes-group-version-kind") or []
            if isinstance(held, dict)
        ]
        self.kinds = dict(named)
        schema_kinds = {name: kind for kind, name in named}
        self.operations = [(path.strip("/").split("/"), found) for path, item in (document.get("paths") or {}).items() if (found := operations_of(item, schema_kinds))]

    def operation(self, segments, method):
        fitting = [(literal, found[method]) for template, found in self.operations if method in found and (literal := fitted(template, segments)) is not None]
        return max(fitting, key=lambda fit: fit[0])[1] if fitting else None

    def canonicalised(self, value, shape, where, directives=False):
        if isinstance(shape, str):
            if shape == QUANTITY_SCHEMA:
                return canonical_at(value, where)
            shape = self.shapes.get(shape)
        if shape is None:
            return value
        if isinstance(value, dict):
            return {key: item if directives and key.startswith(DIRECTIVE) else self.canonicalised(item, step(shape, key), (*where, key), directives) for key, item in value.items()}
        if isinstance(value, list) and "items" in shape:
            return [self.canonicalised(item, shape["items"], (*where, index), directives) for index, item in enumerate(value)]
        return value

    def resolved(self, shape):
        return self.shapes.get(shape, shape) if isinstance(shape, str) else shape

    def at(self, shape, tokens):
        for token in tokens:
            if isinstance(shape, str):
                shape = None if shape == QUANTITY_SCHEMA else self.shapes.get(shape)
            if shape is None:
                return None
            shape = shape["items"] if "items" in shape and (token == "-" or token.isdigit()) else step(shape, token)
        return shape if not isinstance(shape, str) or shape == QUANTITY_SCHEMA or shape in self.shapes else None

    def patched(self, operation, name, index):
        if not isinstance(operation, dict) or operation.get("op") not in (*PATCHED_VALUES, *COPIED_VALUES):
            return operation
        tokens = pointer(operation.get("path"))
        shape = None if tokens is None else self.at(name, tokens)
        if shape is None:
            return operation
        if operation["op"] in COPIED_VALUES:
            source = pointer(operation.get("from"))
            if self.resolved(None if source is None else self.at(name, source)) != self.resolved(shape):
                raise Refused(
                    400,
                    f"the patch's operation {index} {operation['op']}s {operation.get('from')!r} to {operation['path']!r}, where Kubernetes reads quantities it doesn't read at {operation.get('from')!r}: write the quantity as a value (add or replace)",
                )
            return operation
        if "value" not in operation:
            return operation
        return {**operation, "value": self.canonicalised(operation["value"], shape, (f"the patch's operation {index}", *tokens))}


class Schemas:
    def __init__(self, api):
        self.api = api
        self.held = {}

    def reading(self):
        return Reading(self)

    def read(self, path, query, what):
        answered = self.api.call("GET", path, query, b"", None)
        if answered.status != 200:
            raise Refused(502, f"the API server's {what} was not read: it answered {answered.status}")
        return json.loads(answered.body)

    def index(self):
        listed = self.read(OPENAPI, "", "OpenAPI index").get("paths") or {}
        index = {group_version: entry.get("serverRelativeURL") for group_version, entry in listed.items()}
        live = set(index.values())
        self.held = {url: published for url, published in self.held.items() if url in live}
        return index

    def document(self, index, group_version):
        url = index.get(group_version)
        if url is None:
            raise Refused(400, f"the API server publishes no schema for {group_version}")
        if url not in self.held:
            path, _, query = url.partition("?")
            self.held[url] = Published(self.read(path, query, f"OpenAPI schema of {group_version}"))
        return self.held[url]


class Reading:
    def __init__(self, schemas):
        self.schemas = schemas
        self.index = None

    def published(self, group_version):
        if self.index is None:
            self.index = self.schemas.index()
        return self.schemas.document(self.index, group_version)

    def schema(self, kind, home, what):
        name = None if home is None else home.kinds.get(kind)
        if name is not None:
            return home, name
        group, version, named = kind
        published = self.published(f"apis/{group}/{version}" if group else f"api/{version}")
        name = published.kinds.get(kind)
        if name is None:
            raise Refused(400, f"{what} is a {api_version(kind)} {named}, which the API server publishes no schema for")
        return published, name

    def named_path(self, kind, name, namespace, what):
        group, version, _ = kind
        published = self.published(f"apis/{group}/{version}" if group else f"api/{version}")
        paths = [template for template, found in published.operations if "PUT" in found and found["PUT"].kind == kind and len(template) > 3 and template[-1] == "{name}" and template[-3] == "{namespace}"]
        if not paths:
            raise Refused(400, f"the API server publishes no path for {what}, a {api_version(kind)} {kind[2]}, so it could not be deleted")
        filled = {"{namespace}": namespace, "{name}": quote(name, safe="")}
        return "/" + "/".join(filled.get(part, part) for part in paths[0])

    def object(self, manifest, what):
        group, _, version = manifest["apiVersion"].rpartition("/")
        published, name = self.schema((group, version, manifest["kind"]), None, what)
        return published.canonicalised(manifest, name, (what,))

    def operation(self, segments, method):
        home = self.published("/".join(segments[: 2 if segments[0] == "api" else 3]))
        operation = home.operation(segments, method)
        if operation is None:
            raise Refused(400, f"the API server publishes no {method} of /{'/'.join(segments)}, so the body's quantities could not be read")
        if operation.body and operation.kind is None:
            raise Refused(400, f"the API server publishes no kind for {method} /{'/'.join(segments)}, so the body's quantities could not be read")
        return home, operation

    def body(self, home, operation, media, given):
        endpoint = operation.kind
        if media == JSON_PATCH:
            if not isinstance(given, list):
                raise Refused(400, "a JSON patch is a list of operations")
            kept_kind(given, endpoint)
            published, name = self.schema(endpoint, home, "the patched object")
            return [published.patched(change, name, index) for index, change in enumerate(given)]
        if media in (STRATEGIC_PATCH, APPLY_PATCH):
            kind = endpoint
        elif media == MERGE_PATCH:
            given, kind = merged(given, endpoint)
        else:
            given, found = as_decoded(given, "the body")
            kind = decoded_kind(found, endpoint, "the body")
        published, name = self.schema(kind, home, "the body")
        return published.canonicalised(given, name, ("the body",), media == STRATEGIC_PATCH)


def api_version(kind):
    group, version, _ = kind
    return f"{group}/{version}" if group else version


def stated_types(pairs, what):
    found = {}
    for key, value in pairs:
        if value is None:
            continue
        if not isinstance(value, str):
            raise Refused(400, f"{what}'s {key!r} is not a string, and Kubernetes reads it as the object's {TYPE_FIELDS[key.casefold()]}")
        found[TYPE_FIELDS[key.casefold()]] = value
    return found


def decoded_kind(found, default, what):
    stated = found.get("apiVersion", "")
    if stated.count("/") > 1:
        raise Refused(400, f"{what}'s apiVersion {stated!r} is not GROUP/VERSION")
    group, _, version = stated.rpartition("/")
    if not version and (not group or group == default[0]):
        group, version = default[0], default[1]
    return group, version, found.get("kind") or default[2]


def untyped(body):
    return {key: value for key, value in body.items() if key.casefold() not in TYPE_FIELDS}


def as_decoded(body, what):
    if not isinstance(body, Pairs):
        return body, {}
    found = stated_types(body.typing, what)
    return {**found, **untyped(body)}, found


def merged(patch, endpoint):
    if not isinstance(patch, Pairs):
        return patch, endpoint
    live = {"apiVersion": api_version(endpoint), "kind": endpoint[2]}
    held = dict(live)
    for key, value in patch.typing:
        if value is None:
            held.pop(key, None)
        else:
            held[key] = value
    kind = decoded_kind(stated_types(sorted(held.items()), "the patch"), endpoint, "the patch")
    decoded = {"apiVersion": api_version(kind), "kind": kind[2]}
    return {**{field: value for field, value in decoded.items() if value != live[field]}, **untyped(patch)}, kind


def kept_kind(operations, endpoint):
    held = {"apiVersion": api_version(endpoint), "kind": endpoint[2]}
    for index, change in enumerate(operations):
        if not isinstance(change, dict):
            continue
        op, tokens = change.get("op"), pointer(change.get("path"))
        taken = pointer(change.get("from")) if op == "move" else None
        changed = taken is not None and len(taken) == 1 and taken[0].casefold() in TYPE_FIELDS
        if changed:
            held.pop(taken[0], None)
        if tokens is not None and op in (*PATCHED_VALUES, *COPIED_VALUES, "remove"):
            if not tokens and op in COPIED_VALUES:
                raise Refused(400, f"the patch's operation {index} {op}s a value to the whole document, whose type then can't be read: write the document as a value (replace)")
            if not tokens and op in PATCHED_VALUES and isinstance(change.get("value"), Pairs):
                held, changed = dict(change["value"].typing), True
            elif len(tokens) == 1 and tokens[0].casefold() in TYPE_FIELDS:
                if op in COPIED_VALUES:
                    raise Refused(400, f"the patch's operation {index} {op}s a value to {change['path']!r}, the object's type: write the type as a value (add or replace)")
                if op == "remove":
                    held.pop(tokens[0], None)
                else:
                    held[tokens[0]] = change.get("value")
                changed = True
        if not changed:
            continue
        what = f"the patch's operation {index}"
        kind = decoded_kind(stated_types(sorted(held.items()), what), endpoint, what)
        if kind != endpoint:
            raise Refused(400, f"{what} makes the object a {api_version(kind)} {kind[2]}, where the endpoint holds a {api_version(endpoint)} {endpoint[2]}: Kubernetes refuses that patch")


def media_of(kind):
    media = (kind or JSON_BODIES[0]).split(";")[0].strip().lower()
    if media not in JSON_BODIES:
        raise Refused(415, f"a {media} body is refused: the service reads each quantity in a body before Kubernetes does, and it reads JSON only ({', '.join(JSON_BODIES)})")
    return media


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


def owning_pod(claim):
    owners = (claim.get("metadata") or {}).get("ownerReferences") or []
    return next((owner.get("name") for owner in owners if owner.get("kind") == "Pod" and owner.get("controller")), None)


def rootfs_read_at(entry):
    times = [(container.get("rootfs") or {}).get("time") for container in entry.get("containers") or []]
    return min(times) if times and all(times) else None


def claim_row(claim, phases):
    pod = owning_pod(claim)
    return {
        "claim": claim["metadata"]["name"],
        "requester": requester_of(claim["metadata"].get("labels") or {}),
        "storage_requested_bytes": int(quantity(((claim.get("spec") or {}).get("resources") or {}).get("requests", {}).get("storage", "0"))),
        "pod": pod,
        "phase": phases.get(pod),
    }


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
                "disk_read_at": rootfs_read_at(entry),
            }
        )
    phases = {pod["metadata"]["name"]: pod.get("status", {}).get("phase") for pod in pods}
    held = [claim_row(claim, phases) for claim in claims]
    totals = {}
    fields = ("cpu_limit", "memory_limit_bytes", "cpu_nanocores", "memory_bytes", "disk_bytes", "storage_requested_bytes")
    for kind, listed in (("pods", rows), ("claims", held)):
        for row in listed:
            total = totals.setdefault(row["requester"], {"requester": row["requester"], "pods": 0, "claims": 0, **{field: 0 for field in fields}})
            total[kind] += 1
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
        self.reported = {}
        self.outcomes = {}
        self.shots = {}
        self.filed = {}
        self.fetched = None
        self.records = None
        self.open = {}
        self.done = {}
        self.deleting = set()
        self.resumed = False
        self.recall = None
        self.pending = {}
        self.keeping = threading.Lock()
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
            channel, text, _ = said[newest]
            asked += 1
            try:
                held = message_held(self.asked(chat_message(line_id(name, channel, text))))
            except Refused as refused:
                return 0, asked, refused.sentence
            except Unanswered as why:
                return 0, asked, str(why)
            if held:
                with self.lock:
                    self.posted.setdefault(name, set()).update(line_id(name, channel, text) for channel, text, _ in said[: newest + 1])
                return newest + 1, asked, None
        return 0, asked, None

    def reporting(self, name, stream):
        for raw in stream:
            for said in report_lines(name, [raw]):
                self.report(name, said)
            yield raw

    def report(self, name, said):
        with self.lock:
            if said["url"] in self.reported.get(name, ()):
                return
        if not self.signer.held():
            say("pods_unreported", run=json.dumps(said["url"]), reason="no_key")
            self.remember(name, said, "report not landed: no_key")
            return
        try:
            record = self.records(name, said["run"]) if self.records else None
        except Refused as refused:
            say("pods_unreported", run=json.dumps(said["url"]), reason="unread", detail=json.dumps(refused.sentence))
            self.remember(name, said, f"report not landed: unread: {refused.sentence}")
            return
        if record is None or not lane_record_holds(said, record):
            why = f"{name} records no lane run {said['run']} of {said['repo']} {said['commit']} that ended {said['state']} with that reach"
            say("pods_unreported", run=json.dumps(said["url"]), reason="unrecorded", detail=json.dumps(why))
            self.remember(name, said, f"report not landed: unrecorded: {why}")
            with self.lock:
                self.reported.setdefault(name, set()).add(said["url"])
            return
        pause = self.retry_pause
        attempts = 0
        while True:
            try:
                settled = self.report_once(name, said)
                break
            except Unanswered as why:
                attempts += 1
                if attempts & (attempts - 1) == 0:
                    say("pods_retried", attempts=attempts, reason="node_unanswered", run=json.dumps(said["url"]), detail=json.dumps(str(why)))
                self.pause(pause)
                pause *= 2
        if settled:
            with self.lock:
                self.reported.setdefault(name, set()).add(said["url"])

    def report_once(self, name, said):
        outcome, reason, sentence, _ = receipt(self.signer.submit(FORGE_REACH, reach_report(said)))
        if outcome == "applied":
            say("pods_reported", run=json.dumps(said["url"]), commit=said["commit"], state=said["state"])
            self.remember(name, said, f"report landed: {said['repo']} {said['commit']} {said['context']} {said['state']}")
            return True
        if outcome in ("unknown", "unlanded") or (outcome == "failed" and unanswered(sentence)):
            raise Unanswered(sentence)
        say("pods_unreported", run=json.dumps(said["url"]), reason=reason or outcome, detail=json.dumps(sentence))
        self.remember(name, said, f"report not landed: {reason or outcome}: {sentence}")
        return outcome == "rejected"

    def remember(self, name, said, outcome):
        with self.lock:
            self.outcomes.setdefault(name, {})[said["run"]] = outcome

    def told(self, name, text):
        run = RUN_NAMED.match(text)
        with self.lock:
            notes = [] if run is None else [held.get(name, {}).get(run.group(1)) for held in (self.outcomes, self.shots)]
        return " \u00b7 ".join([text, *[note for note in notes if note is not None]])

    def filing(self, name, stream):
        for raw in stream:
            for said in files_lines([raw]):
                self.file(name, said)
            yield raw

    def file(self, name, said):
        key = (said["run"], said["tar"])
        with self.lock:
            if key in self.filed.get(name, ()):
                return
        try:
            outcome = self.filed_outcome(name, said)
        except Refused as refused:
            say("pods_unfiled", run=json.dumps(f"{name}:{said['run']}"), reason="refused", detail=json.dumps(refused.sentence))
            outcome = f"{SHOTS_UNWRITTEN}: {refused.sentence}"
        with self.lock:
            self.filed.setdefault(name, set()).add(key)
            self.shots.setdefault(name, {})[said["run"]] = outcome

    def filed_outcome(self, name, said):
        if not self.signer.held():
            say("pods_unfiled", run=json.dumps(f"{name}:{said['run']}"), reason="no_key")
            return f"{SHOTS_UNWRITTEN}: no_key"
        items = shot_files(self.fetched(name, said["tar"]), said["to"])
        pause = self.retry_pause
        attempts = 0
        while True:
            try:
                return self.file_once(name, said, items)
            except Unanswered as why:
                attempts += 1
                if attempts & (attempts - 1) == 0:
                    say("pods_retried", attempts=attempts, reason="node_unanswered", run=json.dumps(f"{name}:{said['run']}"), detail=json.dumps(str(why)))
                self.pause(pause)
                pause *= 2

    def file_once(self, name, said, items):
        pieces = [(path, data, [data[start : start + CHUNK_SIZE] for start in range(0, len(data), CHUNK_SIZE)]) for path, data in items]
        ids = []
        with tempfile.TemporaryDirectory() as temp:
            paths = []
            for piece in (part for _, _, parts in pieces for part in parts):
                paths.append(Path(temp) / f"chunk-{len(paths)}")
                paths[-1].write_bytes(piece)
            if paths:
                put = self.signer.put(paths, CHUNK)
                why = failed(put)
                if why is not None and unanswered(why):
                    raise Unanswered(why)
                if why is not None:
                    raise Refused(502, f"the shots' chunks were not put: {why}")
                ids = put_answer(put.out.decode(), len(paths))["blobs"]
        listed, at = [], 0
        for path, data, parts in pieces:
            listed.append((path, len(data), ids[at : at + len(parts)]))
            at += len(parts)
        message = f"pods: {name} run {said['run']} wrote {said['to']}"
        while True:
            head = self.signer.query(FILES_PROGRAM, files_head())
            why = failed(head)
            if why is not None and unanswered(why):
                raise Unanswered(why)
            if why is not None:
                raise Refused(502, f"files' head was not read: {why}")
            outcome, reason, sentence, height = receipt(self.signer.submit(FILES_PROGRAM, files_commit_many(head_of(head.out), message, listed)))
            if outcome == "applied":
                say("pods_filed", run=json.dumps(f"{name}:{said['run']}"), files=len(listed), height=height)
                return f"shots written: {len(listed)} files under {said['to']} at block {height}"
            if moved_under(outcome, reason, sentence):
                continue
            if outcome in ("unknown", "unlanded") or (outcome == "failed" and unanswered(sentence)):
                raise Unanswered(sentence)
            say("pods_unfiled", run=json.dumps(f"{name}:{said['run']}"), reason=reason or outcome, detail=json.dumps(sentence))
            return f"{SHOTS_UNWRITTEN}: {reason or outcome}: {sentence}"

    def filed_before(self, name, logged, passed):
        notified = 0
        for raw in logged:
            if notified >= passed:
                return
            notified += len(list(notify_lines(name, [raw])))
            with self.lock:
                self.filed.setdefault(name, set()).update((said["run"], said["tar"]) for said in files_lines([raw]))

    def recorded(self, said):
        after = None
        while True:
            ran = self.signer.query(FORGE_REACH, reach_reports(said["repo"], said["commit"], after))
            if ran.code != 0:
                raise Refused(502, ran.err or ran.out.decode(errors="replace") or f"ducktape exited {ran.code}")
            found, after = reports_of(ran.out)
            if (said["context"], said["url"]) in found:
                return True
            if after is None:
                return False

    def reports_passed(self, name, said):
        for newest in range(len(said) - 1, -1, -1):
            try:
                held = self.recorded(said[newest])
            except Refused as refused:
                return 0, refused.sentence
            if held:
                with self.lock:
                    self.reported.setdefault(name, set()).update(each["url"] for each in said[: newest + 1])
                return newest + 1, None
        return 0, None

    def caught_up(self, name, logged):
        reports = list(report_lines(name, logged))
        if reports:
            reported, why = self.reports_passed(name, reports)
            if why is None:
                say("pods_reports_caught_up", pod=name, already_reported=reported)
            else:
                say("pods_uncaught", pod=name, reports=len(reports), detail=json.dumps(why))
        passed, asked, why = self.passed(name, list(notify_lines(name, logged)))
        if why is not None:
            say("pods_uncaught", pod=name, asked=asked, detail=json.dumps(why))
            return
        self.filed_before(name, logged, passed)
        say("pods_caught_up", pod=name, already_posted=passed, asked=asked)

    def lines(self, name, requester, stream, unvouched=False, concerned=None):
        return self.each(name, requester, notify_lines(name, self.filing(name, self.reporting(name, stream))), unvouched, concerned)

    def each(self, name, requester, said, unvouched=False, concerned=None):
        concerned = requester if concerned is None else concerned
        held = 0
        for channel, text, caller in said:
            message_id = line_id(name, channel, text)
            with self.lock:
                if message_id in self.posted.get(name, ()):
                    continue
            posted = self.post({NOTIFY_CHANNEL: channel}, requester, message_id, line_said(name, self.told(name, text), concerned if caller is None else caller), unvouched)
            if posted:
                held += posted is HELD
                with self.lock:
                    self.posted.setdefault(name, set()).add(message_id)
        return held

    def follow(self, name, requester):
        try:
            ran = self.kube.logs(name, ["--container", RUN])
            if ran.code == 0:
                logged = ran.out.splitlines(keepends=True)
                self.caught_up(name, logged)
                self.track(name, requester, logged)
            else:
                say("pods_uncaught", pod=name, detail=json.dumps(ran.err or f"kubectl exited {ran.code}"))
            process = self.kube.follow(name)
            self.lines(name, requester, self.tracking(name, requester, process.stdout))
            process.wait()
        finally:
            with self.lock:
                self.following.discard(name)

    def read(self, name, requester):
        ran = self.kube.logs(name, ["--container", RUN])
        if ran.code == 0:
            logged = ran.out.splitlines(keepends=True)
            self.lines(name, requester, logged)
            self.track(name, requester, logged)

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

    def track(self, name, requester, logged):
        with self.keeping:
            changes = {}
            with self.lock:
                runs = self.open.setdefault(name, {})
                done = self.done.setdefault(name, set())
                for step, run, channel, call in run_lines(logged):
                    match step:
                        case "armed":
                            known = run in done or run in runs
                            if known:
                                continue
                            runs[run] = {"pod": name, "run": run, "channel": channel, "call": call, "requester": requester}
                            changes[kept_key(name, run)] = json.dumps(runs[run])
                        case "done":
                            done.add(run)
                            if runs.pop(run, None) is not None:
                                changes[kept_key(name, run)] = None
                        case "argv":
                            if run in runs and not runs[run].get("argv"):
                                runs[run]["argv"] = call
                                changes[kept_key(name, run)] = json.dumps(runs[run])
            self.keep(changes)

    def tracking(self, name, requester, stream):
        for raw in stream:
            self.track(name, requester, [raw])
            yield raw

    def keep(self, changes):
        if not changes:
            return
        ran = self.kube.kept_change(changes)
        if ran.code != 0:
            say("pods_unkept", reason="kubectl_failed", keys=len(changes), detail=json.dumps(ran.err or f"kubectl exited {ran.code}"))

    def lose(self, name, why):
        with self.lock:
            runs = self.open.pop(name, {})
            self.done.pop(name, None)
        for run, kept in runs.items():
            if self.called_again(name, run, kept, why):
                continue
            again = f"; call it again: {kept['call']}" if kept["call"] else "; call it again"
            text = f"run {run}{RUN_LOST}{name}: {why}, so nothing of it runs now{again}"
            said = line_said(name, text, kept["requester"])
            self.post({NOTIFY_CHANNEL: kept["channel"]}, kept["requester"], lost_id(name, kept["channel"], run), said)
        with self.keeping:
            self.keep({kept_key(name, run): None for run in runs})

    def called_again(self, name, run, kept, why):
        words = unpacked(kept.get("argv") or "")
        if self.recall is None or not words or "AGAIN=1" in itertools.takewhile(SETTING.fullmatch, words):
            return False
        if kept.get("recalling"):
            text = f"run {run} may have been called again by a pods service that stopped before it said so (pod {name} went before it ended: {why}): read build.sh busy on each build machine before calling it again"
            self.post({NOTIFY_CHANNEL: kept["channel"]}, kept["requester"], lost_id(name, kept["channel"], run), line_said(name, text, kept["requester"]))
            return True
        with self.keeping:
            self.keep({kept_key(name, run): json.dumps({**kept, "recalling": True})})
        try:
            new, reason = self.recall(words), None
        except Refused as refused:
            new, reason = None, refused.sentence
        if new is None:
            with self.lock:
                self.pending[pending_key(name, run)] = {**kept, "why": why}
            with self.keeping:
                self.keep({pending_key(name, run): json.dumps({**kept, "why": why})})
            text = f"run {run} waits to be called again: pod {name} went before it ended ({why}), and no build machine took it ({reason or 'none takes it'}); the next builds call calls it again"
        else:
            text = f"run {run} is now run {new}: pod {name} went before it ended ({why}), so the pods service called it again once: wait on that one"
        self.post({NOTIFY_CHANNEL: kept["channel"]}, kept["requester"], lost_id(name, kept["channel"], run), line_said(name, text, kept["requester"]))
        return True

    def call_pending(self, recall):
        with self.lock:
            keys = list(self.pending)
        for key in keys:
            with self.lock:
                kept = self.pending.pop(key, None)
            if kept is None:
                continue
            try:
                new = recall(unpacked(kept["argv"]))
            except Refused as refused:
                with self.lock:
                    self.pending[key] = kept
                say("pods_uncalled", run=kept["run"], detail=json.dumps(refused.sentence))
                continue
            with self.keeping:
                self.keep({key: None})
            text = f"run {kept['run']} is now run {new}: pod {kept['pod']} went before it ended ({kept['why']}), so the pods service called it again once: wait on that one"
            self.post({NOTIFY_CHANNEL: kept["channel"]}, kept["requester"], recalled_id(kept["pod"], kept["channel"], kept["run"]), line_said(kept["pod"], text, kept["requester"]))

    def resume(self):
        if not self.resumed:
            try:
                kept = self.kube.kept_load()
            except Refused as refused:
                say("pods_unkept", reason="unread", detail=json.dumps(refused.sentence))
                return
            with self.lock:
                for key, value in kept.items():
                    if key.startswith(RERUN):
                        continue
                    record = json.loads(value)
                    if "why" in record:
                        self.pending[key] = record
                        continue
                    self.open.setdefault(record["pod"], {}).setdefault(record["run"], record)
            self.resumed = True
        self.sweep()

    def sweep(self):
        ran = self.kube.list()
        if ran.code != 0:
            say("pods_unswept", reason="kubectl_failed", detail=json.dumps(ran.err or f"kubectl exited {ran.code}"))
            return
        live = {pod["metadata"]["name"] for pod in json.loads(ran.out).get("items", [])}
        with self.lock:
            gone = [name for name in self.open if name not in live]
        for name in gone:
            self.lose(name, "it was gone when the pods service looked")

    def last_read(self, pod):
        meta = pod["metadata"]
        deleting = "deletionTimestamp" in meta and NOTIFY_CHANNEL in (meta.get("annotations") or {})
        if not deleting:
            return
        name = meta["name"]
        with self.lock:
            if name in self.deleting:
                return
            self.deleting.add(name)
        ran = self.kube.logs(name, ["--container", RUN])
        if ran.code == 0:
            self.track(name, requester_of(meta.get("labels") or {}), ran.out.splitlines(keepends=True))

    def seen(self, pod):
        self.followed(pod)
        self.last_read(pod)
        meta = pod["metadata"]
        notes = meta.get("annotations") or {}
        pending = pending_said(pod)
        unsaid = pending is not None and NOTIFY_CHANNEL in notes and NOTIFIED_PENDING not in notes
        if unsaid:
            self.said_pending(pod, pending)
        ended = ended_said(pod)
        watched = ended is not None and NOTIFY_CHANNEL in notes
        if not watched:
            return
        name = meta["name"]
        why = f"it ended before the run did, {ended_line(pod).removeprefix(f'pod {name} ended: ')}"
        if NOTIFIED in notes:
            self.lose(name, why)
            return
        requester = requester_of(meta.get("labels") or {})
        self.read(name, requester)
        self.lose(name, why)
        if self.post(notes, requester, f"pods-{name}-ended", ended):
            self.kube.annotate(name, NOTIFIED, "ended")
            with self.lock:
                self.posted.pop(name, None)

    def said_pending(self, pod, said):
        meta = pod["metadata"]
        name = meta["name"]
        if self.post(meta.get("annotations") or {}, requester_of(meta.get("labels") or {}), f"pods-{name}-pending", said):
            self.kube.annotate(name, NOTIFIED_PENDING, "posted")

    def event(self, event):
        kind = event.get("type")
        if kind in ("ADDED", "MODIFIED"):
            self.seen(event["object"])
        elif kind == "DELETED":
            name = event["object"]["metadata"]["name"]
            with self.lock:
                self.posted.pop(name, None)
                self.reported.pop(name, None)
                self.filed.pop(name, None)
                self.shots.pop(name, None)
                self.outcomes.pop(name, None)
                self.deleting.discard(name)
            self.lose(name, "it was deleted before the run ended")

    def watch(self):
        failures = 0
        while True:
            self.resume()
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


class Rerunner:
    def __init__(self, signer, kube, launch, retry_pause, post):
        self.signer = signer
        self.kube = kube
        self.launch = launch
        self.called = launch
        self.retry_pause = retry_pause
        self.post = post
        self.reporter = None
        self.lock = threading.RLock()
        self.retrying = set()
        self.spawn = lambda target, *args: threading.Thread(target=target, args=args, daemon=True).start()
        self.pause = time.sleep

    def asked(self, program, request):
        ran = self.signer.query(program, request)
        why = failed(ran)
        if why is None:
            return ran.out
        if unanswered(why):
            raise Unanswered(why)
        raise Refused(502, f"{program} was not asked: {why}")

    def identify(self):
        key = self.signer.key()
        pause = self.retry_pause
        attempts = 0
        while True:
            try:
                return account_of(self.asked(IDENTITY, identity_of_key(key)))
            except Unanswered as why:
                attempts += 1
                if attempts & (attempts - 1) == 0:
                    say("pods_retried", attempts=attempts, reason="node_unanswered", detail=json.dumps(str(why)))
                self.pause(pause)
                pause *= 2

    def pages(self, ask, read):
        rows, after = [], None
        while True:
            found, after = read(self.asked(FORGE_PROGRAM, ask(after)))
            rows += found
            if after is None:
                return rows

    def keep(self, changes):
        ran = self.kube.kept_change(changes)
        if ran.code != 0:
            say("pods_unkept", reason="kubectl_failed", keys=len(changes), detail=json.dumps(ran.err or f"kubectl exited {ran.code}"))
        return ran.code == 0

    def front(self, repo, required, lane):
        n = queue_front_of(self.asked(FORGE_PROGRAM, forge_queue_front(repo, required["branch"])))
        if n is None:
            return None
        merge = preview_of(self.asked(FORGE_PROGRAM, forge_preview(repo, n)))
        if merge is None or not FULL_COMMIT.fullmatch(merge):
            return None
        statuses = self.pages(lambda after: forge_statuses(repo, merge, after), statuses_of)
        states = {found["context"]: found["state"] for found in statuses if found["reporter"] == required["reporter"]}
        ready = all(states.get(context) == "passed" for context in required["contexts"] if context != lane)
        waiting = states.get(lane, "pending") == "pending"
        return {"repo": repo, "n": n, "commit": merge, "wanted": ready and waiting}

    def fronts(self, repo):
        lane = f"{self.reporter}/{LANE_CONTEXT}"
        asking = [required for required in self.pages(lambda after: forge_required(repo, after), required_of) if lane in required["contexts"]]
        fronts = {}
        for required in asking:
            front = self.front(repo, required, lane)
            if front is not None:
                fronts[rerun_key(repo, front["commit"])] = front
        return bool(asking), fronts

    def settle(self, repo):
        with self.lock:
            asking, fronts = self.fronts(repo)
            if not asking:
                return
            kept = self.kube.kept_load()
            gone = {key: None for key, value in kept.items() if key.startswith(RERUN) and key not in fronts and json.loads(value).get("repo") == repo}
            for key in gone:
                self.build("stop", json.loads(kept[key]).get("run"))
            if gone:
                self.keep(gone)
            for key, front in fronts.items():
                if not front["wanted"]:
                    continue
                if key not in kept:
                    self.rerun(key, front)
                elif "unstarted" in json.loads(kept[key]):
                    self.retry_later(repo, key)

    def rerun(self, key, row):
        record = {"repo": row["repo"], "n": row["n"], "commit": row["commit"]}
        if not self.keep({key: json.dumps(record)}):
            return False
        try:
            ran = self.launch(rerun_command(row))
        except Refused as refused:
            return self.unstarted(key, row, record, refused.sentence)
        started = STARTED.search(ran.out)
        if started is None:
            return self.unstarted(key, row, record, f"build.sh answered exit {ran.code} with no run")
        run = started.group(1).decode(errors="replace")
        say("pods_rerun", repo=row["repo"], n=row["n"], commit=row["commit"], run=json.dumps(run))
        self.keep({key: json.dumps({**record, "run": run})})
        self.build("bump", run)
        return True

    def build(self, verb, run):
        seen = set()
        while run is not None and run not in seen:
            seen.add(run)
            try:
                ran = self.called(["sh", BUILD_SH, verb, run])
            except Refused as refused:
                say("pods_rerun_unbuilt", verb=verb, run=json.dumps(run), detail=json.dumps(refused.sentence))
                return
            answered = ran.err + "\n" + ran.out.decode(errors="replace")
            if ran.code == 0 or RUN_HAS_ENDED.search(answered):
                return
            moved = MOVED_ELSEWHERE.search(answered)
            if moved is None:
                say("pods_rerun_unbuilt", verb=verb, run=json.dumps(run), detail=json.dumps(f"build.sh {verb} answered exit {ran.code}"))
                return
            run = f"{moved.group(1)}:{moved.group(2)}"

    def unstarted(self, key, row, record, why):
        say("pods_rerun_unstarted", repo=row["repo"], n=row["n"], commit=row["commit"], detail=json.dumps(why))
        self.keep({key: json.dumps({**record, "unstarted": why})})
        told = hashlib.sha256(f"{row['repo']}\n{row['commit']}\n{why}".encode()).hexdigest()
        detail = f"the lane on Preview {row['commit']} did not start: {why}. The pods service asks again from one block, doubling, while that Preview is its queue's front."
        said = {"group": f"lane {row['commit']}", "subject": f"the lane of {row['repo']} #{row['n']}", "status": "failed", "detail": detail, "concerns": []}
        self.post(f"forge:{row['repo']}:{row['n']}", f"pods-rerun-{told}", said)
        self.retry_later(row["repo"], key)
        return False

    def retry_later(self, repo, key):
        if key not in self.retrying:
            self.retrying.add(key)
            self.spawn(self.retry, repo, key)

    def retry(self, repo, key):
        pause = self.retry_pause
        try:
            while True:
                self.pause(pause)
                pause *= 2
                with self.lock:
                    _, fronts = self.fronts(repo)
                    front = fronts.get(key)
                    record = self.kube.kept_load().get(key)
                    if front is None or not front["wanted"] or record is None or "unstarted" not in json.loads(record):
                        return
                    if self.rerun(key, front):
                        return
        except Exception as error:
            say("pods_rerun_unsettled", repo=repo, detail=json.dumps(error.sentence if isinstance(error, Refused) else repr(error)))
        finally:
            with self.lock:
                self.retrying.discard(key)

    def settled(self, repo):
        try:
            self.settle(repo)
        except Exception as error:
            say("pods_rerun_unsettled", repo=repo, detail=json.dumps(error.sentence if isinstance(error, Refused) else repr(error)))

    def reopened(self):
        try:
            repos = self.pages(forge_repos, repos_of)
        except Exception as error:
            say("pods_rerun_unsettled", detail=json.dumps(f"forge's repositories were not listed: {error.sentence if isinstance(error, Refused) else repr(error)}"))
            return
        for repo in repos:
            self.settled(repo)

    def watch(self):
        try:
            self.reporter = self.identify()
        except (OSError, Refused) as error:
            say("pods_rerun_unkeyed", detail=json.dumps(error.sentence if isinstance(error, Refused) else repr(error)))
            return
        if self.reporter is None:
            say("pods_rerun_unkeyed", detail=json.dumps("identity names no account for the service's key"))
            return
        say("pods_rerunning", reporter=self.reporter)
        pause = self.retry_pause
        while True:
            process = self.signer.follow(FORGE_PROGRAM)
            self.reopened()
            heard, at, settled = False, None, set()
            with process.stdout:
                for line in process.stdout:
                    heard = True
                    touched = front_touched(line)
                    if touched is None:
                        continue
                    height, repo = touched
                    if height != at:
                        at, settled = height, set()
                    if repo not in settled:
                        settled.add(repo)
                        self.settled(repo)
            say("pods_rerun_unfollowed", code=process.wait(), heard=heard)
            if heard:
                pause = self.retry_pause
            self.pause(pause)
            if not heard:
                pause *= 2


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
    def __init__(self, kube, signer, fetching, stats, api, runners=None, warm_pod=None):
        self.kube = kube
        self.warm_pod = warm_pod
        self.uncalled = 0
        self.runners = {runner.name: runner for runner in (runners if runners is not None else [Runner(DESKTOP, Path(DESKTOP_KEYS), signer=signer)])}
        self.wanted = {name: threading.Event() for name in self.runners}
        self.followers = {}
        self.rest = lambda name, pause: self.wanted[name].wait(pause)
        self.retry_pause = fetching.block_ms / 1000
        self.clock = time.monotonic
        self.wall = time.time
        self.signer = signer
        self.fetching = fetching
        self.stats = stats
        self.api = api
        self.schemas = Schemas(api)
        self.notifier = Notifier(kube, signer, self.retry_pause)
        self.notifier.records = self.lane_record
        self.notifier.fetched = self.machine_file
        self.notifier.recall = lambda words: self.recalled(words, None)
        self.pause = time.sleep
        self.execs = {}
        self.lock = threading.Lock()
        self.swept = threading.Event()
        self.rerouting = threading.Lock()
        self.launched = {}
        self.rerunner = Rerunner(signer, kube, lambda command: self.routed(None, command, None), self.retry_pause, self.tell_room)

    def machines(self):
        return ", ".join(f"pod {name}" for name in self.runners)

    def tell_room(self, channel, message_id, said):
        return self.notifier.post({NOTIFY_CHANNEL: channel}, None, message_id, said, unvouched=True)

    def pod(self, name):
        ran = self.kube.get(name)
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name} was not read")
        return json.loads(ran.out)

    def keyed(self, doing):
        if not self.signer.held():
            raise Refused(503, f"this service holds no key to {doing} with")

    def waiting(self, asked):
        if asked is None:
            return True
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
            running.end(outcome(lambda: self.executed(asked, name, container, command, alive=alive, note=running.note)))
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
                ran = self.runners[machine].ran(command, None) if machine in self.runners else self.kube.exec(machine, None, command)
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

    def executed(self, asked, name, container, command, again=False, alive=None, note=None):
        if name in self.runners:
            try:
                return self.runners[name].ran(command, alive)
            except Cut as cut:
                return self.read_again(asked, name, cut, "WAIT=1" in (build_settings(command, True) or []), alive)
        if name == BUILDS:
            return self.routed(asked, command, alive)

        def attempt():
            return self.kube.exec(name, container, command)

        while True:
            ran = self.through_apiserver(asked, name, lambda: self.retried(asked, attempt, failed) if again else attempt())
            waited = unstarted(ran, container)
            if waited is None or not self.started(asked, name, waited.container, note):
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

    def started(self, asked, name, container, note=None):
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
                line = pending_line(name, item.get("object") or {})
                if line is not None and note is not None:
                    note(line)
                if not self.waiting(asked):
                    raise Refused(502, f"container {watched} of pod {name} had not started when request {asked['request']}, which asked for this, was gone, so nothing ran")
                if start == "running":
                    return True
        finally:
            if process is not None:
                process.kill()

    def output(self, name, asked_by, asked):
        container = asked.get("container")
        if name not in self.runners:
            pod = self.pod(name)
            names = [held["name"] for held in pod["spec"]["containers"]]
            container = container or (OUT if OUT in names else None)
        command = ["cat", "--", absolute(asked["path"], "path")]
        ran = self.executed(asked_by, name, container, command, again=True)
        if ran.code != 0:
            raise kube_refusal(ran, f"{asked['path']} was not read from pod {name}")
        return ran.out

    def create(self, asked, body):
        spec = checked(unrequested(body, "a pod"), SPEC, "a pod")
        pod = pod_spec(asked, self.requester(asked), spec, self.fetching, self.kube.namespace)
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

    def lane_record(self, machine, run):
        command = ["sh", BUILD_SH, "status", run]
        if machine in self.runners:
            return self.runners[machine].ran(command).out.decode(errors="replace")
        if not self.warm_pod or machine != self.warm():
            return None
        return self.kube.exec(machine, None, command).out.decode(errors="replace")

    def machine_file(self, machine, path):
        command = ["cat", "--", path]
        ran = self.runners[machine].ran(command) if machine in self.runners else self.kube.exec(machine, None, command)
        if ran.code != 0:
            raise kube_refusal(ran, f"{path} was not read from pod {machine}")
        return ran.out

    def warm(self):
        if not self.warm_pod:
            raise Refused(503, f"no build runner is free and this service names no warm pod (PODS_WARM_POD), so pod {BUILDS} has nowhere to run it")
        reply = self.signer.query(FILES_PROGRAM, files_read(self.warm_pod, None, 0, CHUNK_SIZE))
        if reply.code != 0:
            raise Refused(502, f"the warm pod's name was not read from {self.warm_pod}: {failed(reply)}")
        name = contents_of(reply.out)[3].decode(errors="replace").strip()
        if not name:
            raise Refused(502, f"{self.warm_pod} names no warm pod")
        return name

    def pick(self, asked, words, alive):
        passed, readable, kept = [], [], []
        for name, runner in self.runners.items():
            if not runner.takes(words):
                continue
            try:
                said = runner.state()
            except Refused as refused:
                passed.append(("runner_unready", name, refused.sentence))
                continue
            free, why = runner_free(name, said)
            here = None
            if free:
                here, why = self.ahead(asked, name, words, alive)
            if here is None:
                passed.append(("runner_unasked" if free else "runner_busy", name, why))
                kept.append(name)
                continue
            if here[1] == 0:
                return self.went(name, name, passed), True
            readable.append((name, here))
        if not readable and not kept:
            return self.went(self.warm(), "warm", passed), False
        stay = soonest(readable)[0] if readable else kept[0]
        try:
            warm = self.warm()
        except Refused as refused:
            return self.went(stay, stay, [*passed, ("warm_unasked", None, refused.sentence)]), False
        busy = any(reason == "runner_busy" for reason, _, _ in passed)
        if not readable and not busy:
            return self.went(warm, "warm", passed), False
        there, why = self.ahead(asked, warm, words, alive)
        if there is None:
            return self.went(stay, stay, [*passed, ("warm_unasked", None, why)]), False
        if not readable:
            return self.went(warm, "warm", passed), False
        machines = [*readable, (warm, there)]
        machine, timed = soonest(machines)
        if machine != machines[0][0]:
            lane = machines[0][1][0]
            if timed:
                said = f"{lane}: ends in " + ", ".join(f"{here[2]}s on pod {name}" for name, here in machines)
            else:
                said = f"{lane}: " + ", ".join(f"{here[1]}{' ahead' if index == 0 else ''} on pod {name}" for index, (name, here) in enumerate(machines))
            passed.append(("sooner_end" if timed else "shorter_wait", None, said))
        return self.went(machine, "warm" if machine == warm else machine, passed), False

    def went(self, machine, to, passed):
        for reason, runner, sentence in passed:
            say("pods_routed", to=to, reason=reason, **({"runner": runner} if runner else {}), detail=json.dumps(sentence))
        return machine

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
        waits, unread = {}, []
        for name, runner in self.runners.items():
            try:
                lanes = runner.ran(["sh", BUILD_SH, "lanes"], alive)
            except Refused as refused:
                unread.append(refused.sentence)
                continue
            if lanes.code != 0:
                unread.append(f"build.sh lanes on pod {name} answered exit {lanes.code}")
                continue
            waits[name] = {}
            for line in lanes.out.decode(errors="replace").splitlines():
                found, paused = LANE_WAIT.fullmatch(line), LANE_PAUSED.fullmatch(line)
                if found:
                    waits[name][found.group(1)] = int(found.group(2))
                elif paused:
                    waits[name][paused.group(1)] = None
        if not waits:
            raise Refused(502, "; ".join(unread) or "this service names no build runner")
        said, chosen, moved_ahead = [], {}, {}
        candidates = []
        for line in listed.out.decode(errors="replace").splitlines():
            found = MOVABLE_RUN.fullmatch(line)
            if found:
                candidates.append((found.group(2), int(found.group(3)), found.group(1), found.group(4)))
            elif line.startswith("keep "):
                said.append(line)
        for lane, ahead, run, words in sorted(candidates):
            there = ahead - moved_ahead.get(lane, 0)
            taking = [(waits[name].get(lane, 0), name) for name in waits if waits[name].get(lane, 0) is not None and self.runners[name].takes(unpacked(words))]
            if not taking:
                said.append(f"keep {run} no build runner that takes it says its {lane} lane's wait")
                continue
            here, name = min(taking, key=lambda pair: pair[0])
            if here < there:
                chosen.setdefault(name, []).append((run, words))
                waits[name][lane] = here + 1
                moved_ahead[lane] = moved_ahead.get(lane, 0) + 1
            else:
                said.append(f"keep {run} the {lane} lane waits for {here} runs on pod {name} and {there} here")
        for name, runs in chosen.items():
            said += self.carried(asked, warm, name, runs, alive)
        moved = sum(1 for line in said if ": moved to run " in line)
        say("pods_rerouted", moved=moved, kept=len(said) - moved)
        return "\n".join(said) + "\n" if said else f"nothing queued on pod {warm} to move\n"

    def carried(self, asked, warm, name, chosen, alive):
        if any(notifies(words) for _, words in chosen):
            self.follow(name)
        try:
            took = self.runners[name].ran(["sh", BUILD_SH, "take", *[words for _, words in chosen]], alive)
        except Refused as refused:
            raise Refused(502, f"{refused.sentence}; build.sh take on pod {name} may have started some of these runs, each still queued on pod {warm} ({', '.join(run for run, _ in chosen)}): read build.sh busy on pod {name} before calling reroute again") from refused
        lines = took.out.decode(errors="replace").splitlines()
        said, pairs = [], {}
        for index, (run, _) in enumerate(chosen):
            line = lines[index] if index < len(lines) else f"failed: build.sh take answered exit {took.code} and said nothing of it"
            if line.startswith("run "):
                pairs[run] = f"{name}:{line[4:]}"
            else:
                said.append(f"keep {run} pod {name} {line}")
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
            said += self.yielded(warm, name, stays, alive)
        return said

    def warm_moved(self, asked, warm, pairs, alive):
        try:
            ran = self.executed(asked, warm, None, ["sh", BUILD_SH, "moved", *itertools.chain.from_iterable(pairs.items())], alive=alive)
        except Refused as refused:
            return {}, refused.sentence
        lines = (line.split(": ", 1) for line in ran.out.decode(errors="replace").splitlines() if ": " in line)
        return {run: f"{run}: {rest}" for run, rest in lines if run in pairs}, f"build.sh moved on pod {warm} answered exit {ran.code} without a line for it"

    def yielded(self, warm, name, stays, alive):
        words = [word for run, copy in stays.items() for word in (copy.partition(":")[2], f"{warm}:{run}")]
        try:
            ran = self.runners[name].ran(["sh", BUILD_SH, "yield", *words], alive)
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
        return self.runners[machine].ran(command, alive) if machine in self.runners else self.executed(asked, machine, None, command, alive=alive)

    def ahead(self, asked, machine, words, alive):
        try:
            ran = self.asked_on(asked, machine, ["ahead", *words], alive)
        except Refused as refused:
            return None, refused.sentence
        paused = AHEAD_PAUSED.fullmatch(ran.out.strip()) if ran.code == 0 else None
        if paused is not None:
            return None, f"build.sh ahead on pod {machine} says its {paused.group(1).decode()} runs wait on its pause"
        found = AHEAD.fullmatch(ran.out.strip()) if ran.code == 0 else None
        if found is None:
            return None, f"build.sh ahead on pod {machine} answered exit {ran.code} with no ahead line"
        ends = found.group(3)
        return (found.group(1).decode(), int(found.group(2)), int(ends) if ends and ends != b"-" else None), None

    def on(self, asked, machine, words, alive):
        if machine in self.runners and any(word.startswith("NOTIFY=") and word != "NOTIFY=" for word in words):
            self.follow(machine)
        command = ["sh", BUILD_SH, *words]
        if machine in self.runners:
            return self.runners[machine].ran(command, alive)
        return self.executed(asked, machine, None, command, alive=alive)

    def recalled(self, words, asked):
        settings = list(itertools.takewhile(SETTING.fullmatch, words))
        again = [*settings, "AGAIN=1", *words[len(settings) :]]
        reasons = []
        for name, runner in self.runners.items():
            if not runner.takes(again):
                reasons.append(f"pod {name} takes no build.sh {run_kind(again)}")
                continue
            try:
                ran = self.on(asked, name, again, None)
            except Refused as refused:
                reasons.append(refused.sentence)
                continue
            started = STARTED.search(ran.out)
            if started is not None:
                return f"{name}:{started.group(1).decode(errors='replace')}"
            reasons.append(f"build.sh on pod {name} answered exit {ran.code} with no run")
        if asked is not None:
            warm = self.warm()
            ran = self.on(asked, warm, again, None)
            started = STARTED.search(ran.out)
            if started is not None:
                return f"{warm}:{started.group(1).decode(errors='replace')}"
            reasons.append(f"build.sh on pod {warm} answered exit {ran.code} with no run")
        raise Refused(503, "; ".join(reasons) or "this service names no build runner")

    def routed(self, asked, command, alive):
        if self.notifier.pending:
            self.notifier.call_pending(lambda words: self.recalled(words, asked))
        words = runner_words(command, BUILDS)
        settings = list(itertools.takewhile(SETTING.fullmatch, words))
        called = words[len(settings) :]
        first = called[0] if called else ""
        if first == REROUTE and len(called) == 1:
            return Ran(0, self.reroute(asked, alive).encode(), f"{BUILDS}: reroute")
        if first == "bump" and len(called) > 1:
            return self.bumped(asked, settings, called[1:], alive)
        if first in MACHINE_WIDE:
            raise Refused(400, f"build.sh {first} acts on one machine, so name it ({self.machines()}, or the warm pod) rather than pod {BUILDS}")
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
            rest = []
            for word in called[2:]:
                on, colon, its = word.partition(":")
                if colon and on != machine:
                    raise Refused(400, f"build.sh {first} on pod {BUILDS} acts on one machine per call, and {word} names pod {on} while {named} names pod {machine}: call it once for each")
                rest.append(its if colon else word)
            return on_machine(machine, self.on(asked, machine, [*settings, first, run, *rest], alive), started=first == "requeue")
        verb = first if first == "apply" else (called[2] if len(called) > 2 else first)
        claimed = self.claim(asked, settings, words, alive)
        if claimed is not None:
            return claimed
        machine, idle = self.pick(asked, words, alive)
        named = named_commit(called)
        if named is not None:
            held = self.holding(asked, machine, *named, words, alive)
            idle = idle and held == machine
            machine = held
        if not self.still_open(asked):
            raise Refused(502, f"request {asked['request']}, which asked for this, is gone, so build.sh was not called on pod {machine}: nothing ran")
        try:
            ran = self.launched_on(asked, machine, settings, words, alive)
            if idle:
                self.reroute_after(asked)
        except Refused as refused:
            if machine not in self.runners:
                raise
            lost = machine
            started = STARTED.search(refused.out) if isinstance(refused, Cut) else None
            run = f"{lost}:{started.group(1).decode(errors='replace')}" if started else None
            if isinstance(refused, Cut) and verb not in READ_ONLY:
                say("pods_parked", verb=json.dumps(verb), run=run or "unstarted", detail=json.dumps(refused.sentence))
                where = f"build.sh status {run}" if run else f"build.sh busy on pod {lost}"
                raise Refused(502, f"{refused.sentence}; build.sh {verb} changes things, so it is parked rather than run on another machine{f' as run {run}' if run else ''}: once pod {lost} answers, read {where} before calling it again") from refused
            if not self.still_open(asked):
                raise Refused(502, f"{refused.sentence}; request {asked['request']}, which asked for this, is gone, so it was not run on another machine{f', and run {run} may still run there' if run else ''}") from refused
            try:
                machine = self.warm()
            except Refused as unnamed:
                say("pods_unrouted", reason="runner_lost", runner=lost, detail=json.dumps(f"{refused.sentence}; {unnamed.sentence}"))
                still = f" as run {run}, which may still run there" if run else ""
                where = f"status {run}" if run else f"busy on pod {lost}"
                raise Refused(502, f"{refused.sentence}{still}; it was not run elsewhere, since {unnamed.sentence}: once pod {lost} answers, read build.sh {where} before calling it again") from refused
            say("pods_routed", to="warm", reason="runner_lost", runner=lost, detail=json.dumps(refused.sentence))
            ran = self.launched_on(asked, machine, settings, words, alive)
        return on_machine(machine, ran)

    def bumped(self, asked, settings, named, alive):
        outs, errs, code = [], [], 0
        for word in reversed(named):
            machine, colon, run = word.partition(":")
            if not colon:
                run = word
                machine, _ = self.holder(asked, run, alive)
            seen = set()
            while True:
                seen.add((machine, run))
                ran = self.on(asked, machine, [*settings, "bump", run], alive)
                moved = MOVED_ELSEWHERE.search(ran.err + ran.out.decode(errors="replace"))
                if ran.code == 0 or moved is None or (moved.group(1), moved.group(2)) in seen:
                    break
                machine, run = moved.group(1), moved.group(2)
            outs.append(ran.out)
            errs.append(f"{BUILDS}: {word} bumped on {machine}" + (f"\n{ran.err}" if ran.err else ""))
            code = max(code, ran.code)
        return Ran(code, b"".join(reversed(outs)), "\n".join(reversed(errs)))

    def launched_on(self, asked, machine, settings, words, alive):
        held = "WAIT=1" in settings
        unheld = [word for word in settings if word != "WAIT=1"] + words[len(settings) :]
        try:
            ran = self.on(asked, machine, unheld if held else words, alive)
        except Cut as cut:
            self.record(words, machine, cut.out)
            return self.read_again(asked, machine, cut, held, alive)
        self.record(words, machine, ran.out)
        started = STARTED.search(ran.out)
        if not held or started is None:
            return ran
        run = started.group(1).decode(errors="replace")
        try:
            waited = self.asked_on(asked, machine, ["wait", run], alive)
        except Cut as cut:
            return self.read_again(asked, machine, Cut(cut.sentence, f"run {run}\n".encode() + cut.out), held, alive)
        return Ran(waited.code, f"run {run}\n".encode() + waited.out, waited.err)

    def read_again(self, asked, machine, cut, held, alive):
        started = STARTED.search(cut.out)
        if started is None or machine not in self.runners:
            raise cut
        run = started.group(1).decode(errors="replace")
        verb = "wait" if held else "status"
        pause = self.retry_pause
        attempts = 0
        while True:
            try:
                read = self.runners[machine].ran(["sh", BUILD_SH, verb, run], alive)
                break
            except Cut as again:
                attempts += 1
                if attempts & (attempts - 1) == 0:
                    say("pods_retried", attempts=attempts, reason="runner_cut", detail=json.dumps(again.sentence))
                if not self.still_open(asked):
                    raise cut from again
                self.pause(pause)
                pause *= 2
            except Refused as refused:
                raise cut from refused
        if not held and not read.out.startswith(b"job "):
            raise cut
        say("pods_reread", run=json.dumps(f"{machine}:{run}"), verb=verb)
        note = f"{cut.sentence}; then run {run} was read again through {machine}:{run}"
        if held:
            return Ran(read.code, f"run {run}\n".encode() + read.out, f"{note}\n{read.err}".rstrip("\n"))
        return Ran(0, cut.out, f"{note}\n{read.err}".rstrip("\n"))

    def holding(self, asked, machine, repo, commit, words, alive):
        if self.holds(asked, machine, repo, commit, alive) is not False:
            return machine
        others = [name for name, runner in self.runners.items() if name != machine and runner.takes(words)]
        if machine in self.runners:
            try:
                others.append(self.warm())
            except Refused:
                pass
        for other in others:
            if self.holds(asked, other, repo, commit, alive) is True:
                say("pods_routed", to=other if other in self.runners else "warm", reason="holds_commit", detail=json.dumps(f"{repo} {commit} is in pod {other}'s clone and not in pod {machine}'s"))
                return other
        return machine

    def holds(self, asked, machine, repo, commit, alive):
        try:
            ran = self.asked_on(asked, machine, ["holds", repo, commit], alive)
        except Refused:
            return None
        said = ran.out.decode(errors="replace").strip()
        if ran.code == 0 and said.startswith("holds "):
            return True
        if ran.code == 1 and said.startswith("lacks "):
            return False
        return None

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
        say("pods_run_claimed", run=json.dumps(f"{machine}:{run}"), **({"request": asked["request"]} if asked else {}))
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
        for machine in (*self.runners, None):
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
            raise Refused(400, f"pod {BUILDS} routes build.sh to the machine where it ends first and is no pod itself: read {self.machines()} or the warm pod")
        if name in self.runners:
            return 200, {"pod": runner_view(name, self.runners[name].state())}
        return 200, {"pod": view(self.pod(name))}

    def logs(self, name, query):
        if name in self.runners:
            return 200, self.runners[name].logs(log_options(query))
        ran = self.kube.logs(name, log_options(query))
        if ran.code != 0:
            raise kube_refusal(ran, f"pod {name}'s logs were not read")
        return 200, ran.out

    def wait(self, name, query, headers):
        every = wait_every(query, headers)
        log = wait_log(query, headers)
        if name in self.runners:
            if log is not None:
                raise Refused(400, f"{name} is a build runner, with no {OUT} container to read a log in, so its wait takes no log")
            return 200, Stream(self.runner_waited(name, self.runners[name].follow(every), every), reattach=True)
        return 200, Stream(self.waited(name, self.pod(name), every, log), reattach=True)

    def follow(self, name):
        with self.lock:
            if name not in self.followers:
                self.followers[name] = threading.Thread(target=self.followed, args=(name, self.clock(), self.wall()), daemon=True)
                self.followers[name].start()
        self.wanted[name].set()

    def followed(self, name, begun, wall):
        since = begun
        reached = False
        pause = self.retry_pause
        while True:
            asked = self.clock()
            if not reached:
                since, reached = self.runner_began(name, since, begun, wall)
                if reached:
                    self.caught_up(name, math.ceil(asked - since))
            heard = []
            try:
                process = self.runners[name].tail(math.ceil(asked - since))
            except Refused as refused:
                say("pods_unfollowed", pod=name, detail=json.dumps(refused.sentence))
            else:
                concerned = self.warm_requester(name)
                with process.stdout:
                    held = self.notifier.lines(name, None, heard_in(process.stdout, heard), unvouched=True, concerned=concerned)
                say("pods_unfollowed", pod=name, code=process.wait(), already_posted=held)
            if heard:
                since, pause = asked, self.retry_pause
            self.rest(name, pause)
            if not heard:
                pause *= 2
            self.wanted[name].clear()

    def caught_up(self, name, window):
        try:
            logged = self.runners[name].logs([f"--since={window}s"])
        except Refused as refused:
            say("pods_uncaught", pod=name, detail=json.dumps(refused.sentence))
            return
        self.notifier.caught_up(name, logged.splitlines(keepends=True))

    def called(self, asked, name, command):
        settings = build_settings(command, name in self.runners or name == BUILDS)
        if settings is None:
            return command
        if any(word.startswith(CALLER_WORD) for word in settings):
            raise Refused(400, f"pods names a build.sh call's caller itself, as the account egress says sent its request: leave {CALLER_WORD}<n> out of the call")
        a_pod_by_name = name not in self.runners and name != BUILDS
        pinned_here = a_pod_by_name and PINNED_WORD not in settings
        if pinned_here:
            command = with_setting(command, PINNED_WORD)
        try:
            caller = self.sender(asked)
        except Refused as refused:
            with self.lock:
                self.uncalled += 1
                first = self.uncalled == 1
            if first:
                say("pods_uncalled", detail=json.dumps(refused.sentence))
            return command
        with self.lock:
            missed, self.uncalled = self.uncalled, 0
        if missed:
            say("pods_called", missed=missed)
        return command if caller is None else with_caller(command, caller)

    def sender(self, asked):
        reply = self.signer.query(EGRESS, egress_sender(int(asked["request"])))
        why = failed(reply)
        if why is not None:
            raise Refused(502, f"egress was not asked who sent request {asked['request']}: {why}")
        return sender_of(reply.out)

    def requester(self, asked):
        return asked[ASKER] if ASKER in asked else self.sender(asked)

    def warm_requester(self, name):
        if not self.warm_pod:
            return None
        try:
            return requester_of(self.pod(self.warm()).get("metadata", {}).get("labels") or {})
        except Refused as refused:
            say("pods_unconcerned", pod=name, detail=json.dumps(refused.sentence))
            return None

    def runner_began(self, name, since, begun, wall):
        try:
            said = self.runners[name].state()
        except Refused as refused:
            say("pods_unresumed", pod=name, detail=json.dumps(refused.sentence))
            return since, False
        if said.get("started") is None:
            say("pods_unresumed", pod=name, detail=json.dumps(f"the {name}'s ping names no start time"))
            return since, False
        started = go_time(said["started"])
        if started is None:
            say("pods_unresumed", pod=name, detail=json.dumps(f"the {name}'s ping names its start as {said['started']!r}, not as podman's StartedAt (date, time, offset, zone)"))
            return since, False
        return min(since, begun - max(0.0, wall - started)), True

    def runner_waited(self, name, process, every):
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
        line = f"pod {name}'s log stream ended (ssh exit {code}): {why or 'it said nothing'}"
        yield sse("ended", {"line": line, "exit_code": None, "pod": None})

    def waited(self, name, pod, every, log):
        ended = ended_event(pod)
        beat = Beat(every) if log is None else Beat(every, LogReads(lambda: self.log_said(name, log)).said)
        process = None
        noted = False
        try:
            while ended is None:
                if process is None:
                    process, events = self.kube.watch_pod(name), queue.Queue()
                    threading.Thread(target=relay, args=(process, events), daemon=True).start()
                kind, item = yield from beat.heard(events)
                match kind:
                    case "event":
                        ended = watch_ended(name, item)
                        line = None if noted or ended is not None else pending_line(name, item.get("object") or {})
                        if line is not None:
                            noted = True
                            yield sse("note", {"line": line})
                    case "closed":
                        process = None
                        ended = self.reread(name, item)
        finally:
            if process is not None:
                process.kill()
        yield sse("ended", ended)

    def log_said(self, name, path):
        try:
            ran = self.kube.exec(name, OUT, ["sh", "-c", LOG_READ, "sh", path])
            stderr = reached(name, ran)
        except Refused as refused:
            return {"log": {"error": refused.sentence}}
        except Exception as error:
            return {"log": {"error": f"{path} was not read: {error!r}"}}
        size, _, line = ran.out.decode(errors="replace").partition("\n")
        if ran.code != 0 or not size.strip().isdigit():
            return {"log": {"error": stderr.strip() or f"{path} was not read: its read exited {ran.code}"}}
        return {"log": {"bytes": int(size), "line": line.removesuffix("\n")}}

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

    def delete(self, asked, name):
        if name in self.runners:
            raise Refused(403, f"pod {name} is a build runner, a machine and not a pod, so nothing deletes it")
        what = f"pod {name}"
        uid = self.whose(asked, self.pod(name), what)
        answered = self.unmade(f"/api/v1/namespaces/{self.kube.namespace}/pods/{quote(name, safe='')}", "", {}, uid, what)
        if not 200 <= answered.status < 300:
            raise api_refusal(answered, f"{what} was not deleted")
        return 200, {"deleted": name}

    def whose(self, asked, held, what):
        meta = mapping(mapping(held).get("metadata"))
        holder = account_number(str(mapping(meta.get("labels")).get(REQUESTER, "")))
        if holder is None:
            raise Refused(403, f"{what} names no requester, so no call changes or deletes it: the operator removes it with kubectl")
        caller = self.requester(asked)
        if caller != holder:
            calling = "no account" if caller is None else f"account {caller}"
            raise Refused(403, f"{what} is account {holder}'s: only the account whose request made it changes or deletes it (a call {FLEET_PROGRAM} relays: its run's asker), and this call is {calling}'s")
        uid = meta.get("uid")
        if not isinstance(uid, str):
            raise Refused(502, f"{what} was read with no uid, so the call could not be held to it")
        return uid

    def owned_at(self, asked, target, method):
        what = object_named(target)
        answered = self.api.call("GET", target.named, "", b"", None)
        if answered.status == 404 and method in CHANGES:
            made = f"a POST to /v1/k8s{target.named.rsplit('/', 1)[0]} or POST /v1/manifests"
            raise Refused(404, f"{what} is not there, and a {method} changes only an object that is, for its requester: make it with {made}, which name the caller its requester")
        if not 200 <= answered.status < 300:
            raise api_refusal(answered, f"{what} was not read, so whose it is could not be told")
        held = json.loads(answered.body)
        return held, self.whose(asked, held, what)

    def unmade(self, path, query, options, uid, what):
        body = written(preconditioned(options, uid, f"the delete options of {what}")).encode()
        return self.api.call("DELETE", path, query, body, "application/json")

    def exec(self, name, asked_by, body, headers):
        asked = checked(body, EXEC, "an exec")
        every = exec_every(asked, headers)
        command = self.called(asked_by, name, exec_command(asked))
        container = asked.get("container")
        if name in self.runners and any(word.startswith("NOTIFY=") and word != "NOTIFY=" for word in runner_words(command, name)):
            self.follow(name)
        if every is None:
            return 200, exec_answer(name, answered(self.execution(asked_by, name, container, command).get()))
        if name not in self.runners and name != BUILDS:
            self.pod(name)
        return 200, Stream(self.exec_waited(asked_by, name, container, command, every))

    def exec_waited(self, asked, name, container, command, every):
        beat, hearing = Beat(every), Hearing(self.execution(asked, name, container, command, alive=every))
        while (heard := (yield from beat.heard(hearing)))[0] == "note":
            yield sse("note", {"line": heard[1]})
        ran = heard[1]
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
        onto = pushed_onto(asked)
        have = asked.get("have", []) if onto is None else onto.have()
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
        if onto is not None:
            onto.lands(commit, parents, commits)
        packing = ["sh", "-c", PACKING, "pack", path, commit, *have]
        packed = self.executed(asked_by, name, container, packing, again=True)
        pack = git_output(name, packed, f"{commit} was not packed in {path}")
        objects = objects_of(pack)
        answer = {"commit": commit, "parents": parents, "commits": commits, "objects": objects, "size": len(pack), "kind": PACK}
        if objects == 0:
            return 200, onto_answer(onto, {**answer, "pack": None})
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / "pack"
            file.write_bytes(pack)
            put = self.retried(asked_by, lambda: self.signer.put([file], PACK), failed)
        if put.code != 0:
            raise Refused(502, f"the pack was not put: {put.err or put.out.decode(errors='replace')}")
        held = put_answer(put.out.decode(), 1)
        return 200, onto_answer(onto, {**answer, "pack": held.pop("blobs")[0], **held})

    def contents(self, name, asked_by, body):
        asked = named(checked(body, READ, "a read"), ["path"], "a read")
        path = absolute(asked["path"], "path")
        offset = number(asked.get("offset", 0), "offset")
        length = asked.get("length")
        too_short = length is not None and length < 1
        if too_short:
            raise Refused(400, "length is a number from 1")
        command = ["sh", "-c", READING, "read", path, str(offset), "" if length is None else str(length)]
        ran = self.executed(asked_by, name, asked.get("container"), command, again=True)
        return 200, read_answer(name, path, offset, ran)

    def report(self, body):
        asked = named(checked(body, REPORT, "a report"), ["report", "push"], "a report")
        return 200, report_answer(asked["report"], asked["push"])

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
        request = named(checked(unrequested(kept(body), "an apply"), APPLY, "an apply"), ["manifests"], "an apply")
        labels = labels_for(asked, self.requester(asked))
        kinds = self.cluster_kinds()
        namespace = self.kube.namespace
        confined = []
        for index, manifest in enumerate(request["manifests"]):
            made, given = confined_as_given(manifest, f"manifest {index}", kinds, namespace, labels)
            unmoved({}, given, f"manifest {index}")
            confined.append(made)
        reading = self.schemas.reading()
        objects = [reading.object(manifest, f"manifest {index}") for index, manifest in enumerate(confined)]
        self.owned(asked, objects)
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

    def owned(self, asked, objects):
        found = self.kube.held(objects)
        if found.code != 0:
            raise kube_refusal(found, "the manifests' objects were not read, so whose they are could not be told")
        answer = mapping(json.loads(found.out)) if found.out.strip() else {"kind": "List"}
        held = {object_key(item): item for item in listed_items(answer)}
        for index, manifest in enumerate(objects):
            current = held.get(object_key(manifest))
            if current is not None:
                what = f"manifest {index} ({manifest['kind']} {manifest['metadata'].get('name', '')})"
                manifest["metadata"]["uid"] = self.whose(asked, current, what)

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

    def remove(self, asked_by, body):
        asked = named(checked(body, DELETE, "a delete"), ["kind", "name"], "a delete")
        self.readable(asked["kind"], asked["name"])
        what = f"{asked['kind']} {asked['name']}"
        ran = self.kube.objects(asked["kind"], asked["name"])
        if ran.code != 0:
            raise kube_refusal(ran, f"{what} was not deleted")
        held = mapping(json.loads(ran.out))
        uid = self.whose(asked_by, held, what)
        group, _, version = str(held.get("apiVersion", "")).rpartition("/")
        kind = (group, version, str(held.get("kind", "")))
        path = self.schemas.reading().named_path(kind, asked["name"], self.kube.namespace, what)
        answered = self.unmade(path, "", {}, uid, what)
        if not 200 <= answered.status < 300:
            raise api_refusal(answered, f"{what} was not deleted")
        return 200, {"deleted": f"{kind[2].lower()}{'.' + group if group else ''}/{asked['name']}"}

    def passthrough(self, method, segments, query, asked, headers, body):
        target = target_of(segments, self.kube.namespace)
        self.readable(target.resource, None)
        kind = headers.get("content-type")
        connecting = target.subresource in CONNECTS
        if method == "DELETE" and not connecting:
            return self.passed_delete(asked, target, query, body, kind)
        evicted = target.resource == "pods" and target.subresource == "eviction"
        if evicted:
            body = written(evicting(kept(body), self.owned_at(asked, target, method)[1])).encode()
        judged = target.named is not None and not evicted and (method in WRITES or connecting)
        whole = target.subresource in (None, "ephemeralcontainers")
        operation = None
        if method in WRITES and (whole or body):
            reading = self.schemas.reading()
            home, operation = reading.operation(segments, method)
        if operation is not None and operation.body:
            media = media_of(kind)
            if method == "PATCH":
                return self.patched(asked, target, query, whole, kind, media, reading.body(home, operation, media, kept(body)))
            if whole:
                requester = self.requester(asked) if method == "POST" else None
                made, given = confined_as_given(kept(body), "the object", self.cluster_kinds(), self.kube.namespace, labels_for(asked, requester))
                made, kind = reading.object(made, "the object"), "application/json"
            else:
                made = reading.body(home, operation, media, kept(body))
                given = stamps(made)
            before, version = ({}, None) if method == "POST" else self.current(asked, target, method)
            unmoved(before, given, "the body")
            if method == "POST" and judged:
                self.owned_at(asked, target, method)
            if whole:
                self.tried(made, "the object")
            body = written(held_to(made, version, None, "the body")).encode()
        elif judged:
            self.owned_at(asked, target, method)
        answered = self.api.call(method, target.path, query, body, kind)
        return answered.status, answered

    def passed_delete(self, asked, target, query, body, kind):
        if target.named is None:
            caller = self.requester(asked)
            if caller is None:
                raise Refused(403, f"a delete of every {target.resource} a query selects deletes only the caller's own, and egress names no account for this request")
            answered = self.api.call("DELETE", target.path, owned_only(query, caller), body, kind)
            return answered.status, answered
        what = object_named(target)
        uid = self.owned_at(asked, target, "DELETE")[1]
        answered = self.unmade(target.path, query, kept(body), uid, what)
        return answered.status, answered

    def current(self, asked, target, method):
        held = self.owned_at(asked, target, method)[0]
        version = mapping(mapping(held).get("metadata")).get("resourceVersion")
        if target.path != target.named:
            answered = self.api.call("GET", target.path, "", b"", None)
            if not 200 <= answered.status < 300:
                raise Refused(answered.status, f"{target.path} was not read before the write, so its {NOTE}* labels and annotations could not be held as they are: Kubernetes answered {answered.status}")
            held = json.loads(answered.body)
        return stamps(held), version

    def patched(self, asked, target, query, whole, kind, media, patch):
        what = "the patch"
        before, version = self.current(asked, target, "PATCH")
        body = written(held_to(patch, version, media, what)).encode()
        tried = self.api.call("PATCH", target.path, dry_run(query), body, kind)
        if not 200 <= tried.status < 300:
            return tried.status, tried
        result = json.loads(tried.body)
        unmoved(before, stamps(result), what)
        if whole:
            self.patchable(result)
        answered = self.api.call("PATCH", target.path, query, body, kind)
        return answered.status, answered

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
        try:
            return self.route(method, target, headers, body)
        except Refused as refusal:
            raise self.replaced(target, refusal) from refusal

    def replaced(self, target, refusal):
        segments = [segment for segment in urlsplit(target).path.split("/") if segment]
        if refusal.status != 404 or segments[:2] != ["v1", "pods"] or len(segments) < 3 or segments[2] in self.runners or segments[2] == BUILDS or not self.warm_pod:
            return refusal
        name = segments[2]
        try:
            current = self.warm()
        except Refused:
            return refusal
        if current == name:
            return refusal
        return Refused(404, f"{refusal.sentence}; if pod {name} was the warm pod, it has been replaced: the warm pod is now {current}")

    def fleet_scoped(self, asked, method, segments):
        if ASKER not in asked:
            return
        match (method, *segments[1:]):
            case ("POST", "pods"):
                return
            case ("GET" | "DELETE", "pods", name) | ("GET", "pods", name, "wait") | ("POST", "pods", name, "exec" | "blob"):
                pass
            case _:
                raise Refused(403, f"a call {FLEET_PROGRAM} relays makes, reads, waits for, execs in, puts the outputs of and deletes its own run's pod only, not {method} /{'/'.join(segments)}")
        if name in self.runners or name == BUILDS:
            raise Refused(403, f"pod {name} is a build runner, which no call {FLEET_PROGRAM} relays reaches")
        labels = self.pod(name)["metadata"].get("labels") or {}
        run, attempt = asked[FLEET_RUN], asked[FLEET_ATTEMPT]
        ours = labels.get(RUN_LABEL) == run and labels.get(ATTEMPT_LABEL) == attempt
        if not ours:
            raise Refused(403, f"pod {name} is not the pod of run {run} attempt {attempt}, which this call {FLEET_PROGRAM} relays is for")

    def route(self, method, target, headers, body):
        parts = urlsplit(target)
        segments = [segment for segment in parts.path.split("/") if segment]
        query = parse_qs(parts.query, keep_blank_values=True)
        asked = asked_by(headers)
        if segments[:1] != ["v1"]:
            raise Refused(404, f"{parts.path} is not a route; routes are under /v1")
        self.fleet_scoped(asked, method, segments)
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
                return self.wait(name, query, headers)
            case ("POST", "pods", name, "exec"):
                return self.exec(name, asked, parsed(body), headers)
            case ("POST", "pods", name, "read"):
                return self.contents(name, asked, parsed(body))
            case ("POST", "pods", name, "blob"):
                return self.blob(name, asked, parsed(body))
            case ("POST", "pods", name, "file"):
                return self.file(name, asked, parsed(body))
            case ("POST", "pods", name, "push"):
                return self.push(name, asked, parsed(body))
            case ("DELETE", "pods", name):
                return self.delete(asked, name)
            case ("POST", "manifests"):
                return self.apply(asked, body)
            case ("DELETE", "manifests"):
                return self.remove(asked, parsed(body))
            case ("GET", "manifests", kind):
                return self.read(kind, None)
            case ("GET", "manifests", kind, name):
                return self.read(kind, name)
            case ("POST", "report"):
                return self.report(parsed(body))
            case ("GET", "usage"):
                return self.usage()
            case (_, "k8s", *path):
                return self.passthrough(method, path, parts.query, asked, headers, body)
            case _:
                raise Refused(404, f"{method} {parts.path} is not a route")


POD_ROUTES = ("logs", "wait", "exec", "read", "blob", "file", "push")


def route_of(target):
    match [segment for segment in urlsplit(target).path.split("/") if segment]:
        case ["v1", ("pods" | "manifests" | "usage" | "report") as route]:
            return route
        case ["v1", "pods", _]:
            return "pods/NAME"
        case ["v1", "pods", _, action] if action in POD_ROUTES:
            return f"pods/NAME/{action}"
        case ["v1", "manifests", _]:
            return "manifests/KIND"
        case ["v1", "manifests", _, _]:
            return "manifests/KIND/NAME"
        case ["v1", "k8s", *_]:
            return "k8s"
        case _:
            return "unknown"


def parsed(body):
    try:
        return json.loads(body or b"{}")
    except json.JSONDecodeError as error:
        raise Refused(400, f"the body is not JSON: {error}") from error


def hang_up(connection):
    try:
        connection.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass


class Drain:
    def __init__(self):
        self.held = threading.Condition()
        self.whole = 0
        self.idle = set()
        self.streams = set()
        self.stopping = False

    def opened(self, connection):
        with self.held:
            if not self.stopping:
                self.idle.add(connection)

    def closed(self, connection):
        with self.held:
            self.idle.discard(connection)
            self.streams.discard(connection)

    def called(self, connection):
        with self.held:
            self.idle.discard(connection)
            self.whole += 1

    def answered(self, connection):
        with self.held:
            self.whole -= 1
            self.held.notify_all()
            if not self.stopping:
                self.idle.add(connection)
            return not self.stopping

    def streaming(self, connection):
        with self.held:
            self.whole -= 1
            self.held.notify_all()
            if not self.stopping:
                self.streams.add(connection)
            return not self.stopping

    def stop(self):
        with self.held:
            self.stopping = True
            cut, self.idle, self.streams = self.idle | self.streams, set(), set()
        for connection in cut:
            hang_up(connection)
        with self.held:
            self.held.wait_for(lambda: self.whole == 0)
        return len(cut)


def handler(service, drain=None):
    drain = Drain() if drain is None else drain

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def setup(self):
            super().setup()
            drain.opened(self.connection)

        def finish(self):
            drain.closed(self.connection)
            super().finish()

        def respond(self, method):
            drain.called(self.connection)
            streaming = False
            try:
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
                    streaming = True
                    self.stream(status, answer)
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
                if drain.stopping:
                    self.send_header("connection", "close")
                    self.close_connection = True
                self.end_headers()
                self.wfile.write(payload)
            finally:
                kept = streaming or drain.answered(self.connection)
                if not kept:
                    self.close_connection = True

        def stream(self, status, answer):
            events = answer.events
            kept = drain.streaming(self.connection)
            self.send_response(status)
            self.send_header("content-type", "text/event-stream")
            if answer.reattach:
                self.send_header(REATTACH, REATTACH_BY)
            self.send_header("cache-control", "no-cache")
            self.send_header("transfer-encoding", "chunked")
            self.send_header("connection", "close")
            self.end_headers()
            said = streamed(events)
            try:
                if not kept:
                    self.wfile.flush()
                    hang_up(self.connection)
                for event in said:
                    self.wfile.write(chunk(event))
                    self.wfile.flush()
                self.wfile.write(LAST_CHUNK)
                self.wfile.flush()
            except OSError:
                say("pods_stream_unheard", reason="stopping" if drain.stopping else "caller_gone")
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
            status = args[1] if len(args) > 1 else ""
            say("pods_request", method=self.command, route=route_of(self.path), status=status)

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


HARDCAP = ("cpu", "memory")
HELD_PHASES = ("Running", "Pending")
GIB = 1 << 30


def request_of(holder, name):
    return ((holder.get("resources") or {}).get("requests") or {}).get(name)


def pod_requests(pod):
    spec = pod.get("spec") or {}
    stated = {name: request_of(spec, name) for name in HARDCAP}
    return {
        name: quantity(stated[name]) if stated[name] is not None else sum((quantity(request_of(container, name)) for container in spec.get("containers") or [] if request_of(container, name) is not None), Fraction(0))
        for name in HARDCAP
    }


def hardcap(nodes, pods, namespace):
    free = {name: sum((quantity(node["status"]["allocatable"][name]) for node in nodes["items"]), Fraction(0)) for name in HARDCAP}
    for pod in pods["items"]:
        if pod["metadata"]["namespace"] == namespace or (pod.get("status") or {}).get("phase") not in HELD_PHASES:
            continue
        for name, value in pod_requests(pod).items():
            free[name] -= value
    return f"{math.floor(free['cpu'] * 1000)}m", f"{math.floor(free['memory'] / GIB)}Gi"


def shown(name, value):
    if name == "cpu":
        return f"{value}" if value.denominator == 1 else f"{math.floor(value * 1000)}m"
    for unit, size in (("Gi", GIB), ("Mi", 1 << 20), ("Ki", 1 << 10)):
        if value % size == 0:
            return f"{value // size}{unit}"
    return f"{value}"


def under_held(caps, quota, pods, namespace):
    status = mapping(mapping(quota).get("status"))
    used, hard = mapping(status.get("used")), mapping(status.get("hard"))
    said = []
    for name in HARDCAP:
        key = f"requests.{name}"
        cap = quantity(caps[name])
        held = used.get(key)
        lowered = hard.get(key) is None or cap < quantity(hard[key])
        refused = held is not None and cap < quantity(held) and lowered
        if not refused:
            continue
        ours = [pod for pod in pods["items"] if pod["metadata"]["namespace"] == namespace and (pod.get("status") or {}).get("phase") in HELD_PHASES]
        holding = sorted(((pod_requests(pod)[name], pod["metadata"]["name"], pod["status"]["phase"]) for pod in ours), key=lambda row: (-row[0], row[1]))
        listed = ", ".join(f"{pod} ({phase}) {shown(name, value)}" for value, pod, phase in holding if value > 0)
        said.append(
            f"hardcap's {key} would drop to {caps[name]}, below the {held} that pods in {namespace} hold now (its status.used), so deploy applies nothing; "
            f"those pods hold, largest first: {listed}. Deploy again once enough of them end."
        )
    return said


def held_quota(kubectl, namespace):
    ran = subprocess.run([*kubectl, "get", "resourcequota", "hardcap", "--namespace", namespace, "-o", "json"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if ran.returncode == 0:
        return json.loads(ran.stdout)
    if "(NotFound)" in ran.stderr:
        return None
    sys.exit(f"hardcap in {namespace} was not read, so what its pods hold is unknown and deploy applies nothing: {ran.stderr.strip() or f'kubectl exited {ran.returncode}'}")


def hardcap_main():
    kubectl = shlex.split(os.environ.get("PODS_KUBECTL", "kubectl"))
    namespace = os.environ.get("PODS_NAMESPACE", "agents")
    listed = [json.loads(subprocess.run([*kubectl, "get", *kind, "-o", "json"], check=True, stdout=subprocess.PIPE, text=True).stdout) for kind in (["nodes"], ["pods", "--all-namespaces"])]
    quota = held_quota(kubectl, namespace)
    try:
        cpu, memory = hardcap(*listed, namespace)
    except Refused as refusal:
        sys.exit(refusal.sentence)
    refused = under_held({"cpu": cpu, "memory": memory}, quota, listed[1], namespace)
    if refused:
        sys.exit(" ".join(refused))
    print(cpu, memory)


def fetch_main():
    ducktape = shlex.split(os.environ.get("PODS_DUCKTAPE_CLI", "ducktape"))
    fetcher = Fetcher(ducktape, os.environ["PODS_NODE"], os.environ["PODS_FORGE"], Path(tempfile.mkdtemp()), block_ms=int(os.environ["PODS_BLOCK_MS"]))
    try:
        fetcher.fetch(json.loads(os.environ["PODS_INPUTS"]), Path(IN_ROOT))
    except Refused as refusal:
        print(refusal.sentence, file=sys.stderr, flush=True)
        Path("/dev/termination-log").write_text(refusal.sentence)
        sys.exit(1)


def signal_woken(number, frame):
    pass


class Signals:
    def __init__(self, *numbers):
        self.numbers = set(numbers)
        self.heard, self.said = socket.socketpair()
        self.said.setblocking(False)
        signal.set_wakeup_fd(self.said.fileno(), warn_on_full_buffer=False)
        for number in numbers:
            signal.signal(number, signal_woken)

    def wait(self):
        while True:
            number = self.heard.recv(1)[0]
            if number in self.numbers:
                return signal.Signals(number).name


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
    try:
        runners = runners_of(os.environ.get("PODS_RUNNERS", f"{DESKTOP}={DESKTOP_KEYS}"), signer)
    except ValueError as error:
        sys.exit(str(error))
    service = Service(kube, signer, fetching, Stats(), Api(), runners, os.environ.get("PODS_WARM_POD"))
    threading.Thread(target=service.notifier.watch, daemon=True).start()
    threading.Thread(target=service.sweeper, daemon=True).start()
    if signer.held():
        threading.Thread(target=service.rerunner.watch, daemon=True).start()
    for runner in runners:
        if runner.held():
            service.follow(runner.name)
    host, port = os.environ.get("PODS_LISTEN", "0.0.0.0:8080").rsplit(":", 1)
    drain = Drain()
    server = ThreadingHTTPServer((host, int(port)), handler(service, drain))
    signals = Signals(signal.SIGTERM)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    bound, at = server.server_address[:2]
    say("pods_serving", namespace=kube.namespace, listen=f"{bound}:{at}", keyed=signer.held())
    stopping = signals.wait()
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    server.shutdown()
    server.server_close()
    say("pods_stopping", signal=stopping)
    cut = drain.stop()
    say("pods_drained", cut=cut)
    os._exit(0)


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["fetch"]:
            fetch_main()
        case ["hardcap"]:
            hardcap_main()
        case []:
            main()
        case other:
            sys.exit(f"usage: server.py [fetch|hardcap], not {' '.join(other)}")
