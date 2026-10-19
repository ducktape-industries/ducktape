"""Exercise the Mac side against the real relay over HTTP, real git, and a fake xcrun, make and xcodebuild with a fake app's AX door; run by `make test`."""
import http.client
import importlib.util
import json
import os
import pathlib
import plistlib
import signal
import socket
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = pathlib.Path(__file__).resolve().parent


def loaded(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


relay = loaded("relay", HERE.parent / "k8s" / "mac" / "relay.py")
mac = loaded("mac", HERE / "mac.py")
relay.say = lambda event, **fields: None
HEARD = threading.Condition()
SAID = []


def heard(event, **fields):
    with HEARD:
        SAID.append((event, fields))
        HEARD.notify_all()


mac.say = heard

KEPT = {
    "name": "ducktape-j089",
    "udid": "KEPT-1",
    "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17",
    "runtime": "com.apple.CoreSimulator.SimRuntime.iOS-26-5",
    "state": "Booted",
    "isAvailable": True,
}
TOKEN = "s3cret"
TO = "/shared/runs"

XCRUN = r'''#!/usr/bin/env python3
import json, os, pathlib, plistlib, signal, subprocess, sys, uuid
root = pathlib.Path(os.environ["FAKE_SIM"])
state = json.loads((root / "state.json").read_text())
with open(root / "calls.jsonl", "a") as calls:
    calls.write(json.dumps(["xcrun", *sys.argv[1:]]) + "\n")
def save():
    (root / "state.json").write_text(json.dumps(state))
def device(udid):
    return next(d for d in state["devices"] if d["udid"] == udid)
match sys.argv[2:]:
    case ["list", "devices", "--json"]:
        grouped = {}
        for d in state["devices"]:
            grouped.setdefault(d["runtime"], []).append({k: v for k, v in d.items() if k != "runtime"})
        print(json.dumps({"devices": grouped}))
    case ["create", name, kind, runtime]:
        udid = str(uuid.uuid4()).upper()
        state["devices"].append({"name": name, "udid": udid, "deviceTypeIdentifier": kind, "runtime": runtime, "state": "Shutdown", "isAvailable": True})
        save()
        print(udid)
    case ["bootstatus", udid, "-b"]:
        device(udid)["state"] = "Booted"
        save()
    case ["install", udid, app]:
        info = plistlib.loads((pathlib.Path(app) / "Info.plist").read_bytes())
        (root / "containers" / udid / info["CFBundleIdentifier"] / "Library" / "Caches").mkdir(parents=True, exist_ok=True)
    case ["get_app_container", udid, bundle, "data"]:
        print(root / "containers" / udid / bundle)
    case ["launch", udid, bundle]:
        app = subprocess.Popen([sys.executable, str(root / "app.py"), str(root / "containers" / udid / bundle), state["mode"]], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        app.stdout.readline()
        if state["mode"] == "crash":
            app.wait()
        state["pids"].append(app.pid)
        save()
        print(f"{bundle}: {app.pid}")
    case ["terminate", udid, bundle]:
        for pid in state["pids"]:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    case ["io", udid, "screenshot", path]:
        pathlib.Path(path).write_bytes(b"\x89PNG fake screenshot of " + udid.encode())
    case ["addmedia", udid, *files]:
        state["media"] += [pathlib.Path(f).name + ":" + pathlib.Path(f).read_text() for f in files]
        save()
    case ["spawn", udid, "log", "show", *rest]:
        print("fake log " + " ".join(rest))
    case ["shutdown", udid]:
        device(udid)["state"] = "Shutdown"
        save()
    case ["delete", udid]:
        state["devices"] = [d for d in state["devices"] if d["udid"] != udid]
        save()
    case other:
        print(f"fake xcrun knows no {other}", file=sys.stderr)
        sys.exit(64)
'''

MAKE = r'''#!/usr/bin/env python3
import json, os, pathlib, plistlib, sys
root = pathlib.Path(os.environ["FAKE_SIM"])
with open(root / "calls.jsonl", "a") as calls:
    calls.write(json.dumps(["make", *sys.argv[1:], "CARGO_TARGET_DIR=" + os.environ.get("CARGO_TARGET_DIR", "")]) + "\n")
products = pathlib.Path.cwd() / "phone" / "ios" / "build" / "derived" / "Build" / "Products"
match sys.argv[3]:
    case "ios-sim":
        app = products / "Debug-iphonesimulator" / "Ducktape.app"
        app.mkdir(parents=True, exist_ok=True)
        (app / "Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": "industries.ducktape.mobile", "CFBundleExecutable": "Ducktape"}))
        print("compiling ducktape-phone", flush=True)
        print(app)
    case "ios-driver":
        products.mkdir(parents=True, exist_ok=True)
        (products / "Driver.xctestrun").write_text("{}")
        print(products / "Driver.xctestrun")
    case target:
        print(f"make: *** No rule to make target '{target}'.  Stop.", file=sys.stderr)
        sys.exit(2)
'''

XCODEBUILD = r'''#!/usr/bin/env python3
import json, os, pathlib, sys, urllib.request
if sys.argv[1:] == ["-version"]:
    print("Xcode 26.5\nBuild version 17F00")
    sys.exit(0)
root = pathlib.Path(os.environ["FAKE_SIM"])
url = os.environ["TEST_RUNNER_DUCKTAPE_DRIVER"]
after = 0
while True:
    asked = json.load(urllib.request.urlopen(f"{url}/next?after={after}"))
    command = asked["command"]
    if command.get("end"):
        sys.exit(0)
    with open(root / "calls.jsonl", "a") as calls:
        calls.write(json.dumps(["tap", command["tap"], command["app"]]) + "\n")
    ok = command["tap"] in ("Allow", "Photo")
    done = {"seq": asked["seq"], "ok": ok}
    if not ok:
        done["error"] = f"no element labelled {command['tap']} appeared in {command['deadline_ms']} ms"
    request = urllib.request.Request(f"{url}/done", data=json.dumps(done).encode(), method="POST")
    urllib.request.urlopen(request).read()
    after = asked["seq"]
'''

APP = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
container, mode = pathlib.Path(sys.argv[1]), sys.argv[2]
if mode == "crash":
    (pathlib.Path(os.environ["FAKE_CRASHES"]) / "Ducktape-2026-10-19-120000.ips").write_text('{"exception": "EXC_BAD_ACCESS"}')
    sys.exit(3)
class Door(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def reply(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
    def do_GET(self):
        if self.headers.get("authorization") != "Bearer t0k":
            return self.reply(401, {"error": "the door's token is required"})
        self.reply(200, {"nodes": [{"id": "phone:chat/attach", "role": "Button", "name": "Attach"}]})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if self.path == "/wait" and body.get("name") == "hold":
            open(pathlib.Path(os.environ["FAKE_SIM"]) / "hold").close()
            return self.reply(200, {"node": {"id": "phone:hold", "name": "hold"}})
        if self.path == "/wait" and body.get("name") == "missing":
            return self.reply(408, {"error": f"nothing matched within {body['deadline_ms']} ms"})
        if self.path == "/wait":
            return self.reply(200, {"node": {"id": "phone:chat/" + body["name"].lower(), "role": body.get("role"), "name": body["name"]}})
        self.reply(200, {"did": self.path, "with": body})
    def log_message(self, form, *args):
        pass
server = ThreadingHTTPServer(("127.0.0.1", 0), Door)
door = container / "Library" / "Caches" / "ducktape"
door.mkdir(parents=True, exist_ok=True)
(door / "ax-door.json").write_text(json.dumps({"port": server.server_address[1], "token": "t0k"}))
print("ready", flush=True)
server.serve_forever()
'''


class Signer:
    def __init__(self):
        self.files = {}
        self.puts = []
        self.submits = []

    def held(self):
        return True

    def put(self, paths, kind):
        self.puts.append([path.read_bytes() for path in paths])
        return relay.Ran(0, "".join(f"sha256:{index:064x}\n" for index in range(len(paths))).encode(), "")

    def submit(self, program, payload):
        self.submits.append((program, payload))
        return relay.Ran(0, b"preconfirmed applied 0a", "")

    def query(self, program, request):
        if request[0] == 0:
            return relay.Ran(0, relay.u8(0) + relay.u8(1) + relay.u8(0) + b"\xab" * 32, "")
        reader = relay.Reader(request)
        reader.u8()
        data = self.files[reader.bytes().decode()]
        reply = relay.u8(5) + relay.u8(0) + b"\xcd" * 32 + relay.u64(len(data)) + relay.u64(0) + relay.borsh_bytes(data) + relay.u8(1)
        return relay.Ran(0, reply, "")


class Nudge(threading.Event):
    def __init__(self):
        super().__init__()
        self.waits = []

    def wait(self, timeout=None):
        with HEARD:
            self.waits.append(timeout)
            HEARD.notify_all()
        return super().wait()


class Undecided:
    def __init__(self, door_file, pid):
        pass

    def __enter__(self):
        return self.wait

    def wait(self):
        raise AssertionError("the launch was not decided at its first look")

    def __exit__(self, *exception):
        return False


class Mac(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = root = pathlib.Path(self.temp.name)
        self.sim = root / "sim"
        self.sim.mkdir()
        self.crashes = root / "crashes"
        self.crashes.mkdir()
        self.state({"devices": [KEPT], "pids": [], "media": [], "mode": "ok"})
        (self.sim / "app.py").write_text(APP)
        bin = root / "bin"
        bin.mkdir()
        for name, text in (("xcrun", XCRUN), ("make", MAKE), ("xcodebuild", XCODEBUILD)):
            (bin / name).write_text(text)
            (bin / name).chmod(0o755)
        self.environ = dict(os.environ)
        os.environ.update(PATH=f"{bin}:{os.environ['PATH']}", FAKE_SIM=str(self.sim), FAKE_CRASHES=str(self.crashes))
        self.commit = self.forge()
        (root / "token").write_text(TOKEN)
        self.signer = Signer()
        forge = relay.Forge(root / "relay" / "repos", f"file://{root}/forge", "https://node.example")
        self.service = relay.Service(self.signer, relay.Store(root / "relay"), root / "token", forge)
        self.servers = [relay.listening("127.0.0.1:0", answer) for answer in (self.service.agents, self.service.macs)]
        for server in self.servers:
            threading.Thread(target=server.serve_forever, daemon=True).start()
        client = mac.Relay(f"http://127.0.0.1:{self.servers[1].server_address[1]}", root / "token")
        self.mac = mac.Mac(client, root / "mac", "ducktape-j089", crashes=self.crashes, watcher=Undecided)
        self.request = 40

    def tearDown(self):
        self.mac.will_sleep()
        for pid in self.sim_state()["pids"]:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for server in self.servers:
            server.shutdown()
            server.server_close()
        os.environ.clear()
        os.environ.update(self.environ)
        self.temp.cleanup()

    def state(self, state):
        (self.sim / "state.json").write_text(json.dumps(state))

    def sim_state(self):
        return json.loads((self.sim / "state.json").read_text())

    def calls(self):
        lines = (self.sim / "calls.jsonl").read_text().splitlines() if (self.sim / "calls.jsonl").exists() else []
        return [json.loads(line) for line in lines]

    def forge(self):
        work = self.root / "work"
        work.mkdir()
        git = lambda *args: subprocess.run(["git", "-C", str(work), *args], check=True, capture_output=True).stdout.decode().strip()
        git("init", "--quiet", "--initial-branch=dev")
        (work / "README.md").write_text("app\n")
        git("add", "README.md")
        git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "app")
        (self.root / "forge").mkdir()
        subprocess.run(["git", "clone", "--quiet", "--bare", str(work), str(self.root / "forge" / "app")], check=True)
        return git("rev-parse", "HEAD")

    def start(self):
        threading.Thread(target=self.mac.serve, daemon=True).start()
        self.until(lambda: self.service.mac["connected"])

    def until(self, holds):
        with self.service.changed:
            self.service.changed.wait_for(holds)

    def ask(self, body):
        self.request += 1
        connection = http.client.HTTPConnection(*self.servers[0].server_address)
        headers = {"ducktape-chain": "duckpond", "ducktape-program": "harness", "ducktape-request": str(self.request)}
        connection.request("POST", "/v1/runs", body=json.dumps(body).encode(), headers=headers)
        answer = connection.getresponse()
        said = json.loads(answer.read())
        connection.close()
        self.assertEqual(answer.status, 201, said)
        return said["run"]["id"]

    def ended(self, name):
        self.until(lambda: self.service.runs.get(name, {}).get("state") in relay.ENDED)
        return relay.view(self.service.runs[name])

    def build(self, **fields):
        return {"repo": "app", "commit": self.commit, "build": "ios-sim", "to": TO, **fields}

    def outputs(self, run):
        return [path.removeprefix(f"{TO}/{run['id']}/") for path in run["outputs"]]

    def put(self, wanted):
        return [piece for pieces in self.signer.puts for piece in pieces if wanted in piece]

    def test_a_build_and_its_script_land_every_output_in_files(self):
        self.signer.files["/shared/cat.png"] = b"cat"
        script = [
            {"launch": {}},
            {"system": {"tap": "Allow", "app": "com.apple.springboard", "deadline_ms": 5000}},
            {"wait": {"role": "Button", "name": "Attach", "deadline_ms": 5000}},
            {"act": {"find": {"role": "Button", "name": "Attach", "deadline_ms": 5000}, "action": "click"}},
            {"media": {"files": {"path": "/shared/cat.png"}}},
            {"system": {"tap": "Photo", "deadline_ms": 5000}},
            {"tree": "chat"},
            {"screenshot": "chat"},
        ]
        self.start()
        name = self.ask(self.build(vars={"FEATURES": "ax-door"}, script={"steps": script}))
        run = self.ended(name)
        self.assertEqual((run["state"], run["reason"]), ("succeeded", None))
        expected = ["07-chat.json", "08-chat.png", "build.log", "device.log", "driver.log", "script.log", "run.json"]
        self.assertEqual(self.outputs(run), expected)
        makes = [call for call in self.calls() if call[0] == "make"]
        target = f"CARGO_TARGET_DIR={self.root}/mac/target/app"
        self.assertEqual(makes, [["make", "-C", "phone", "ios-sim", "FEATURES=ax-door", target], ["make", "-C", "phone", "ios-driver", "FEATURES=ax-door", target]])
        self.assertIn(["tap", "Allow", "com.apple.springboard"], self.calls())
        self.assertIn(["tap", "Photo", "industries.ducktape.mobile"], self.calls())
        self.assertEqual(self.sim_state()["media"], ["cat.png:cat"])
        log = self.put(b"01 launch: launched")[0].decode()
        self.assertIn("02 system tap Allow: tapped Allow", log)
        self.assertIn('03 wait Button Attach: {"node": {"id": "phone:chat/attach"', log)
        self.assertIn('04 act click Attach: found phone:chat/attach; {"did": "/act", "with": {"action": "click", "id": "phone:chat/attach"}}', log)
        self.assertIn("08 screenshot chat: saved 08-chat.png", log)
        self.assertEqual(self.put(b"fake screenshot"), [b"\x89PNG fake screenshot of KEPT-1"])
        self.assertIn(b"phone:chat/attach", self.put(b"nodes")[0])
        self.assertIn(b'--predicate process == "Ducktape"', self.put(b"fake log")[0])

        again = self.ask({"from": name, "to": TO, "script": {"steps": [{"launch": {}}, {"screenshot": "again"}]}})
        run = self.ended(again)
        self.assertEqual(run["state"], "succeeded")
        self.assertEqual(len([call for call in self.calls() if call[0] == "make"]), 2)

    def test_a_failing_step_stops_the_script_and_shows_the_screen(self):
        self.start()
        script = [{"launch": {}}, {"wait": {"name": "missing", "deadline_ms": 10}}, {"screenshot": "never"}]
        run = self.ended(self.ask(self.build(script={"steps": script})))
        self.assertEqual(run["state"], "failed")
        self.assertEqual(run["reason"], "step 2 (wait missing): the door answered 408: nothing matched within 10 ms")
        self.assertIn("02-failed.png", self.outputs(run))
        self.assertNotIn("03-never.png", self.outputs(run))

    def test_a_crash_at_launch_fails_the_run_with_its_crash_report(self):
        state = self.sim_state()
        state["mode"] = "crash"
        self.state(state)
        self.start()
        run = self.ended(self.ask(self.build(script={"steps": [{"launch": {}}]})))
        self.assertEqual(run["reason"], "step 1 (launch): the app's process ended before it opened its AX door; see crashes/ and device.log")
        self.assertIn("crashes/Ducktape-2026-10-19-120000.ips", self.outputs(run))

    def test_a_failed_build_says_so_and_keeps_its_log(self):
        self.start()
        run = self.ended(self.ask(self.build(build="android", script={"steps": [{"launch": {}}]})))
        self.assertEqual((run["state"], run["reason"]), ("failed", "make -C phone android exited 2; see build.log"))
        self.assertEqual(self.outputs(run), ["build.log", "run.json"])
        self.assertIn(b"No rule to make target 'android'", self.put(b"No rule")[0])

    def test_a_script_without_system_steps_builds_no_driver_and_a_system_step_with_no_element_fails(self):
        self.start()
        run = self.ended(self.ask(self.build(script={"steps": [{"launch": {}}]})))
        self.assertEqual(run["state"], "succeeded")
        self.assertNotIn("ios-driver", json.dumps(self.calls()))
        steps = [{"launch": {}}, {"system": {"tap": "Nope", "deadline_ms": 7}}]
        run = self.ended(self.ask({"from": run["id"], "to": TO, "script": {"steps": steps}}))
        self.assertEqual(run["reason"], "step 2 (system tap Nope): no element labelled Nope appeared in 7 ms")

    def test_a_fresh_device_copies_the_kept_one_and_goes_when_the_run_ends(self):
        self.start()
        run = self.ended(self.ask(self.build(device={"fresh": True}, script={"steps": [{"launch": {}}]})))
        self.assertEqual(run["state"], "succeeded")
        created = next(call for call in self.calls() if call[2] == "create")
        self.assertEqual(created[3:], [f"ducktape-run-{run['id']}", KEPT["deviceTypeIdentifier"], KEPT["runtime"]])
        self.assertEqual([device["udid"] for device in self.sim_state()["devices"]], ["KEPT-1"])

    def test_a_run_left_working_is_interrupted_when_the_mac_starts_again(self):
        name = self.ask(self.build())
        folder = self.root / "mac" / "runs" / name
        folder.mkdir(parents=True)
        (folder / "state.json").write_text(json.dumps({"run": {"id": name}, "state": "building", "reported": False}))
        self.service.state(name, {"state": "building"})
        self.start()
        run = self.ended(name)
        self.assertEqual((run["state"], run["reason"]), ("interrupted", "the mac service restarted while it ran"))

    def test_the_mac_says_goodbye_before_it_sleeps_and_takes_queued_runs_when_it_wakes(self):
        self.start()
        self.mac.will_sleep()
        self.until(lambda: not self.service.mac["connected"])
        name = self.ask(self.build())
        self.assertEqual(self.service.runs[name]["state"], "queued")
        self.mac.woke()
        self.assertEqual(self.ended(name)["state"], "succeeded")

    def test_a_deleted_run_is_forgotten_on_the_mac(self):
        self.start()
        name = self.ask(self.build())
        self.ended(name)
        folder = self.root / "mac" / "runs" / name
        self.assertTrue((folder / "src" / "README.md").is_file())
        connection = http.client.HTTPConnection(*self.servers[0].server_address)
        connection.request("DELETE", f"/v1/runs/{name}")
        self.assertEqual(connection.getresponse().status, 200)
        connection.close()
        with HEARD:
            HEARD.wait_for(lambda: ("mac_forgot", {"run": name}) in SAID)
        self.assertFalse(folder.exists())
        worktrees = subprocess.run(["git", "-C", str(self.root / "mac" / "repos" / "app.git"), "worktree", "list"], capture_output=True).stdout.decode()
        self.assertNotIn(name, worktrees)

    def test_runs_on_the_kept_device_take_it_in_turn(self):
        hold = self.sim / "hold"
        os.mkfifo(hold)
        self.start()
        steps = [{"launch": {}}, {"wait": {"name": "hold", "deadline_ms": 1}}, {"screenshot": "first"}]
        first = self.ask(self.build(script={"steps": steps}))
        self.until(lambda: self.service.runs[first]["step"] == "step 2/3: wait hold")
        second = self.ask(self.build(script={"steps": [{"launch": {}}, {"screenshot": "second"}]}))
        waiting = f"waiting for the kept device ducktape-j089, which run {first} holds"
        self.until(lambda: self.service.runs[second]["step"] == waiting)
        installs = lambda: [index for index, call in enumerate(self.calls()) if call[2:3] == ["install"]]
        self.assertEqual(len(installs()), 1)
        with open(hold, "w"):
            pass
        self.assertEqual(self.ended(first)["state"], "succeeded")
        self.assertEqual(self.ended(second)["state"], "succeeded")
        calls = self.calls()
        shot = next(index for index, call in enumerate(calls) if call[2:4] == ["io", "KEPT-1"] and first in call[-1])
        self.assertEqual(len(installs()), 2)
        self.assertLess(shot, installs()[1])
        self.assertIn(second, calls[installs()[1]][-1])

    def test_a_work_stream_gone_silent_is_dropped_and_dialled_again(self):
        heard = threading.Condition()
        works = []
        released = threading.Event()

        class Silent(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):
                self.rfile.read(int(self.headers.get("content-length") or 0))
                self.send_response(200)
                if self.path != "/v1/mac/work":
                    self.send_header("content-length", "0")
                    self.end_headers()
                    return
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                self.wfile.flush()
                with heard:
                    works.append(self.path)
                    heard.notify_all()
                released.wait()

            def log_message(self, form, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Silent)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        client = mac.Relay(f"http://127.0.0.1:{server.server_address[1]}", self.root / "token")
        kept = []

        def keep(sock):
            kept.append(sock)
            mac.kept_alive(sock)

        client.keep = keep
        other = mac.Mac(client, self.root / "other", "ducktape-j089", crashes=self.crashes, watcher=Undecided)
        threading.Thread(target=other.serve, daemon=True).start()
        with heard:
            heard.wait_for(lambda: len(works) >= 1)
        kept[0].shutdown(socket.SHUT_RDWR)
        with heard:
            heard.wait_for(lambda: len(works) >= 2)
        self.assertEqual(kept[1].getsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE), 1)
        self.assertEqual(kept[1].getsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPCNT), 3)
        other.will_sleep()
        released.set()
        server.shutdown()
        server.server_close()

    def test_a_relay_not_reached_is_tried_again_on_a_network_change_on_wake_and_after_the_fixed_pause(self):
        dead = socket.socket()
        dead.bind(("127.0.0.1", 0))
        client = mac.Relay(f"http://127.0.0.1:{dead.getsockname()[1]}", self.root / "token")
        other = mac.Mac(client, self.root / "other", "ducktape-j089", crashes=self.crashes, watcher=Undecided)
        other.nudged = Nudge()
        threading.Thread(target=other.serve, daemon=True).start()
        with HEARD:
            HEARD.wait_for(lambda: len(other.nudged.waits) == 1)
        other.network_changed()
        with HEARD:
            HEARD.wait_for(lambda: len(other.nudged.waits) == 2)
        self.assertEqual(other.nudged.waits, [5, 5])
        client.port = self.servers[1].server_address[1]
        other.woke()
        self.until(lambda: self.service.mac["connected"])
        other.will_sleep()
        dead.close()

    def test_power_lines_say_goodbye_before_sleep_and_nudge_on_wake_and_network_change(self):
        self.start()
        self.mac.nudged.clear()
        self.mac.hear_power(["sh", "-c", "echo will-sleep; read answer; [ \"$answer\" = ok ] && echo network-changed"])
        self.until(lambda: not self.service.mac["connected"])
        self.assertFalse(self.mac.awake.is_set())
        self.assertTrue(self.mac.nudged.is_set())
        self.mac.hear_power(["sh", "-c", "echo did-wake"])
        self.assertTrue(self.mac.awake.is_set())
        self.until(lambda: self.service.mac["connected"])


if __name__ == "__main__":
    unittest.main()
