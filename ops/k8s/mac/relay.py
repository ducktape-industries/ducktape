#!/usr/bin/env python3
# The mac service's relay, the egress service `mac`: agents' runs (a phone build of a forge commit, then a UI script in the iOS simulator) held on the cluster for a Mac that dials out to it, pulls them and sends back their states and outputs.
# A run's script and media are read from files when it is asked for, its source is bundled from forge for the Mac, its outputs go into files in one commit, and one post says it ended. With MAC_LABELS it is also an actions runner for the Mac: it claims the checks those labels cover while the Mac is idle and reports them.
# ops/k8s/mac/mac.yaml runs it, relay-test.py tests it, and ops/mac/mac.py is the Mac's side.
import hmac
import json
import os
import re
import secrets
import select
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path, PurePosixPath
from urllib.parse import parse_qs, unquote, urlsplit

ASKED = ("chain", "program", "request")
REQUEST_NUMBER = re.compile(r"[0-9]+")
OID = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
VARIABLE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
RUN = {
    "repo": str,
    "commit": str,
    "build": str,
    "from": str,
    "vars": dict,
    "script": dict,
    "device": dict,
    "to": str,
    "requester": int,
    "notify": dict,
}
BUILT = ("repo", "commit", "build")
DEVICE = {"fresh": bool, "type": str, "runtime": str}
NOTIFY = {"channel": str, "thread": int}
SCRIPT = {"steps": list, "files": dict}
FILES = {"path": str, "snapshot": str}
MEDIA = {"files": dict}
SYSTEM = {"tap": str, "app": str, "deadline_ms": int}
CHANGED = {"name": str, "deadline_ms": int}
STEPS = {
    "launch": dict,
    "terminate": dict,
    "wait": dict,
    "press": dict,
    "type": dict,
    "scroll": dict,
    "tree": str,
    "screenshot": str,
    "changed": dict,
    "media": dict,
    "system": dict,
}
STATE = {"state": str, "step": str, "reason": str, "exit": int}
WORK = {"facts": dict, "runs": list}
SOURCE = {"repo": str, "commit": str, "have": list}
WAIT = {"every"}
JSON_KINDS = {str: "string", int: "number", bool: "boolean", dict: "object", list: "array"}
QUEUED = "queued"
WORKING = ("accepted", "building", "testing", "writing")
ENDED = ("succeeded", "failed", "interrupted", "stopped")
VIEWED = (
    "id",
    "created",
    "state",
    "step",
    "reason",
    "repo",
    "commit",
    "from",
    "build",
    "vars",
    "device",
    "to",
    "steps",
    "requester",
    "notify",
    "asked",
    "started",
    "finished",
    "outputs",
)
CHAT = "chat"
FILES_PROGRAM = "files"
CHUNK = "chunk"
CHUNK_SIZE = 1 << 20
KEEPALIVE = (10, 5, 3)
HASHES = {"sha256": (0, 32), "sha1": (1, 20)}
ACTIONS = "actions"
OUTPUT = "output"
LOG = "log"
FOLLOW_PAUSE = 5
CHECK_STATES = ("queued", "running", "passed", "failed", "errored", "superseded", "waiting", "skipped")


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


@dataclass
class Raw:
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


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def failed(ran):
    if ran.code == 0:
        return None
    return ran.err or ran.out.decode(errors="replace").strip() or f"exited {ran.code}"


def moved_under(outcome, reason, sentence):
    if outcome != "rejected":
        return False
    return reason == "stale" or (reason == "not_found" and "minted no snapshot" in sentence)


def receipt(ran):
    text = ran.out.decode(errors="replace").strip()
    applied = re.match(r"preconfirmed applied ?([0-9a-f]*)", text)
    rejected = re.match(r"preconfirmed rejected (\w+): (.*)", text, re.S)
    if ran.code == 0 and applied:
        return "applied", applied.group(1), ""
    if ran.code == 0 and rejected:
        return "rejected", rejected.group(1), rejected.group(2).strip()
    return "failed", "", ran.err or text or f"ducktape exited {ran.code}"


def put_answer(text, count):
    ids = [line.strip() for line in text.splitlines()[:count]]
    if len(ids) != count or not all(":" in blob for blob in ids):
        raise Refused(502, f"the blob put printed {text!r}, not {count} blob ids")
    return ids


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


def blob_id(text):
    kind, _, digest = text.partition(":")
    known = HASHES.get(kind)
    if known is None or not re.fullmatch(r"[0-9a-f]+", digest) or len(digest) != 2 * known[1]:
        raise Refused(400, f"{text} is not a blob id: one is sha256:<64 hex> or sha1:<40 hex>")
    return u8(known[0]) + bytes.fromhex(digest)


def u32(value):
    return value.to_bytes(4, "little")


def actions_register(labels):
    return u8(2) + borsh_vec([borsh_text(label) for label in labels]) + borsh_bytes(b"")


def actions_claim(check):
    return u8(3) + u64(check)


def actions_report(check, state, summary, log):
    return u8(4) + u64(check) + u8(CHECK_STATES.index(state)) + borsh_text(summary) + borsh_option(log, borsh_text)


def actions_outputs(check, files):
    return u8(7) + u64(check) + borsh_vec([borsh_text(path) + blob_id(blob) + u64(size) for path, blob, size in files])


def actions_check(check):
    return u8(0) + u64(check)


def actions_queued(after):
    return u8(2) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def actions_needs(check):
    return u8(6) + u64(check)


def actions_outputs_of(check):
    return u8(7) + u64(check)


def unframed(framed):
    header, nul, body = framed.partition(b"\0")
    if not nul:
        raise Refused(502, "a blob came back with no frame")
    return body


def placed(job, path):
    parts = [job, *path.split("/")]
    if any(part in ("", ".", "..") for part in parts) or "/" in job:
        raise Refused(422, f"check output {path!r} of `{job}` would land outside its needs folder")
    return "/".join(parts)


def check_dir(check):
    workflow = check["workflow"].removesuffix(".toml")
    return f"/shared/actions/{check['repo']}/{check['commit']}/{workflow}/{check['job']['id']}-{check['attempt']}"


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

    def u32(self):
        return int.from_bytes(self.take(4), "little")

    def text(self):
        return self.bytes().decode()

    def option(self, read):
        return read() if self.u8() else None

    def vec(self, read):
        return [read() for _ in range(self.u32())]

    def check(self):
        row = {"id": self.u64(), "run": self.u64(), "repo": self.text(), "commit": self.text(), "workflow": self.text(), "attempt": self.u32()}
        job = {"id": self.text(), "runs_on": self.vec(self.text), "image": self.text(), "steps": self.vec(self.text)}
        job["env"] = dict(self.vec(lambda: (self.text(), self.text())))
        self.vec(lambda: (self.text(), self.text()))
        state = self.u8()
        row.update(job=job, state=CHECK_STATES[state] if state < len(CHECK_STATES) else f"state {state}")
        self.option(self.u64), self.option(self.u64), self.u64(), self.option(self.u64), self.option(self.u64), self.text(), self.option(self.text)
        return row

    def page(self, read):
        self.u64()
        return self.vec(read), self.option(self.bytes)


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


def files_commit(base, message, files):
    puts = [
        u8(0) + borsh_text(path) + u8(0) + (0).to_bytes(4, "little") + u8(1) + u64(size) + borsh_vec([blob_id(chunk) for chunk in chunks])
        for path, size, chunks in files
    ]
    return u8(1) + borsh_option(base, blob_id) + borsh_text(message) + borsh_vec(puts)


def kept_alive(sock):
    idle, interval, count = KEEPALIVE
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
    idle_option = getattr(socket, "TCP_KEEPIDLE", None) or socket.TCP_KEEPALIVE
    for option, value in ((idle_option, idle), (socket.TCP_KEEPINTVL, interval), (socket.TCP_KEEPCNT, count)):
        sock.setsockopt(socket.IPPROTO_TCP, option, value)
    if hasattr(socket, "TCP_USER_TIMEOUT"):
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_USER_TIMEOUT, (idle + interval * count) * 1000)


def snapshot_of(text):
    return text if text is None or ":" in text else f"sha256:{text}"


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


def checked(body, shape, what):
    if not isinstance(body, dict):
        raise Refused(400, f"{what} is a JSON object")
    unknown = sorted(set(body) - set(shape))
    if unknown:
        raise Refused(400, f"{what} has no field {', '.join(unknown)}; its fields are {', '.join(shape)}")
    for field, kind in shape.items():
        if field not in body:
            continue
        value = body[field]
        a_bool_for_another_kind = isinstance(value, bool) and kind is not bool
        if not isinstance(value, kind) or a_bool_for_another_kind:
            raise Refused(400, f"{what}'s {field} is a JSON {JSON_KINDS[kind]}")
    return body


def named(body, fields, what):
    missing = [field for field in fields if field not in body or body[field] in ("", None)]
    if missing:
        raise Refused(400, f"{what} names its {' and '.join(missing)}")
    return body


def absolute(path, field):
    if not path.startswith("/"):
        raise Refused(400, f"{field} is an absolute path")
    return path


def from_zero(value, field):
    if value < 0:
        raise Refused(400, f"{field} is a number from 0")
    return value


def asked_by(headers):
    asked = {name: headers.get(f"ducktape-{name}") for name in ASKED}
    missing = [f"ducktape-{name}" for name, value in asked.items() if not value]
    if missing:
        raise Refused(400, f"this service answers a network's egress calls only, and {', '.join(missing)} is missing")
    if not REQUEST_NUMBER.fullmatch(asked["request"]):
        raise Refused(400, f"ducktape-request {asked['request']!r} is not an egress request number")
    return asked


def wait_every(query):
    unknown = sorted(set(query) - WAIT)
    if unknown:
        raise Refused(400, f"a wait takes no {', '.join(unknown)}; it takes every")
    value = query.get("every", [""])[-1]
    if not value.isdigit() or int(value) < 1:
        raise Refused(400, "a wait names every: the whole seconds from 1 between the alive events that keep the caller's request from expiring")
    return int(value)


def source_of(body):
    built = [field for field in BUILT if field in body]
    reused = "from" in body
    match (reused, built):
        case (True, []):
            return {"from": body["from"], "repo": None, "commit": None, "build": None}
        case (False, ["repo", "commit", "build"]):
            if not OID.fullmatch(body["commit"]):
                raise Refused(400, f"commit {body['commit']!r} is a whole commit id, 40 or 64 lowercase hex digits")
            return {"from": None, "repo": body["repo"], "commit": body["commit"], "build": body["build"]}
        case (True, _):
            raise Refused(400, "a run builds a forge commit (repo, commit, build) or reuses another run's build (from), not both")
        case _:
            missing = [field for field in BUILT if field not in built]
            raise Refused(400, f"a run names repo, commit and build, or from; it lacks {', '.join(missing)}")


def vars_of(given):
    for name, value in given.items():
        if not VARIABLE.fullmatch(name):
            raise Refused(400, f"vars' {name!r} is not a make variable name")
        if not isinstance(value, str):
            raise Refused(400, f"vars' {name} is a JSON string")
    return given


def notify_of(given):
    notify = named(checked(given, NOTIFY, "notify"), ["channel"], "notify")
    if "thread" in notify:
        from_zero(notify["thread"], "notify's thread")
    return notify


def spec_of(body):
    body = checked(body, RUN, "a run")
    named(body, ["to"], "a run")
    spec = source_of(body)
    spec["to"] = absolute(body["to"], "to")
    spec["vars"] = vars_of(body.get("vars", {}))
    spec["device"] = checked(body.get("device", {}), DEVICE, "device")
    spec["requester"] = from_zero(body["requester"], "requester") if "requester" in body else None
    spec["notify"] = notify_of(body["notify"]) if "notify" in body else None
    return spec


def step_of(step, number):
    what = f"step {number}"
    if not isinstance(step, dict) or len(step) != 1:
        raise Refused(400, f"{what} is a JSON object with one key, one of {', '.join(STEPS)}")
    ((name, value),) = step.items()
    kind = STEPS.get(name)
    if kind is None:
        raise Refused(400, f"{what} is {name}, not a step; the steps are {', '.join(STEPS)}")
    if not isinstance(value, kind):
        raise Refused(400, f"{what}'s {name} is a JSON {JSON_KINDS[kind]}")
    match name:
        case "tree" | "screenshot":
            if not NAME.fullmatch(value):
                raise Refused(400, f"{what}'s {name} names its output with letters, digits, '.', '_' and '-'")
        case "changed":
            named(checked(value, CHANGED, f"{what}'s changed"), ["name", "deadline_ms"], f"{what}'s changed")
            from_zero(value["deadline_ms"], f"{what}'s deadline_ms")
            if not NAME.fullmatch(value["name"]):
                raise Refused(400, f"{what}'s changed names its output with letters, digits, '.', '_' and '-'")
        case "system":
            named(checked(value, SYSTEM, f"{what}'s system"), ["tap", "deadline_ms"], f"{what}'s system")
            from_zero(value["deadline_ms"], f"{what}'s deadline_ms")
        case "media":
            files = checked(named(checked(value, MEDIA, f"{what}'s media"), ["files"], f"{what}'s media")["files"], FILES, f"{what}'s files")
            named(files, ["path"], f"{what}'s files")
    return name, value


def steps_of(data, path):
    try:
        script = json.loads(data)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise Refused(400, f"the script {path} is not JSON: {error}") from error
    steps = script.get("steps") if isinstance(script, dict) else None
    if not isinstance(steps, list):
        raise Refused(400, f"the script {path} is a JSON object with steps, an array")
    return steps


def view(record):
    viewed = {field: record.get(field) for field in VIEWED}
    if record.get("check") is not None:
        viewed["check"] = record["check"]
    return viewed


def ended_line(record):
    count = len(record.get("outputs") or [])
    where = f"{count} outputs in {record['to'].rstrip('/')}/{record['id']}/" if count else "no outputs were written"
    if record["state"] == "succeeded":
        return f"mac run {record['id']} succeeded: {where}"
    return f"mac run {record['id']} {record['state']}: {record.get('reason') or 'no reason was given'}; {where}"


def output_path(segments):
    parts = [unquote(segment) for segment in segments]
    unsafe = not parts or any(part in ("", ".", "..") or "/" in part for part in parts)
    if unsafe:
        raise Refused(400, "an output's path is relative, with no empty, '.' or '..' part")
    return PurePosixPath(*parts)


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
        return run([*self.ducktape, "query", program, "--raw"], stdin=request, env=self.env())

    def blob(self, blob):
        return run([*self.ducktape, "blob", "get", blob], env=self.env())

    def follow(self, program):
        return Following([*self.ducktape, "changes", program], self.env())


class Following:
    def __init__(self, argv, env):
        self.argv = argv
        self.env = env
        self.why = None

    def __iter__(self):
        process = subprocess.Popen(self.argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env)
        with process.stdout:
            yield from process.stdout
        said = process.stderr.read().decode(errors="replace").strip()
        process.stderr.close()
        self.why = said or f"exited {process.wait()}"


class Forge:
    def __init__(self, root, forge, node, runner=run):
        self.root = root
        self.forge = forge.rstrip("/")
        parts = urlsplit(forge)
        self.git = ["git", "-c", f"duck.{parts.netloc}.node={node}"] if parts.scheme == "duck" else ["git"]
        self.runner = runner
        self.env = dict(os.environ, DUCKTAPE_PASSWORD=secrets.token_hex(16), NO_COLOR="1", DUCKTAPE_DEVICE_KEY_STORE="file")
        self.locks = {}
        self.lock = threading.Lock()

    def lock_of(self, repo):
        with self.lock:
            return self.locks.setdefault(repo, threading.Lock())

    def must(self, argv, doing):
        ran = self.runner(argv, env=self.env)
        if ran.code != 0:
            raise Refused(502, f"{doing}: {failed(ran)}")
        return ran.out

    def holds(self, mirror, oid):
        return self.runner([*self.git, "-C", str(mirror), "cat-file", "-e", f"{oid}^{{commit}}"], env=self.env).code == 0

    def bundle(self, repo, commit, have):
        mirror = self.root / f"{repo}.git"
        inside = [*self.git, "-C", str(mirror)]
        with self.lock_of(repo):
            if not mirror.exists():
                self.root.mkdir(parents=True, exist_ok=True)
                self.must([*self.git, "clone", "--mirror", f"{self.forge}/{repo}", str(mirror)], f"{repo} was not cloned from forge")
            if not self.holds(mirror, commit):
                self.must([*inside, "fetch", "--prune", "origin"], f"{repo} was not fetched from forge")
            if not self.holds(mirror, commit):
                ran = self.runner([*inside, "fetch", "origin", commit], env=self.env)
                if ran.code != 0:
                    raise Refused(404, f"forge's {repo} has no commit {commit}: {failed(ran)}")
            known = [oid for oid in have if self.holds(mirror, oid)]
            ref = f"refs/mac/{commit}"
            self.must([*inside, "update-ref", ref, commit], f"{commit} was not named in {repo}")
            with tempfile.TemporaryDirectory() as temp:
                file = Path(temp) / "source.bundle"
                exclusions = ["--not", *known] if known else []
                self.must([*inside, "bundle", "create", str(file), ref, *exclusions], f"{repo} at {commit} was not bundled")
                return file.read_bytes()


class Store:
    def __init__(self, root):
        self.root = root

    def file(self):
        return self.root / "runs.json"

    def load(self):
        try:
            return json.loads(self.file().read_text())
        except FileNotFoundError:
            return {}

    def save(self, runs):
        self.root.mkdir(parents=True, exist_ok=True)
        fresh = self.root / "runs.json.new"
        fresh.write_text(json.dumps(runs))
        os.replace(fresh, self.file())

    def run_dir(self, name):
        return self.root / "runs" / name


class Work:
    def __init__(self):
        self.events = []
        self.closed = False
        self.lock = threading.Lock()
        self.woken, self.wake = os.pipe()

    def send(self, event):
        with self.lock:
            self.events.append(event)
            os.write(self.wake, b".")

    def close(self):
        with self.lock:
            self.closed = True
            os.write(self.wake, b".")

    def taken(self):
        with self.lock:
            events, self.events = self.events, []
            return events, self.closed

    def waited(self, connection):
        ready, _, _ = select.select([connection, self.woken], [], [])
        if self.woken in ready:
            os.read(self.woken, 4096)
        return connection not in ready

    def release(self):
        os.close(self.woken)
        os.close(self.wake)


class Service:
    def __init__(self, signer, store, token, forge, clock=now, labels=()):
        self.signer = signer
        self.labels = tuple(labels)
        self.pause = time.sleep
        self.following = False
        self.pulling = False
        self.unnamed = set()
        self.store = store
        self.forge = forge
        self.token = token
        self.clock = clock
        self.changed = threading.Condition()
        self.runs = store.load()
        self.mac = {"connected": False, "since": clock(), "facts": {}}
        self.work = None
        self.finishing = set()
        self.keep = kept_alive
        self.spawn = lambda job: threading.Thread(target=job, daemon=True).start()

    def resume(self):
        unwritten = [name for name, record in self.runs.items() if "ending" in record]
        for name in unwritten:
            self.spawn(lambda name=name: self.finish(name))
        if self.labels:
            outcome, reason, sentence = receipt(self.signer.submit(ACTIONS, actions_register(self.labels)))
            say("mac_registered", labels=",".join(self.labels), outcome=outcome, detail=json.dumps(f"{reason}: {sentence}"))
            self.follow()

    def follow(self):
        with self.changed:
            if not self.labels or self.following:
                return
            self.following = True
        self.spawn(self.followed)

    def followed(self):
        try:
            while True:
                following = self.signer.follow(ACTIONS)
                heard, height = False, None
                for line in following:
                    heard = True
                    at = line.split(b"\t", 1)[0]
                    if at != height:
                        height = at
                        self.settle_checks()
                        self.pull()
                say("mac_unfollowed", heard=heard, detail=json.dumps(following.why))
                if not heard:
                    self.pause(FOLLOW_PAUSE)
        finally:
            with self.changed:
                self.following = False

    def asked_actions(self, request, what):
        ran = self.signer.query(ACTIONS, request)
        if ran.code != 0:
            raise Refused(502, f"actions did not answer {what}: {failed(ran)}")
        return Reader(ran.out)

    def queued(self):
        checks, after = [], None
        while True:
            reader = self.asked_actions(actions_queued(after), "Queued")
            reader.u8()
            items, after = reader.page(reader.check)
            checks.extend(items)
            if after is None:
                return checks

    def check_of(self, check):
        reader = self.asked_actions(actions_check(check), "Check")
        reader.u8()
        return reader.option(reader.check)

    def needed(self, check):
        reader = self.asked_actions(actions_needs(check), "Needs")
        reader.u8()
        needs = reader.vec(lambda: (reader.text(), reader.option(reader.u64)))
        inputs = []
        for job, met in needs:
            if met is None:
                raise Refused(422, f"needs `{job}`, which no passed check met")
            reader = self.asked_actions(actions_outputs_of(met), "Outputs")
            reader.u8()
            row = reader.option(lambda: (reader.u64(), reader.vec(lambda: (reader.text(), reader.blob(), reader.u64()))))
            for path, blob, _ in row[1] if row else []:
                name = placed(job, path)
                got = self.signer.blob(blob)
                if got.code != 0:
                    raise Refused(502, f"blob {blob} was not fetched: {failed(got)}")
                inputs.append((name, unframed(got.out)))
        return inputs

    def idle(self):
        return self.mac["connected"] and all(record["state"] in ENDED for record in self.runs.values())

    def pull(self):
        with self.changed:
            unreported = [name for name, record in self.runs.items() if record.get("check") and "ending" in record]
        self.again(unreported)
        with self.changed:
            if not self.labels or self.pulling or not self.idle():
                return
            self.pulling = True
        try:
            queued = self.queued()
            self.unnamed &= {check["id"] for check in queued}
            covered = [check for check in queued if set(check["job"]["runs_on"]) <= set(self.labels) and check["id"] not in self.unnamed]
            for check in sorted(covered, key=lambda check: check["id"]):
                outcome, reason, sentence = receipt(self.signer.submit(ACTIONS, actions_claim(check["id"])))
                if outcome == "applied":
                    self.checked(check)
                    return
                say("mac_unclaimed", check=check["id"], reason=reason or outcome, detail=json.dumps(sentence))
                if reason == "unauthorized":
                    self.unnamed.add(check["id"])
                if reason not in ("wrong_state", "unauthorized"):
                    return
        except Refused as refusal:
            say("mac_unpulled", detail=json.dumps(refusal.sentence))
        finally:
            with self.changed:
                self.pulling = False

    def checked(self, check):
        name = f"check-{check['id']}"
        try:
            inputs = self.needed(check["id"])
        except Refused as refusal:
            inputs, unstarted = [], refusal.sentence
        else:
            unstarted = None
        with self.changed:
            folder = self.store.run_dir(name) / "inputs"
            folder.mkdir(parents=True, exist_ok=True)
            for index, (_, data) in enumerate(inputs):
                (folder / str(index)).write_bytes(data)
            record = {
                "from": None,
                "repo": check["repo"],
                "commit": check["commit"],
                "build": None,
                "to": check_dir(check),
                "vars": {},
                "device": {},
                "requester": None,
                "notify": None,
                "id": name,
                "created": self.clock(),
                "state": QUEUED,
                "step": None,
                "reason": None,
                "steps": None,
                "asked": None,
                "started": None,
                "finished": None,
                "outputs": [],
                "script": None,
                "inputs": [input_name for input_name, _ in inputs],
                "check": check["id"],
                "job": {"steps": check["job"]["steps"], "env": check["job"]["env"]},
            }
            if unstarted is not None:
                record.update(state="writing", step="reporting the check", ending={"state": "failed", "reason": unstarted})
            self.runs[name] = record
            if unstarted is None:
                self.offer(record)
            self.saved()
        if unstarted is not None:
            self.finish(name)

    def settle_checks(self):
        with self.changed:
            open_checks = [(name, record["check"]) for name, record in self.runs.items() if record.get("check") and record["state"] not in ENDED and "ending" not in record]
        for name, check in open_checks:
            try:
                row = self.check_of(check)
            except Refused as refusal:
                say("mac_unsettled", check=check, detail=json.dumps(refusal.sentence))
                continue
            if row is not None and row["state"] in ("queued", "running"):
                continue
            with self.changed:
                if self.runs.pop(name, None) is None:
                    continue
                if self.work is not None:
                    self.work.send(sse("forget", {"id": name}))
                self.saved()
            shutil.rmtree(self.store.run_dir(name), ignore_errors=True)
            say("mac_dropped", check=check, state=row["state"] if row else "gone")

    def reported_now(self, check, state, summary, log):
        outcome, reason, sentence = receipt(self.signer.submit(ACTIONS, actions_report(check, state, summary, log)))
        if outcome == "failed":
            return sentence
        if outcome == "rejected":
            say("mac_unreported", check=check, reason=reason, detail=json.dumps(sentence))
        return None

    def presence(self):
        mac = dict(self.mac)
        if not mac["connected"]:
            mac["sentence"] = f"the Mac is asleep or unreachable since {mac['since']}; queued runs start when it wakes"
        return mac

    def keyed(self, doing):
        if not self.signer.held():
            raise Refused(503, f"this service holds no key to {doing} with")

    def saved(self):
        self.store.save(self.runs)
        self.changed.notify_all()

    def known(self, name):
        record = self.runs.get(name)
        if record is None:
            raise Refused(404, f"there is no run {name}")
        return record

    def again(self, names):
        with self.changed:
            unwritten = [name for name in names if "ending" in self.runs.get(name, {}) and name not in self.finishing]
        for name in unwritten:
            self.spawn(lambda name=name: self.finish(name))

    def read(self, files):
        path, snapshot = files["path"], snapshot_of(files.get("snapshot"))
        data = b""
        while True:
            request = files_read(path, snapshot, len(data), CHUNK_SIZE)
            ran = self.signer.query(FILES_PROGRAM, request)
            if ran.code != 0:
                raise Refused(422, f"files did not read {path}: {failed(ran)}")
            read_at, _, _, piece, eof = contents_of(ran.out)
            snapshot = snapshot or read_at
            data += piece
            if eof or not piece:
                return data

    def scripted(self, given):
        if given is None:
            return None, []
        given = checked(given, SCRIPT, "the script")
        match sorted(given):
            case ["steps"]:
                steps = given["steps"]
            case ["files"]:
                files = named(checked(given["files"], FILES, "the script's files"), ["path"], "the script's files")
                steps = steps_of(self.read(files), files["path"])
            case _:
                raise Refused(400, "a script is {steps} or {files}, one of them")
        script, inputs = [], []
        for number, step in enumerate(steps, 1):
            name, value = step_of(step, number)
            if name == "media":
                files = value["files"]
                inputs.append((PurePosixPath(files["path"]).name, self.read(files)))
                value = {"input": len(inputs) - 1, "name": inputs[-1][0]}
            script.append({name: value})
        return script, inputs

    def handed(self, record):
        handed = {"run": view(record), "script": record["script"], "inputs": record["inputs"]}
        if record.get("job") is not None:
            handed["job"] = record["job"]
        return sse("run", handed)

    def offer(self, record):
        if self.work is None:
            return
        self.work.send(self.handed(record))

    def create(self, asked, body):
        self.keyed("read a run's inputs and write its outputs")
        spec = spec_of(body)
        name = f"{asked['chain']}-{asked['request']}"
        with self.changed:
            if name in self.runs:
                return 200, {"run": view(self.runs[name]), "mac": self.presence()}
            if spec["from"] is not None:
                self.known(spec["from"])
        script, inputs = self.scripted(body.get("script"))
        with self.changed:
            if name in self.runs:
                return 200, {"run": view(self.runs[name]), "mac": self.presence()}
            folder = self.store.run_dir(name) / "inputs"
            folder.mkdir(parents=True, exist_ok=True)
            for index, (_, data) in enumerate(inputs):
                (folder / str(index)).write_bytes(data)
            record = {
                **spec,
                "id": name,
                "created": self.clock(),
                "state": QUEUED,
                "step": None,
                "reason": None,
                "steps": None if script is None else len(script),
                "asked": asked,
                "started": None,
                "finished": None,
                "outputs": [],
                "script": script,
                "inputs": [input_name for input_name, _ in inputs],
            }
            self.runs[name] = record
            self.offer(record)
            self.saved()
            return 201, {"run": view(record), "mac": self.presence()}

    def listed(self):
        self.again(list(self.runs))
        with self.changed:
            runs = [view(record) for record in sorted(self.runs.values(), key=lambda record: (record["created"], record["id"]))]
        return 200, {"runs": runs, "mac": self.presence()}

    def status(self, name):
        self.again([name])
        with self.changed:
            return 200, {"run": view(self.known(name)), "mac": self.presence()}

    def ended_of(self, name):
        record = self.runs.get(name)
        if record is None:
            return {"line": f"run {name} was deleted", "run": None}
        if record["state"] in ENDED:
            return {"line": ended_line(record), "run": view(record)}
        return None

    def wait(self, name, query):
        every = wait_every(query)
        with self.changed:
            self.known(name)
        self.again([name])
        return 200, Stream(lambda connection: self.waited(name, every))

    def waited(self, name, every):
        due = time.monotonic() + every
        while True:
            with self.changed:
                ended = self.ended_of(name)
                if ended is None:
                    self.changed.wait(max(due - time.monotonic(), 0))
                    ended = self.ended_of(name)
            if ended is not None:
                yield sse("ended", ended)
                return
            if time.monotonic() >= due:
                due += every
                yield sse("alive", {})

    def delete(self, name):
        with self.changed:
            record = self.known(name)
            if record.get("check") and record["state"] not in ENDED:
                return self.dropped(name, record)
            self.runs.pop(name)
            if self.work is not None:
                self.work.send(sse("forget", {"id": name}))
            self.saved()
        shutil.rmtree(self.store.run_dir(name), ignore_errors=True)
        self.spawn(self.pull)
        return 200, {"deleted": name, "mac": self.presence()}

    def dropped(self, name, record):
        record["deleted"] = True
        record["ending"] = {"state": "failed", "reason": "its run was deleted on the mac relay"}
        record["state"], record["step"] = "writing", "reporting the check"
        if self.work is not None:
            self.work.send(sse("forget", {"id": name}))
        self.saved()
        shutil.rmtree(self.store.run_dir(name) / "outputs", ignore_errors=True)
        self.spawn(lambda: self.finish(name))
        return 200, {"deleted": name, "mac": self.presence()}

    def connect(self, body):
        body = checked(body, WORK, "the Mac's hello")
        held = set(body.get("runs", []))
        if not all(isinstance(name, str) for name in held):
            raise Refused(400, "the Mac's hello lists its runs by id, as strings")
        with self.changed:
            if self.work is not None:
                self.work.close()
            work = self.work = Work()
            self.mac = {"connected": True, "since": self.clock(), "facts": body.get("facts", {})}
            for name in sorted(held - set(self.runs)):
                work.send(sse("forget", {"id": name}))
            for name, record in self.runs.items():
                waiting = record["state"] == QUEUED
                vanished = record["state"] in WORKING and "ending" not in record and name not in held
                if waiting:
                    work.send(self.handed(record))
                if vanished:
                    record["state"] = "writing"
                    record["ending"] = {"state": "interrupted", "reason": "the Mac no longer held this run when it connected again"}
            self.saved()
        self.again(list(self.runs))
        self.follow()
        self.spawn(self.pull)
        return 200, Stream(lambda connection: self.worked(work, connection))

    def worked(self, work, connection):
        try:
            self.keep(connection)
            while True:
                events, closed = work.taken()
                yield from events
                if closed or not work.waited(connection):
                    return
        finally:
            self.left(work)
            work.release()

    def left(self, work):
        with self.changed:
            if self.work is not work:
                return
            self.work = None
            self.mac = {"connected": False, "since": self.clock(), "facts": self.mac["facts"]}
            self.changed.notify_all()

    def bye(self):
        with self.changed:
            work = self.work
        if work is not None:
            work.close()
            self.left(work)
        return 200, {"mac": self.presence()}

    def state(self, name, body):
        body = named(checked(body, STATE, "a run's state"), ["state"], "a run's state")
        state = body["state"]
        if state not in WORKING + ENDED:
            raise Refused(400, f"{state} is not a run's state; they are {', '.join(WORKING + ENDED)}")
        with self.changed:
            record = self.known(name)
            if record["state"] in ENDED:
                raise Refused(409, f"run {name} has ended")
            unwritten = "ending" in record
        if unwritten:
            self.finish(name)
            with self.changed:
                return 200, {"run": view(self.known(name))}
        with self.changed:
            record = self.known(name)
            record["started"] = record["started"] or self.clock()
            if state in ENDED:
                record["state"], record["step"] = "writing", "writing the outputs into files"
                record["ending"] = {"state": state, "reason": body.get("reason")}
                if "exit" in body:
                    record["ending"]["exit"] = body["exit"]
            else:
                record["state"], record["step"] = state, body.get("step")
                record["reason"] = body.get("reason", record["reason"])
            self.saved()
        if state in ENDED:
            self.finish(name)
        with self.changed:
            return 200, {"run": view(self.known(name))}

    def output(self, name, segments, body):
        path = output_path(segments)
        with self.changed:
            record = self.known(name)
            if record["state"] in ENDED:
                raise Refused(409, f"run {name} has ended")
        target = self.store.run_dir(name) / "outputs" / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(body)
        return 200, {"stored": path.as_posix(), "size": len(body)}

    def source(self, body):
        body = named(checked(body, SOURCE, "a source"), ["repo", "commit"], "a source")
        if not OID.fullmatch(body["commit"]):
            raise Refused(400, f"commit {body['commit']!r} is a whole commit id")
        have = [oid for oid in body.get("have", []) if isinstance(oid, str) and OID.fullmatch(oid)]
        return 200, Raw("application/x-git-bundle", self.forge.bundle(body["repo"], body["commit"], have))

    def input(self, name, index):
        with self.changed:
            record = self.known(name)
            known = index.isdigit() and int(index) < len(record["inputs"])
        if not known:
            raise Refused(404, f"run {name} has no input {index}")
        return 200, Raw("application/octet-stream", (self.store.run_dir(name) / "inputs" / index).read_bytes())

    def finish(self, name):
        with self.changed:
            record = self.runs.get(name)
            if record is None or "ending" not in record or name in self.finishing:
                return
            self.finishing.add(name)
            final = {**record, **record["ending"], "finished": self.clock(), "step": None}
        try:
            self.finished(name, final)
        finally:
            with self.changed:
                self.finishing.discard(name)

    def finished(self, name, final):
        if final.get("check"):
            return self.checked_out(name, final)
        base = f"{final['to'].rstrip('/')}/{name}"
        root = self.store.run_dir(name) / "outputs"
        found = sorted(path for path in root.rglob("*") if path.is_file()) if root.is_dir() else []
        files = [(f"{base}/{path.relative_to(root).as_posix()}", path.read_bytes()) for path in found]
        final["outputs"] = [path for path, _ in files] + [f"{base}/run.json"]
        files.append((f"{base}/run.json", (json.dumps(view(final), indent=2) + "\n").encode()))
        match self.written(name, files):
            case None:
                pass
            case ("refused", sentence):
                final["outputs"] = []
                final["reason"] = "; ".join(part for part in (final["reason"], f"its outputs were not written: {sentence}") if part)
            case ("unsent", sentence):
                with self.changed:
                    record = self.runs.get(name)
                    if record is not None and "ending" in record:
                        record["step"] = f"its outputs are not written yet ({sentence}); the next call about this run writes them again"
                        self.saved()
                return
        with self.changed:
            if name not in self.runs:
                return
            final.pop("ending")
            self.runs[name] = final
            self.saved()
        self.post(final)
        self.pull()

    def unsent(self, name, sentence):
        with self.changed:
            record = self.runs.get(name)
            if record is not None and "ending" in record:
                record["step"] = f"its check is not reported yet ({sentence}); the next call about this run reports it again"
                self.saved()

    def checked_out(self, name, final):
        root = self.store.run_dir(name) / "outputs"
        found = sorted(path for path in root.rglob("*") if path.is_file()) if root.is_dir() else []
        log_file = root / LOG
        others = [path for path in found if path != log_file]
        log, notes = None, []
        if log_file in found:
            target = f"{final['to']}/{LOG}"
            match self.written(name, [(target, log_file.read_bytes())]):
                case None:
                    log = target
                case ("refused", sentence):
                    notes.append(f"its log was not written: {sentence}")
                case ("unsent", sentence):
                    return self.unsent(name, sentence)
        if others:
            put = self.signer.put(others, OUTPUT)
            if put.code != 0:
                return self.unsent(name, f"the outputs were not put: {failed(put)}")
            blobs = put_answer(put.out.decode(), len(others))
            files = [(path.relative_to(root).as_posix(), blob, path.stat().st_size) for path, blob in zip(others, blobs)]
            outcome, reason, sentence = receipt(self.signer.submit(ACTIONS, actions_outputs(final["check"], files)))
            if outcome == "failed":
                return self.unsent(name, sentence)
            if outcome == "rejected":
                notes.append(f"its outputs were not recorded: {reason}: {sentence}")
        state = {"succeeded": "passed", "failed": "failed" if final.get("exit") is not None else "errored"}.get(final["state"], "errored")
        summary = "; ".join(part for part in (final.get("reason") or "every step passed", *notes) if part)
        unreported = self.reported_now(final["check"], state, summary, log)
        if unreported is not None:
            return self.unsent(name, unreported)
        final["outputs"] = [path.relative_to(root).as_posix() for path in found]
        with self.changed:
            if name not in self.runs:
                return
            final.pop("ending")
            if final.get("deleted"):
                self.runs.pop(name)
            else:
                self.runs[name] = final
            self.saved()
        if final.get("deleted"):
            shutil.rmtree(self.store.run_dir(name), ignore_errors=True)
        self.pull()

    def written(self, name, files):
        try:
            chunks = self.chunked(files)
        except Refused as refusal:
            return "unsent", refusal.sentence
        message = f"mac: run {name}'s outputs"
        commit = lambda base: files_commit(base, message, [(path, len(data), chunks[path]) for path, data in files])
        while True:
            head = self.signer.query(FILES_PROGRAM, files_head())
            if head.code != 0:
                return "unsent", f"files' head was not read: {failed(head)}"
            try:
                base = head_of(head.out)
            except Refused as refusal:
                return "unsent", refusal.sentence
            outcome, reason, sentence = receipt(self.signer.submit(FILES_PROGRAM, commit(base)))
            match outcome:
                case "applied":
                    return None
                case "rejected" if moved_under(outcome, reason, sentence):
                    say("mac_recommitted", reason="files_moved", detail=json.dumps(f"{reason}: {sentence}"))
                case "rejected":
                    return "refused", f"files refused the write: {reason}: {sentence}"
                case _:
                    return "unsent", f"the write was not sent: {sentence}"

    def chunked(self, files):
        pieces = [(path, data[start : start + CHUNK_SIZE]) for path, data in files for start in range(0, len(data), CHUNK_SIZE)]
        chunks = {path: [] for path, _ in files}
        if not pieces:
            return chunks
        with tempfile.TemporaryDirectory() as temp:
            paths = [Path(temp) / f"chunk-{index}" for index in range(len(pieces))]
            for path, (_, piece) in zip(paths, pieces):
                path.write_bytes(piece)
            put = self.signer.put(paths, CHUNK)
        if put.code != 0:
            raise Refused(502, f"the outputs' chunks were not put: {failed(put)}")
        for (path, _), blob in zip(pieces, put_answer(put.out.decode(), len(pieces))):
            chunks[path].append(blob)
        return chunks

    def post(self, record):
        notify = record.get("notify")
        if notify is None:
            return
        message_id = f"mac-{record['id']}-ended"
        payload = post_message(notify["channel"], message_id, record.get("requester"), ended_line(record), notify.get("thread"))
        outcome, reason, sentence = receipt(self.signer.submit(CHAT, payload))
        posted = outcome == "applied" or (outcome == "rejected" and reason == "already_exists")
        if posted:
            say("mac_posted", message=message_id)
            return
        say("mac_unposted", message=message_id, reason=reason or outcome, detail=json.dumps(sentence))

    def authorized(self, headers):
        try:
            token = self.token.read_text().strip()
        except FileNotFoundError:
            token = ""
        if not token:
            raise Refused(503, "this service holds no token for the Mac's calls")
        given = (headers.get("authorization") or "").removeprefix("Bearer ")
        if not hmac.compare_digest(given.encode(), token.encode()):
            raise Refused(401, "the Mac's calls carry its token")

    def agents(self, method, target, headers, body):
        parts = urlsplit(target)
        segments = [segment for segment in parts.path.split("/") if segment]
        query = parse_qs(parts.query, keep_blank_values=True)
        if segments[:1] != ["v1"]:
            raise Refused(404, f"{parts.path} is not a route; routes are under /v1")
        match (method, *segments[1:]):
            case ("POST", "runs"):
                return self.create(asked_by(headers), parsed(body))
            case ("GET", "runs"):
                return self.listed()
            case ("GET", "runs", name):
                return self.status(name)
            case ("GET", "runs", name, "wait"):
                return self.wait(name, query)
            case ("DELETE", "runs", name):
                return self.delete(name)
            case ("GET", "mac"):
                return 200, {"mac": self.presence()}
            case _:
                raise Refused(404, f"{method} {parts.path} is not a route")

    def macs(self, method, target, headers, body):
        self.authorized(headers)
        parts = urlsplit(target)
        segments = [segment for segment in parts.path.split("/") if segment]
        if segments[:2] != ["v1", "mac"]:
            raise Refused(404, f"{parts.path} is not a route; the Mac's routes are under /v1/mac")
        match (method, *segments[2:]):
            case ("POST", "work"):
                return self.connect(parsed(body))
            case ("POST", "bye"):
                return self.bye()
            case ("POST", "source"):
                return self.source(parsed(body))
            case ("POST", "runs", name, "state"):
                return self.state(name, parsed(body))
            case ("PUT", "runs", name, "outputs", *path):
                return self.output(name, path, body)
            case ("GET", "runs", name, "inputs", index):
                return self.input(name, index)
            case _:
                raise Refused(404, f"{method} {parts.path} is not a route")


def parsed(body):
    try:
        return json.loads(body or b"{}")
    except json.JSONDecodeError as error:
        raise Refused(400, f"the body is not JSON: {error}") from error


def handler(answer):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def respond(self, method):
            length = int(self.headers.get("content-length") or 0)
            body = self.rfile.read(length) if length else b""
            headers = {name.lower(): value for name, value in self.headers.items()}
            try:
                status, reply = answer(method, self.path, headers, body)
            except Refused as refusal:
                status, reply = refusal.status, {"error": refusal.sentence}
            except Exception as error:
                say("mac_failed", reason="unexpected", detail=json.dumps(repr(error)))
                status, reply = 500, {"error": f"the mac service failed: {error!r}"}
            match reply:
                case Stream(events=events):
                    self.stream(status, events(self.connection))
                    return
                case Raw(kind=kind, body=payload):
                    pass
                case _:
                    payload, kind = json.dumps(reply).encode(), "application/json"
            self.send_response(status)
            self.send_header("content-type", kind)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def stream(self, status, events):
            self.send_response(status)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.send_header("connection", "close")
            self.end_headers()
            try:
                for event in events:
                    self.wfile.write(event)
                    self.wfile.flush()
            except OSError:
                say("mac_stream_unheard", reason="caller_gone")
            finally:
                events.close()
                self.close_connection = True

        def do_GET(self):
            self.respond("GET")

        def do_POST(self):
            self.respond("POST")

        def do_PUT(self):
            self.respond("PUT")

        def do_DELETE(self):
            self.respond("DELETE")

        def log_message(self, form, *args):
            route = urlsplit(self.path).path
            status = args[1] if len(args) > 1 else ""
            say("mac_request", method=self.command, route=route, status=status)

    return Handler


def listening(address, answer):
    host, port = address.rsplit(":", 1)
    return ThreadingHTTPServer((host, int(port)), handler(answer))


def exit_on_signal(number, frame):
    say("mac_stopping", signal=signal.Signals(number).name)
    sys.exit(0)


def main():
    signer = Signer(shlex.split(os.environ.get("MAC_DUCKTAPE", "ducktape")), Path(os.environ.get("MAC_KEYS", "/keys")))
    root = Path(os.environ.get("MAC_STATE", "/state"))
    forge = Forge(root / "repos", os.environ["MAC_FORGE"], os.environ["MAC_NODE"])
    labels = os.environ.get("MAC_LABELS", "").split()
    service = Service(signer, Store(root), Path(os.environ.get("MAC_TOKEN", "/token/token")), forge, labels=labels)
    service.resume()
    agents = listening(os.environ.get("MAC_AGENTS_LISTEN", "0.0.0.0:8080"), service.agents)
    mac = listening(os.environ.get("MAC_MAC_LISTEN", "0.0.0.0:8081"), service.macs)
    threading.Thread(target=mac.serve_forever, daemon=True).start()
    signal.signal(signal.SIGTERM, exit_on_signal)
    say("mac_serving", agents=agents.server_address, mac=mac.server_address, keyed=signer.held())
    agents.serve_forever()


if __name__ == "__main__":
    main()
