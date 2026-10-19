#!/usr/bin/env python3
# The mac service's Mac side: dials out to the relay (ops/k8s/mac/relay.py), pulls agents' runs, builds each forge commit with the app's phone/Makefile, runs its UI script in the iOS simulator (the app's AX door, simctl and the XCUITest driver) and sends back states and outputs.
# A LaunchAgent (com.ducktape.mac.plist) keeps it running; power.swift tells it when macOS sleeps, wakes and changes network. mac-test.py tests it on Linux against the real relay, git, and a fake xcrun, make and xcodebuild.
import http.client
import json
import os
import platform
import plistlib
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
import urllib.error
import urllib.request
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, quote, urlsplit

HERE = Path(__file__).resolve().parent
WORKING = ("accepted", "building", "testing", "writing")
ENDED = ("succeeded", "failed", "interrupted", "stopped")
DOOR_FILE = Path("Library/Caches/ducktape/ax-door.json")
CRASHES = Path.home() / "Library/Logs/DiagnosticReports"
FRESH = "ducktape-run-"
DRIVER = "TEST_RUNNER_DUCKTAPE_DRIVER"
RELAY_DOWN_PAUSE = 5
KEEPALIVE = (10, 5, 3)


class Failed(Exception):
    def __init__(self, sentence, seen=None):
        super().__init__(sentence)
        self.sentence = sentence
        self.seen = seen


class Stopped(Exception):
    pass


class Unreached(Exception):
    pass


@dataclass
class Ran:
    code: int
    out: str
    err: str


def say(event, **fields):
    words = " ".join(f"{name}={value}" for name, value in fields.items())
    print(f"event={event} {words}".strip(), file=sys.stderr, flush=True)


def why(ran):
    return ran.err or ran.out.strip() or f"exited {ran.code}"


def written(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    fresh = path.with_name(path.name + ".new")
    fresh.write_bytes(data)
    os.replace(fresh, path)


def loaded(folder):
    try:
        return json.loads((folder / "state.json").read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def described(name, value):
    match name:
        case "press" | "type" | "scroll":
            find = value.get("find") or {}
            target = value.get("id") or find.get("label") or find.get("text") or find.get("id", "")
            return f"{name} {target}".rstrip()
        case "wait":
            return "wait " + " ".join(str(value[field]) for field in ("id", "role", "label", "text", "in") if field in value)
        case "system":
            return f"system tap {value.get('tap', '')}"
        case "media":
            return f"media {value.get('name', '')}"
        case "tree" | "screenshot":
            return f"{name} {value}"
        case "changed":
            return f"changed {value.get('name', '')}"
        case _:
            return name


def door_of(door_file):
    try:
        door = json.loads(door_file.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return None
    whole = isinstance(door, dict) and isinstance(door.get("port"), int) and isinstance(door.get("token"), str)
    return door if whole else None


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def kept_alive(sock):
    idle, interval, count = KEEPALIVE
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
    idle_option = getattr(socket, "TCP_KEEPIDLE", None) or socket.TCP_KEEPALIVE
    for option, value in ((idle_option, idle), (socket.TCP_KEEPINTVL, interval), (socket.TCP_KEEPCNT, count)):
        sock.setsockopt(socket.IPPROTO_TCP, option, value)


class Kqueue:
    def __init__(self, door_file, pid):
        self.door_file = door_file
        self.pid = pid
        self.fds = []
        self.queue = None

    def __enter__(self):
        if not hasattr(select, "kqueue"):
            raise Failed("this host has no kqueue to watch the app's launch with")
        self.queue = select.kqueue()
        folder = next(parent for parent in [self.door_file.parent, *self.door_file.parents] if parent.is_dir())
        watched = [folder, self.door_file] if self.door_file.is_file() else [folder]
        changes = []
        for path in watched:
            fd = os.open(path, os.O_RDONLY)
            self.fds.append(fd)
            flags = select.KQ_NOTE_WRITE | select.KQ_NOTE_EXTEND
            changes.append(select.kevent(fd, select.KQ_FILTER_VNODE, select.KQ_EV_ADD | select.KQ_EV_CLEAR, flags))
        changes.append(select.kevent(self.pid, select.KQ_FILTER_PROC, select.KQ_EV_ADD, select.KQ_NOTE_EXIT))
        try:
            self.queue.control(changes, 0, 0)
        except ProcessLookupError:
            pass
        return self.wait

    def wait(self):
        self.queue.control(None, 1, None)

    def __exit__(self, *exception):
        for fd in self.fds:
            os.close(fd)
        self.queue.close()


def events_of(answer):
    kind, data = None, None
    while line := answer.readline():
        text = line.decode(errors="replace").rstrip("\r\n")
        if text.startswith("event: "):
            kind = text.removeprefix("event: ")
        elif text.startswith("data: "):
            data = json.loads(text.removeprefix("data: "))
        elif text == "" and kind is not None:
            yield kind, data
            kind, data = None, None


class Relay:
    def __init__(self, base, token_file):
        parts = urlsplit(base)
        self.secure = parts.scheme == "https"
        self.host = parts.hostname
        self.port = parts.port
        self.prefix = parts.path.rstrip("/")
        self.token_file = token_file
        self.keep = kept_alive

    def connection(self):
        kind = http.client.HTTPSConnection if self.secure else http.client.HTTPConnection
        return kind(self.host, self.port)

    def headers(self, body):
        headers = {"authorization": f"Bearer {self.token_file.read_text().strip()}"}
        if isinstance(body, dict):
            headers["content-type"] = "application/json"
        return headers

    def call(self, method, path, body=None):
        payload = json.dumps(body).encode() if isinstance(body, dict) else body
        connection = self.connection()
        try:
            connection.request(method, self.prefix + path, body=payload, headers=self.headers(body))
            answer = connection.getresponse()
            return answer.status, answer.read()
        except OSError as error:
            raise Unreached(f"the relay was not reached: {error}") from error
        finally:
            connection.close()

    def work(self, hello):
        connection = self.connection()
        try:
            connection.connect()
            sock = connection.sock
            self.keep(sock)
            connection.request("POST", self.prefix + "/v1/mac/work", body=json.dumps(hello).encode(), headers=self.headers(hello))
            answer = connection.getresponse()
        except OSError as error:
            connection.close()
            raise Unreached(f"the relay was not reached: {error}") from error
        if answer.status != 200:
            said = answer.read().decode(errors="replace")
            connection.close()
            raise Unreached(f"the relay refused the work stream: {answer.status} {said}")
        return connection, sock, answer


class Sources:
    def __init__(self, root, relay):
        self.root = root
        self.relay = relay
        self.locks = {}
        self.lock = threading.Lock()

    def lock_of(self, repo):
        with self.lock:
            return self.locks.setdefault(repo, threading.Lock())

    def mirror(self, repo):
        return self.root / f"{repo}.git"

    def worktree(self, job, repo, commit, target, log):
        mirror = self.mirror(repo)
        inside = ["git", "-C", str(mirror)]
        with self.lock_of(repo):
            if not mirror.exists():
                self.root.mkdir(parents=True, exist_ok=True)
                job.must(["git", "init", "--quiet", "--bare", str(mirror)], log, f"no repository for {repo}")
            held = subprocess.run([*inside, "cat-file", "-e", f"{commit}^{{commit}}"], capture_output=True).returncode == 0
            if not held:
                self.fetched(job, repo, commit, mirror, log)
            job.must([*inside, "worktree", "add", "--detach", str(target), commit], log, f"no worktree of {repo} at {commit}")

    def fetched(self, job, repo, commit, mirror, log):
        tips = subprocess.run(["git", "-C", str(mirror), "for-each-ref", "--format=%(objectname)"], capture_output=True)
        have = sorted(set(tips.stdout.decode().split()))
        try:
            status, data = self.relay.call("POST", "/v1/mac/source", {"repo": repo, "commit": commit, "have": have})
        except Unreached as error:
            raise Failed(f"{repo}'s source at {commit} was not fetched: {error}") from error
        if status != 200:
            raise Failed(f"the relay gave no source of {repo} at {commit}: {status} {data.decode(errors='replace')}")
        with tempfile.TemporaryDirectory() as temp:
            bundle = Path(temp) / "source.bundle"
            bundle.write_bytes(data)
            ref = f"refs/mac/{commit}"
            job.must(["git", "-C", str(mirror), "fetch", "--quiet", str(bundle), f"{ref}:{ref}"], log, f"{repo}'s bundle at {commit} was not taken")

    def drop(self, repo, target):
        inside = ["git", "-C", str(self.mirror(repo))]
        with self.lock_of(repo):
            subprocess.run([*inside, "worktree", "remove", "--force", str(target)], capture_output=True)
            subprocess.run([*inside, "worktree", "prune"], capture_output=True)


class Driver:
    def __init__(self, job, xctestrun, udid, log):
        self.changed = threading.Condition()
        self.commands = []
        self.results = {}
        self.ended = None
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        url = f"http://127.0.0.1:{self.server.server_address[1]}"
        argv = ["xcodebuild", "test-without-building", "-xctestrun", str(xctestrun), "-destination", f"id={udid}"]
        self.process = job.started(argv, log, env=dict(os.environ, **{DRIVER: url}))
        threading.Thread(target=self.watched, daemon=True).start()

    def watched(self):
        code = self.process.wait()
        with self.changed:
            self.ended = code
            self.changed.notify_all()

    def handler(self):
        driver = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def answer(self, status, body):
                payload = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_GET(self):
                parts = urlsplit(self.path)
                after = parse_qs(parts.query).get("after", ["0"])[-1]
                if parts.path != "/next" or not after.isdigit():
                    return self.answer(404, {"error": f"{self.path} is not a route"})
                self.answer(200, driver.next(int(after)))

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                done = json.loads(self.rfile.read(length) or b"{}")
                if self.path != "/done" or not isinstance(done.get("seq"), int):
                    return self.answer(404, {"error": f"{self.path} is not a route"})
                driver.done(done)
                self.answer(200, {})

            def log_message(self, form, *args):
                pass

        return Handler

    def next(self, after):
        with self.changed:
            self.changed.wait_for(lambda: len(self.commands) > after)
            return {"seq": after + 1, "command": self.commands[after]}

    def done(self, result):
        with self.changed:
            self.results[result["seq"]] = result
            self.changed.notify_all()

    def tap(self, command):
        with self.changed:
            self.commands.append(command)
            seq = len(self.commands)
            self.changed.notify_all()
            self.changed.wait_for(lambda: seq in self.results or self.ended is not None)
            result = self.results.get(seq)
        if result is None:
            raise Failed(f"the XCUITest driver ended with exit code {self.ended} before it answered; see driver.log")
        if not result.get("ok"):
            raise Failed(result.get("error") or "the XCUITest driver said no")
        return f"tapped {command['tap']}"

    def end(self):
        with self.changed:
            self.commands.append({"end": True})
            self.changed.notify_all()
        self.process.wait()
        self.server.shutdown()
        self.server.server_close()


class Simulator:
    def __init__(self, job, app, bundle):
        self.job = job
        self.app = app
        self.bundle = bundle
        self.udid = None
        self.fresh = False
        self.container = None
        self.door = None
        self.driver = None
        self.held = False
        self.frame = None

    def simctl(self, *args):
        ran = self.job.xcrun("simctl", *args)
        if ran.code != 0:
            raise Failed(f"xcrun simctl {args[0]} failed: {why(ran)}")
        return ran.out

    def prepare(self, wanted):
        devices = self.job.mac.devices()
        kept = next((device for device in devices if device["name"] == self.job.mac.device), None)
        if wanted.get("fresh"):
            kind = wanted.get("type") or (kept or {}).get("deviceTypeIdentifier")
            runtime = wanted.get("runtime") or (kept or {}).get("runtime")
            if not kind or not runtime:
                raise Failed(f"a fresh device names its type and runtime, since the Mac has no device {self.job.mac.device} to copy them from")
            self.udid = self.simctl("create", FRESH + self.job.name, kind, runtime).strip()
            self.fresh = True
        elif kept is None:
            raise Failed(f"the Mac has no simulator device named {self.job.mac.device}; the operator makes it once and signs the app in on it")
        else:
            self.job.hold()
            self.held = True
            self.udid = kept["udid"]
        self.simctl("bootstatus", self.udid, "-b")
        self.simctl("install", self.udid, str(self.app))
        self.container = Path(self.simctl("get_app_container", self.udid, self.bundle, "data").strip())

    def step(self, number, name, value):
        out = self.job.out
        match name:
            case "launch":
                return self.launch()
            case "terminate":
                self.simctl("terminate", self.udid, self.bundle)
                self.door = None
                return "terminated"
            case "wait":
                return self.door_call("POST", "/inspect/wait", value).decode(errors="replace")
            case "press" | "type" | "scroll":
                return self.aimed(name, value)
            case "tree":
                target = out / f"{number:02d}-{value}.json"
                written(target, self.door_call("GET", "/inspect/tree"))
                return f"saved {target.name}"
            case "screenshot":
                said = self.screenshot(out / f"{number:02d}-{value}.png")
                self.frame = self.framed()
                return said
            case "changed":
                return self.changed(out / f"{number:02d}-{value['name']}.png", value["deadline_ms"])
            case "media":
                return self.media(value)
            case "system":
                return self.system(value)

    def screenshot(self, target):
        target.parent.mkdir(parents=True, exist_ok=True)
        self.simctl("io", self.udid, "screenshot", str(target))
        return f"saved {target.name}"

    def framed(self):
        with tempfile.TemporaryDirectory() as temp:
            frame = Path(temp) / "frame.bmp"
            self.simctl("io", self.udid, "screenshot", "--type=bmp", str(frame))
            return frame.read_bytes()

    def changed(self, target, deadline_ms):
        if self.frame is None:
            raise Failed("there is no screen to compare with: a screenshot step comes first")
        until = time.monotonic() + deadline_ms / 1000
        seen, frames = self.frame, 0
        while True:
            frame = self.framed()
            frames += 1
            moved = frame != self.frame
            still = frame == seen
            if moved and still:
                self.frame = frame
                self.screenshot(target)
                return f"the screen changed and held still after {frames} frames; saved {target.name}"
            if time.monotonic() >= until:
                state = "kept changing" if moved else "did not change"
                raise Failed(f"the screen {state} within {deadline_ms} ms ({frames} frames)")
            seen = frame

    def launch(self):
        door_file = self.container / DOOR_FILE
        door_file.unlink(missing_ok=True)
        said = self.simctl("launch", self.udid, self.bundle).strip()
        pid = int(said.rsplit(":", 1)[-1])
        while True:
            with self.job.mac.watcher(door_file, pid) as wait:
                door = door_of(door_file)
                if door is not None:
                    break
                if not alive(pid):
                    raise Failed("the app's process ended before it opened its AX door; see crashes/ and device.log")
                wait()
        self.door = door
        return f"launched, pid {pid}, AX door on port {door['port']}"

    def aimed(self, name, value):
        if "find" not in value:
            return self.door_call("POST", f"/inspect/{name}", value).decode(errors="replace")
        found = json.loads(self.door_call("POST", "/inspect/wait", value["find"]))
        node = found.get("node") if isinstance(found, dict) else None
        if not isinstance(node, dict) or "id" not in node:
            raise Failed(f"the inspector's wait for {json.dumps(value['find'])} answered no node with an id: {json.dumps(found)}")
        asked = {**{field: given for field, given in value.items() if field != "find"}, "id": node["id"]}
        said = self.door_call("POST", f"/inspect/{name}", asked).decode(errors="replace")
        return f"found {node['id']}; {said}"

    def door_call(self, method, path, body=None):
        if self.door is None:
            raise Failed("no app is running with its AX door: launch it first")
        payload = None if body is None else json.dumps(body).encode()
        headers = {"authorization": f"Bearer {self.door['token']}", "content-type": "application/json"}
        request = urllib.request.Request(f"http://127.0.0.1:{self.door['port']}{path}", data=payload, method=method, headers=headers)
        try:
            with urllib.request.urlopen(request) as answer:
                return answer.read()
        except urllib.error.HTTPError as refusal:
            said = refusal.read().decode(errors="replace")
            try:
                body = json.loads(said)
            except json.JSONDecodeError:
                body = None
            told = body if isinstance(body, dict) else {}
            seen = told.get("tree")
            sentence = told.get("error", said)
            raise Failed(f"the door answered {refusal.code}: {sentence}", None if seen is None else json.dumps(seen).encode()) from refusal
        except (urllib.error.URLError, OSError) as error:
            raise Failed(f"the AX door was not reached: {getattr(error, 'reason', error)}") from error

    def media(self, value):
        status, data = self.job.mac.relay.call("GET", f"/v1/mac/runs/{self.job.name}/inputs/{value['input']}")
        if status != 200:
            raise Failed(f"the relay did not give input {value['input']}: {status} {data.decode(errors='replace')}")
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / value["name"]
            file.write_bytes(data)
            self.simctl("addmedia", self.udid, str(file))
        return f"added {value['name']} to Photos"

    def system(self, value):
        if self.driver is None:
            xctestrun = self.job.record.get("driver")
            if not xctestrun:
                raise Failed("the build has no XCUITest driver (make -C phone ios-driver)")
            self.driver = Driver(self.job, xctestrun, self.udid, self.job.out / "driver.log")
        return self.driver.tap({"tap": value["tap"], "app": value.get("app", self.bundle), "deadline_ms": value["deadline_ms"]})

    def collect(self, process, since):
        if self.udid is None:
            return
        start = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(since))
        ran = self.job.xcrun("simctl", "spawn", self.udid, "log", "show", "--start", start, "--predicate", f'process == "{process}"', "--style", "compact")
        written(self.job.out / "device.log", (ran.out if ran.code == 0 else f"log show failed: {why(ran)}\n").encode())
        crashes = self.job.mac.crashes
        reports = sorted(crashes.glob(f"{process}*.ips")) if crashes.is_dir() else []
        for report in reports:
            if report.stat().st_mtime >= since:
                written(self.job.out / "crashes" / report.name, report.read_bytes())

    def close(self):
        try:
            if self.driver is not None:
                self.driver.end()
            if self.fresh:
                self.job.xcrun("simctl", "shutdown", self.udid)
                self.job.xcrun("simctl", "delete", self.udid)
        finally:
            if self.held:
                self.job.release()


class Job:
    def __init__(self, mac, folder, record):
        self.mac = mac
        self.folder = folder
        self.record = record
        self.name = record["run"]["id"]
        self.out = folder / "out"
        self.stopped = False
        self.processes = set()
        self.lock = threading.Lock()

    def save(self, **fields):
        self.record.update(fields)
        written(self.folder / "state.json", json.dumps(self.record).encode())

    def check(self):
        if self.stopped:
            raise Stopped()

    def report(self, state, step=None):
        self.check()
        self.save(state=state, step=step)
        self.mac.tell(self.name, {"state": state, "step": step})

    def hold(self):
        mac = self.mac
        with mac.kept:
            holder = mac.holder
        if holder is not None:
            self.report("testing", f"waiting for the kept device {mac.device}, which run {holder} holds")
        with mac.kept:
            mac.kept.wait_for(lambda: mac.holder is None or self.stopped)
            self.check()
            mac.holder = self.name

    def release(self):
        mac = self.mac
        with mac.kept:
            if mac.holder == self.name:
                mac.holder = None
                mac.kept.notify_all()

    def stop(self):
        self.stopped = True
        with self.mac.kept:
            self.mac.kept.notify_all()
        with self.lock:
            processes = list(self.processes)
        for process in processes:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def started(self, argv, log, cwd=None, env=None):
        self.check()
        log.parent.mkdir(parents=True, exist_ok=True)
        with open(log, "ab") as sink:
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=sink, stderr=subprocess.STDOUT, start_new_session=True)
        with self.lock:
            self.processes.add(process)
        return process

    def command(self, argv, log, cwd=None, env=None):
        self.check()
        log.parent.mkdir(parents=True, exist_ok=True)
        last = ""
        with open(log, "ab") as sink:
            sink.write(f"$ {shlex.join(argv)}\n".encode())
            sink.flush()
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
            with self.lock:
                self.processes.add(process)
            with process.stdout:
                for line in process.stdout:
                    sink.write(line)
                    last = line.decode(errors="replace").strip() or last
            code = process.wait()
        with self.lock:
            self.processes.discard(process)
        self.check()
        return code, last

    def must(self, argv, log, doing):
        code, last = self.command(argv, log)
        if code != 0:
            raise Failed(f"{doing}: {last or f'exited {code}'}")

    def xcrun(self, *args):
        self.check()
        done = subprocess.run(["xcrun", *args], capture_output=True)
        return Ran(done.returncode, done.stdout.decode(errors="replace"), done.stderr.decode(errors="replace").strip())

    def needs_driver(self):
        return any("system" in step for step in self.record.get("script") or [])

    def make(self, target, log):
        run = self.record["run"]
        src = Path(self.record["src"])
        variables = [f"{name}={value}" for name, value in sorted((run.get("vars") or {}).items())]
        argv = ["make", "-C", "phone", target, *variables]
        self.report("building", shlex.join(argv))
        env = dict(os.environ, CARGO_TARGET_DIR=str(self.mac.home / "target" / self.record["repo"]))
        code, last = self.command(argv, log, cwd=src, env=env)
        if code != 0:
            raise Failed(f"{shlex.join(argv)} exited {code}; see build.log")
        product = Path(last)
        return str(product if product.is_absolute() else src / product)

    def build(self):
        run = self.record["run"]
        log = self.out / "build.log"
        if run.get("from"):
            built = loaded(self.mac.folder(run["from"]))
            if built is None or not built.get("product"):
                raise Failed(f"run {run['from']}'s build is not on the Mac: it never built, or was deleted")
            self.save(product=built["product"], src=built["src"], repo=built["repo"], driver=built.get("driver"))
        else:
            src = self.folder / "src"
            self.report("building", f"fetching {run['repo']} at {run['commit']}")
            self.mac.sources.worktree(self, run["repo"], run["commit"], src, log)
            self.save(src=str(src), repo=run["repo"])
            self.save(product=self.make(run["build"], log))
        if self.needs_driver() and not self.record.get("driver"):
            self.save(driver=self.make("ios-driver", log))

    def test(self):
        script = self.record["script"]
        app = Path(self.record["product"])
        plist = app / "Info.plist"
        if app.suffix != ".app" or not plist.is_file():
            raise Failed(f"the build's product {app} is not a simulator .app, so the script has nothing to run")
        info = plistlib.loads(plist.read_bytes())
        self.report("testing", "preparing the simulator")
        since = time.time()
        simulator = Simulator(self, app, info["CFBundleIdentifier"])
        log = self.out / "script.log"
        try:
            simulator.prepare(self.record["run"].get("device") or {})
            for number, step in enumerate(script, 1):
                ((name, value),) = step.items()
                doing = described(name, value)
                self.report("testing", f"step {number}/{len(script)}: {doing}")
                try:
                    said = simulator.step(number, name, value)
                except Failed as failure:
                    self.logged(log, f"{number:02d} {doing}: failed: {failure.sentence}")
                    self.shot(simulator, number)
                    if failure.seen is not None:
                        written(self.out / f"{number:02d}-failed.json", failure.seen)
                    raise Failed(f"step {number} ({doing}): {failure.sentence}") from failure
                self.logged(log, f"{number:02d} {doing}: {said}")
        finally:
            self.quietly(lambda: simulator.collect(info["CFBundleExecutable"], since))
            self.quietly(simulator.close)

    def shot(self, simulator, number):
        if simulator.udid is None:
            return
        self.quietly(lambda: simulator.screenshot(self.out / f"{number:02d}-failed.png"))

    def quietly(self, act):
        try:
            act()
        except (Failed, OSError) as error:
            say("mac_unkept", run=self.name, detail=json.dumps(str(error)))

    def logged(self, log, line):
        log.parent.mkdir(parents=True, exist_ok=True)
        with open(log, "a") as sink:
            sink.write(line.rstrip("\n") + "\n")

    def work(self):
        ending = None
        try:
            self.build()
            if self.record.get("script"):
                self.test()
            ending = ("succeeded", None)
        except Stopped:
            ending = None
        except Failed as failure:
            ending = ("failed", failure.sentence)
        except Exception as error:
            say("mac_failed", run=self.name, detail=json.dumps(repr(error)))
            ending = ("failed", f"the Mac's side failed: {error!r}")
        with self.mac.lock:
            self.mac.jobs.pop(self.name, None)
        if ending is None or self.stopped:
            self.mac.remove(self.name)
            return
        self.save(state=ending[0], reason=ending[1], step=None, reported=False)
        self.mac.deliver(self.folder)


class Mac:
    def __init__(self, relay, home, device, crashes=CRASHES, watcher=Kqueue):
        self.relay = relay
        self.home = home
        self.sources = Sources(home / "repos", relay)
        self.device = device
        self.crashes = crashes
        self.watcher = watcher
        self.jobs = {}
        self.lock = threading.Lock()
        self.kept = threading.Condition()
        self.holder = None
        self.awake = threading.Event()
        self.awake.set()
        self.stream = None
        self.nudged = threading.Event()

    def folder(self, name):
        return self.home / "runs" / name

    def folders(self):
        root = self.home / "runs"
        return sorted(folder for folder in root.iterdir() if (folder / "state.json").is_file()) if root.is_dir() else []

    def held(self):
        return [folder.name for folder in self.folders()]

    def xcrun(self, *args):
        done = subprocess.run(["xcrun", *args], capture_output=True)
        return Ran(done.returncode, done.stdout.decode(errors="replace"), done.stderr.decode(errors="replace").strip())

    def devices(self):
        ran = self.xcrun("simctl", "list", "devices", "--json")
        if ran.code != 0:
            raise Failed(f"xcrun simctl list failed: {why(ran)}")
        listed = json.loads(ran.out).get("devices", {})
        return [{**device, "runtime": runtime} for runtime, devices in listed.items() for device in devices]

    def facts(self):
        facts = {"macos": platform.mac_ver()[0] or platform.platform()}
        xcode = subprocess.run(["xcodebuild", "-version"], capture_output=True)
        facts["xcode"] = " ".join(xcode.stdout.decode(errors="replace").split("\n")[:1]).strip() or "unknown"
        try:
            kept = next((device for device in self.devices() if device["name"] == self.device), None)
        except Failed as failure:
            kept = {"error": failure.sentence}
        facts["device"] = None if kept is None else {field: kept.get(field) for field in ("name", "udid", "deviceTypeIdentifier", "runtime", "error")}
        return facts

    def recover(self):
        for folder in self.folders():
            record = loaded(folder)
            if record is not None and record.get("state") in WORKING:
                record.update(state="interrupted", reason="the mac service restarted while it ran", step=None, reported=False)
                written(folder / "state.json", json.dumps(record).encode())
        try:
            leftovers = [device for device in self.devices() if device["name"].startswith(FRESH)]
        except Failed as failure:
            say("mac_unswept", detail=json.dumps(failure.sentence))
            return
        for device in leftovers:
            self.xcrun("simctl", "shutdown", device["udid"])
            self.xcrun("simctl", "delete", device["udid"])

    def serve(self):
        self.recover()
        while True:
            self.awake.wait()
            self.nudged.clear()
            try:
                self.session()
            except Unreached as error:
                say("mac_unconnected", detail=json.dumps(str(error)))
                self.nudged.wait(RELAY_DOWN_PAUSE)

    def session(self):
        connection, sock, answer = self.relay.work({"facts": self.facts(), "runs": self.held()})
        with self.lock:
            self.stream = sock
        say("mac_connected")
        threading.Thread(target=self.sync, daemon=True).start()
        try:
            for kind, data in events_of(answer):
                match kind:
                    case "run":
                        self.take(data)
                    case "forget":
                        self.forget(data["id"])
        except OSError as error:
            say("mac_stream_ended", detail=json.dumps(str(error)))
        finally:
            with self.lock:
                self.stream = None
            connection.close()

    def take(self, data):
        name = data["run"]["id"]
        folder = self.folder(name)
        known = loaded(folder)
        if known is not None:
            if known.get("state") in WORKING:
                self.tell(name, {"state": known["state"], "step": known.get("step")})
            return
        record = {"run": data["run"], "script": data.get("script"), "inputs": data.get("inputs", []), "state": "accepted", "reported": False}
        job = Job(self, folder, record)
        job.save()
        with self.lock:
            self.jobs[name] = job
        self.tell(name, {"state": "accepted"})
        threading.Thread(target=job.work, daemon=True).start()

    def forget(self, name):
        with self.lock:
            job = self.jobs.get(name)
        if job is not None:
            job.stop()
            return
        self.remove(name)

    def remove(self, name):
        folder = self.folder(name)
        record = loaded(folder) or {}
        own = folder / "src"
        if own.exists() and record.get("repo"):
            self.sources.drop(record["repo"], own)
        shutil.rmtree(folder, ignore_errors=True)
        say("mac_forgot", run=name)

    def tell(self, name, body):
        sent = {field: value for field, value in body.items() if value is not None}
        try:
            status, _ = self.relay.call("POST", f"/v1/mac/runs/{name}/state", sent)
        except Unreached as error:
            say("mac_untold", run=name, detail=json.dumps(str(error)))
            return
        if status == 404:
            self.forget(name)

    def deliver(self, folder):
        record = loaded(folder)
        name = folder.name
        out = folder / "out"
        files = sorted(path for path in out.rglob("*") if path.is_file()) if out.is_dir() else []
        try:
            for path in files:
                target = quote(path.relative_to(out).as_posix())
                status, _ = self.relay.call("PUT", f"/v1/mac/runs/{name}/outputs/{target}", path.read_bytes())
                if status != 200:
                    return self.delivered(folder, status)
            ending = {field: record[field] for field in ("state", "reason") if record.get(field) is not None}
            status, _ = self.relay.call("POST", f"/v1/mac/runs/{name}/state", ending)
        except Unreached as error:
            say("mac_undelivered", run=name, detail=json.dumps(str(error)))
            return
        self.delivered(folder, status)

    def delivered(self, folder, status):
        match status:
            case 200 | 409:
                record = loaded(folder)
                record["reported"] = True
                written(folder / "state.json", json.dumps(record).encode())
            case 404:
                self.remove(folder.name)
            case _:
                say("mac_undelivered", run=folder.name, status=status)

    def sync(self):
        for folder in self.folders():
            record = loaded(folder)
            unreported = record is not None and record.get("state") in ENDED and not record.get("reported")
            if unreported:
                self.deliver(folder)

    def will_sleep(self):
        self.awake.clear()
        try:
            self.relay.call("POST", "/v1/mac/bye")
        except Unreached as error:
            say("mac_unsaid", detail=json.dumps(str(error)))
        with self.lock:
            stream = self.stream
        if stream is not None:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def woke(self):
        self.awake.set()
        self.nudged.set()

    def network_changed(self):
        self.nudged.set()

    def hear_power(self, argv):
        with subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as process:
            for line in process.stdout:
                match line.strip():
                    case "will-sleep":
                        self.will_sleep()
                        process.stdin.write("ok\n")
                        process.stdin.flush()
                    case "did-wake":
                        self.woke()
                    case "network-changed":
                        self.network_changed()
        say("mac_power_unheard", code=process.returncode)


def main():
    home = Path(os.environ.get("MAC_HOME") or Path.home() / "Library/Application Support/ducktape-mac")
    relay = Relay(os.environ["MAC_RELAY"], Path(os.environ.get("MAC_TOKEN_FILE") or home / "token"))
    crashes = Path(os.environ.get("MAC_CRASHES") or CRASHES)
    mac = Mac(relay, home, os.environ.get("MAC_DEVICE", "ducktape-j089"), crashes=crashes)
    power = shlex.split(os.environ.get("MAC_POWER") or shlex.join(["/usr/bin/swift", str(HERE / "power.swift")]))
    threading.Thread(target=mac.hear_power, args=(power,), daemon=True).start()
    say("mac_serving", relay=os.environ["MAC_RELAY"], home=str(home))
    mac.serve()


if __name__ == "__main__":
    main()
