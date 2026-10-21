#!/usr/bin/env python3
# watch: duckpond's alarms, off the chain and read-only. Each sample reads the block rate, the node's and the public /v1/status, ccs, harness's model budget, the Kubernetes API, the disks, the Deployments duckpond runs on and the run container of the warm pod /shared/tools/warm-pod names.
# It posts to chat channel `alerts` when a check turns bad and when it is good again, never waiting on a block, and hands an alert that no block carries by the next sample to the pager command `WATCH_PAGER` names.
# ops/k8s/watch/watch.yaml runs it on fabrics-1, watch-test.py tests it, and `watch.py once` prints one sample.
import json
import os
import shlex
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, replace
from pathlib import Path

SAMPLE_S = 10
RATE_WINDOW_S = 40
RATE_FLOOR = 0.7
ATTEMPTS_PER_LINE = 10
ALLOWED = "allowed"
GB = 10**9
RULES = ("free_bytes_under", "free_percent_under", "used_percent_at")
SERVICE_ACCOUNT = Path("/var/run/secrets/kubernetes.io/serviceaccount")
LEVELS = {"alert_unposted": "warn", "check_unread": "warn", "page_failed": "warn"}
OK, BAD, UNREAD = "ok", "bad", "unread"
MARKS = {BAD: "🔴", OK: "🟢"}
TURN_AFTER = 2
SHAPE_ERRORS = (AttributeError, KeyError, TypeError, ValueError)
WARM_POD_RUN = "run"


class Unread(Exception):
    pass


def say(event, **fields):
    print(json.dumps({"level": LEVELS.get(event, "info"), "event": event, **fields}, sort_keys=True), flush=True)


def worth_a_line(attempts):
    return attempts == 1 or attempts % ATTEMPTS_PER_LINE == 0


@dataclass(frozen=True)
class Reading:
    check: str
    label: str
    seen: str
    value: str
    after: int = 1


def judged(check, label, bad, value, after=1):
    return Reading(check, label, BAD if bad else OK, value, after)


def unread(check, label, why):
    return Reading(check, label, UNREAD, why)


class Http:
    def __init__(self, opener=urllib.request.urlopen):
        self.opener = opener

    def get(self, url, headers=None, context=None):
        request = urllib.request.Request(url, headers={"User-Agent": "watch", **(headers or {})})
        try:
            with self.opener(request, timeout=SAMPLE_S, context=context) as answer:
                return answer.status, answer.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()
        except OSError as error:
            raise Unread(str(error)) from error


def height_of(status):
    size = int.from_bytes(status[:4], "little")
    at = 4 + size + 24
    if len(status) < at + 8:
        raise Unread(f"/v1/status answered {len(status)} bytes, too few to hold a height")
    return int.from_bytes(status[at : at + 8], "little")


def status_reading(check, label, http, url):
    try:
        code, body = http.get(url + "/v1/status")
        if code != 200:
            return judged(check, label, True, f"answered {code}"), None
        height = height_of(body)
    except Unread as error:
        return judged(check, label, True, f"no answer: {error}"), None
    return judged(check, label, False, f"height {height}"), height


def rate_reading(samples, now_ms, height):
    label = "block rate"
    if height is None:
        return [], unread("rate", label, "the node did not answer")
    samples = [*samples, (now_ms, height)]
    old = [sample for sample in samples if now_ms - sample[0] >= RATE_WINDOW_S * 1000]
    if not old:
        return samples, unread("rate", label, f"measuring its first {RATE_WINDOW_S} s")
    since, base = old[-1]
    kept = samples[samples.index(old[-1]) :]
    seconds = (now_ms - since) / 1000
    rate = (height - base) / seconds
    value = f"{rate:.2f} blocks/s over the last {seconds:.0f} s (floor {RATE_FLOOR})"
    return kept, judged("rate", label, rate < RATE_FLOOR, value, TURN_AFTER)


def public_reading(http, url, direct):
    label = "public /v1/status"
    if direct.seen != OK:
        return unread("public", label, "the node itself did not answer")
    reading, _ = status_reading("public", label, http, url)
    return replace(reading, after=TURN_AFTER)


def ccs_reading(http, base):
    label = "ccs"
    try:
        code, _ = http.get(base + "/ccs/status")
    except Unread as error:
        return judged("ccs", label, True, f"no answer: {error}")
    return judged("ccs", label, code >= 500, f"/ccs/status answered {code}")


def limited(status):
    return status is not None and status != ALLOWED


def budget_reading(report):
    label = "model budget"
    found = []
    for account in report["accounts"]:
        name = account.get("label") or account["account"]
        if limited(account.get("status")):
            found.append(f"{name} is {account['status']}")
        for window in account["windows"]:
            if limited(window.get("status")):
                found.append(f"{name}'s {window['name']} window is {window['status']} at {window['utilization']}")
    if found:
        return judged("budget", label, True, "; ".join(found))
    return judged("budget", label, False, f"every window of {len(report['accounts'])} accounts is {ALLOWED}")


@dataclass(frozen=True)
class Disk:
    check: str
    label: str
    path: str
    rule: str
    limit: int
    clear: int | None = None

    @staticmethod
    def of(row):
        if row["rule"] not in RULES:
            raise ValueError(f"disk {row['check']}'s rule is one of {', '.join(RULES)}, not {row['rule']}")
        clear = None if row.get("clear") is None else int(row["clear"])
        disk = Disk(row["check"], row["label"], row["path"], row["rule"], int(row["limit"]), clear)
        on_the_bad_side = clear is not None and not past_on_the_good_side(disk.rule, clear, disk.limit)
        if on_the_bad_side:
            raise ValueError(f"disk {disk.check}'s clear line {clear} is not past its line {disk.limit} on the good side")
        return disk


def past_on_the_good_side(rule, clear, limit):
    return clear < limit if rule == "used_percent_at" else clear > limit


def disk_bad(rule, line, stat):
    match rule:
        case "free_bytes_under":
            return stat.f_bavail * stat.f_frsize < line
        case "free_percent_under":
            return stat.f_bavail * 100 < line * stat.f_blocks
        case "used_percent_at":
            return 100 - stat.f_bavail * 100 // stat.f_blocks >= line


def amount(rule, line):
    match rule:
        case "free_bytes_under":
            return f"{line / GB:g} GB free"
        case "free_percent_under":
            return f"{line}% free"
        case "used_percent_at":
            return f"{line}% used"


def rule_text(disk):
    bad, good = ("bad at", "good again under") if disk.rule == "used_percent_at" else ("bad under", "good again at")
    text = f"{bad} {amount(disk.rule, disk.limit)}"
    return text if disk.clear is None else f"{text}, {good} {amount(disk.rule, disk.clear)}"


def disk_line(disk, posted):
    banded = posted == BAD and disk.clear is not None
    return disk.clear if banded else disk.limit


def disk_reading(disk, stat, posted=None):
    free = stat.f_bavail * stat.f_frsize
    capacity = stat.f_blocks * stat.f_frsize
    shown = f"{free / GB:.1f} GB free of {capacity / GB:.1f} GB ({stat.f_bavail * 100 // stat.f_blocks}% free), {rule_text(disk)}"
    return judged(disk.check, disk.label, disk_bad(disk.rule, disk_line(disk, posted), stat), shown, TURN_AFTER)


def disk_read(disk, stat=os.statvfs, posted=None):
    try:
        return disk_reading(disk, stat(disk.path), posted)
    except OSError as error:
        return unread(disk.check, disk.label, f"{disk.path} does not read: {error}")


class Kube:
    def __init__(self, http, mount=SERVICE_ACCOUNT, environ=os.environ):
        self.http = http
        self.mount = mount
        self.environ = environ

    def get(self, path):
        token = (self.mount / "token").read_text().strip()
        context = ssl.create_default_context(cafile=str(self.mount / "ca.crt"))
        host = self.environ["KUBERNETES_SERVICE_HOST"]
        bracketed = f"[{host}]" if ":" in host else host
        url = f"https://{bracketed}:{self.environ['KUBERNETES_SERVICE_PORT']}{path}"
        return self.http.get(url, {"Authorization": f"Bearer {token}", "Accept": "application/json"}, context)


def api_reading(kube):
    label = "Kubernetes API"
    try:
        code, body = kube.get("/readyz")
    except Unread as error:
        return judged("kube", label, True, f"no answer: {error}")
    return judged("kube", label, code != 200, f"/readyz answered {code} {body.decode(errors='replace').strip()}")


def available(deployment):
    conditions = deployment.get("status", {}).get("conditions", [])
    held = [condition for condition in conditions if condition["type"] == "Available"]
    return held[0] if held else {"status": "Unknown", "reason": "NoAvailableCondition", "message": ""}


def deployment_reading(kube, api, name):
    check = f"deployment-{name.replace('/', '-')}"
    label = f"Deployment {name}"
    if api.seen != OK:
        return unread(check, label, "the Kubernetes API is not ready")
    namespace, deployment = name.split("/")
    try:
        code, body = kube.get(f"/apis/apps/v1/namespaces/{namespace}/deployments/{deployment}")
    except Unread as error:
        return unread(check, label, f"no answer: {error}")
    if code != 200:
        return judged(check, label, True, f"the API answered {code}")
    condition = available(json.loads(body))
    value = f"Available {condition['status']} ({condition.get('reason', '')}) {condition.get('message', '')}".strip()
    return judged(check, label, condition["status"] != "True", value)


def warm_pod_named(cli, path):
    stat = cli.query("files", {"Stat": {"at": None, "path": path}})["Stat"]
    file = (stat["entry"] or {"node": {}})["node"].get("File")
    holds_no_bytes = file is None or file["size"] == 0
    if holds_no_bytes:
        return ""
    read = {"Read": {"at": stat["snapshot"], "len": file["size"], "offset": 0, "path": path}}
    return bytes.fromhex(cli.query("files", read)["Contents"]["bytes"]).decode(errors="replace").strip()


def run_container_reading(check, label, name, pod):
    statuses = pod.get("status", {}).get("containerStatuses") or []
    runs = [status for status in statuses if status["name"] == WARM_POD_RUN]
    if not runs:
        return unread(check, label, f"{name} reports no state for its {WARM_POD_RUN} container")
    state = runs[0].get("state") or {}
    ended = state.get("terminated")
    if ended is not None:
        return judged(check, label, True, f"{name}'s {WARM_POD_RUN} container ended: {ended.get('reason', 'no reason given')}, exit {ended.get('exitCode')}")
    return judged(check, label, False, f"{name}'s {WARM_POD_RUN} container is {next(iter(state), 'in no state')}")


def warm_pod_answer_reading(check, label, path, namespace, name, code, body):
    match code:
        case 200:
            return run_container_reading(check, label, name, json.loads(body))
        case 404:
            return judged(check, label, True, f"{path} names {name}, which namespace {namespace} does not hold (404)")
        case _:
            return unread(check, label, f"the API answered {code} for pod {namespace}/{name}")


def warm_pod_reading(cli, kube, api, path, namespace):
    return replace(warm_pod_seen(cli, kube, api, path, namespace), after=TURN_AFTER)


def warm_pod_seen(cli, kube, api, path, namespace):
    check, label = "warm-pod", "warm pod"
    if api.seen != OK:
        return unread(check, label, "the Kubernetes API is not ready")
    try:
        name = warm_pod_named(cli, path)
    except Unread as error:
        return unread(check, label, f"files did not answer: {error}")
    except SHAPE_ERRORS as error:
        return unread(check, label, f"files answered {path} in another shape: {error!r}")
    if not name:
        return judged(check, label, True, f"{path} names no pod")
    try:
        code, body = kube.get(f"/api/v1/namespaces/{namespace}/pods/{urllib.parse.quote(name, safe='')}")
        return warm_pod_answer_reading(check, label, path, namespace, name, code, body)
    except Unread as error:
        return unread(check, label, f"no answer for pod {namespace}/{name}: {error}")
    except SHAPE_ERRORS as error:
        return unread(check, label, f"the API answered pod {namespace}/{name} in another shape: {error!r}")


class Cli:
    def __init__(self, ducktape, node, workspace, keys, runner=subprocess.run):
        self.ducktape = ducktape
        self.node = node
        self.workspace = workspace
        self.keys = keys
        self.runner = runner

    def env(self):
        return dict(
            os.environ,
            NO_COLOR="1",
            DUCKTAPE_DEVICE_KEY_STORE="file",
            DUCKTAPE_USER_KEY=str(self.keys / "key"),
            DUCKTAPE_PASSWORD=(self.keys / "password").read_text().strip(),
        )

    def call(self, *args):
        argv = [*self.ducktape, "--workspace", str(self.workspace), "--node", self.node, *args]
        try:
            done = self.runner(argv, capture_output=True, env=self.env(), timeout=SAMPLE_S)
        except subprocess.TimeoutExpired as expired:
            raise Unread(f"ducktape {args[0]} did not answer within {SAMPLE_S} s") from expired
        if done.returncode != 0:
            raise Unread(done.stderr.decode(errors="replace").strip() or f"ducktape exited {done.returncode}")
        return json.loads(done.stdout)

    def query(self, program, query, confirmed=False):
        return self.call("query", program, json.dumps(query), *(["--confirmed"] if confirmed else []))

    def op(self, program, op):
        return self.call("op", program, json.dumps(op))


def budget_read(cli):
    try:
        report = cli.query("harness", "Budget")["Budget"]
    except Unread as error:
        return unread("budget", "model budget", f"harness did not answer: {error}")
    return budget_reading(report)


@dataclass(frozen=True)
class Config:
    node: str
    public: str
    ccs: str
    channel: str
    disks: tuple
    deployments: tuple
    warm_pod_file: str = ""
    warm_pod_namespace: str = ""

    @staticmethod
    def from_env(env):
        return Config(
            node=env["WATCH_NODE"].rstrip("/"),
            public=env["WATCH_PUBLIC"].rstrip("/"),
            ccs=env.get("WATCH_CCS", "").rstrip("/"),
            channel=env["WATCH_CHANNEL"],
            disks=tuple(Disk.of(row) for row in json.loads(env["WATCH_DISKS"])),
            deployments=tuple(env["WATCH_DEPLOYMENTS"].split()),
            warm_pod_file=env["WATCH_WARM_POD_FILE"],
            warm_pod_namespace=env["WATCH_WARM_POD_NAMESPACE"],
        )


def fresh_state():
    return {"checks": {}, "unread": [], "streaks": {}, "outbox": []}


def message_text(reading):
    match reading.seen:
        case "bad":
            return f"{MARKS[BAD]} {reading.label}: {reading.value}"
        case "ok":
            return f"{MARKS[OK]} {reading.label} is good again: {reading.value}"


def decide(state, readings, now_ms):
    checks = dict(state["checks"])
    unread_now = set(state["unread"])
    streaks = dict(state["streaks"])
    outbox = list(state["outbox"])
    for reading in readings:
        if reading.seen == UNREAD:
            streaks.pop(reading.check, None)
            if reading.check not in unread_now:
                say("check_unread", check=reading.check, detail=reading.value)
            unread_now.add(reading.check)
            continue
        unread_now.discard(reading.check)
        held = checks.get(reading.check)
        unchanged = held is not None and held["seen"] == reading.seen
        quiet_start = held is None and reading.seen == OK
        if unchanged:
            streaks.pop(reading.check, None)
            continue
        samples = streaks.get(reading.check, 0) + 1
        waits = samples < reading.after and not quiet_start
        if waits:
            streaks[reading.check] = samples
            say("turn_held", check=reading.check, seen=reading.seen, samples=samples, value=reading.value)
            continue
        streaks.pop(reading.check, None)
        checks[reading.check] = {"seen": reading.seen, "since": now_ms, "value": reading.value}
        if quiet_start:
            continue
        say("turned", check=reading.check, seen=reading.seen, value=reading.value)
        outbox.append({"id": f"watch-{reading.check}-{reading.seen}-{now_ms}", "text": message_text(reading), "tried": 0})
    return {**state, "checks": checks, "unread": sorted(unread_now), "streaks": streaks, "outbox": outbox}


def post_op(channel, item):
    paragraph = {"Paragraph": [{"text": item["text"], "marks": []}]}
    return {"PostMessage": {"channel_id": channel, "message_id": item["id"], "blocks": [paragraph], "thread": None}}


def posted_already(cli, item):
    found = cli.query("chat", {"MessageById": {"message_id": item["id"]}}, confirmed=True)
    return found.get("Message") is not None


class Pager:
    def __init__(self, argv, runner=subprocess.run):
        self.argv = argv
        self.runner = runner

    def page(self, item):
        if not self.argv:
            say("page_unsent", id=item["id"], reason="no_pager")
            return {**item, "paged": True}
        item = {**item, "pages": item.get("pages", 0) + 1}
        done = self.runner(self.argv, input=item["text"].encode(), capture_output=True)
        if done.returncode != 0:
            if worth_a_line(item["pages"]):
                say("page_failed", id=item["id"], attempts=item["pages"], code=done.returncode, detail=done.stderr.decode(errors="replace").strip())
            return item
        say("paged", id=item["id"])
        return {**item, "paged": True}


def flush(cli, channel, outbox, pager):
    left = []
    chat_down = None
    for item in outbox:
        item = {**item, "tried": item["tried"] + 1}
        if chat_down is not None:
            if not item.get("paged"):
                item = pager.page(item)
            left.append(item)
            continue
        try:
            if posted_already(cli, item):
                say("alert_posted", id=item["id"])
                continue
            sent_before = item.get("sent", False)
            cli.op("chat", post_op(channel, item))
            item = {**item, "sent": True}
            problem = "sent, and no block carries it yet" if sent_before else None
        except Unread as error:
            problem = str(error)
            chat_down = problem
        if problem is not None:
            if worth_a_line(item["tried"]):
                say("alert_unposted", id=item["id"], attempts=item["tried"], detail=problem)
            if not item.get("paged"):
                item = pager.page(item)
        left.append(item)
    return left


class Store:
    def __init__(self, path):
        self.path = path

    def load(self):
        return {**fresh_state(), **json.loads(self.path.read_text())} if self.path.exists() else fresh_state()

    def save(self, state):
        temp = self.path.with_suffix(".tmp")
        temp.write_text(json.dumps(state, sort_keys=True))
        temp.replace(self.path)


class Watch:
    def __init__(self, config, http, kube, cli, store, pager, clock=time.time, stat=os.statvfs):
        self.config = config
        self.pager = pager
        self.http = http
        self.kube = kube
        self.cli = cli
        self.store = store
        self.clock = clock
        self.stat = stat
        self.samples = []
        self.state = store.load()

    def sample(self, now_ms):
        direct, height = status_reading("node", "node /v1/status", self.http, self.config.node)
        self.samples, rate = rate_reading(self.samples, now_ms, height)
        api = api_reading(self.kube)
        readings = [direct, rate, public_reading(self.http, self.config.public, direct), budget_read(self.cli), api]
        if self.config.ccs:
            readings.append(ccs_reading(self.http, self.config.ccs))
        readings.extend(deployment_reading(self.kube, api, name) for name in self.config.deployments)
        if self.config.warm_pod_file:
            readings.append(warm_pod_reading(self.cli, self.kube, api, self.config.warm_pod_file, self.config.warm_pod_namespace))
        readings.extend(disk_read(disk, self.stat, self.state["checks"].get(disk.check, {}).get("seen")) for disk in self.config.disks)
        return readings

    def tick(self):
        now_ms = int(self.clock() * 1000)
        self.state = decide(self.state, self.sample(now_ms), now_ms)
        self.store.save(self.state)
        self.state = {**self.state, "outbox": flush(self.cli, self.config.channel, self.state["outbox"], self.pager)}
        self.store.save(self.state)

    def run(self, pause=time.sleep):
        say("watching", channel=self.config.channel, deployments=list(self.config.deployments), warm_pod=self.config.warm_pod_file, disks=[disk.check for disk in self.config.disks])
        while True:
            began = self.clock()
            self.tick()
            pause(max(0.0, SAMPLE_S - (self.clock() - began)))


def parts(env):
    config = Config.from_env(env)
    http = Http()
    workspace = Path(env.get("WATCH_WORKSPACE") or tempfile.mkdtemp())
    workspace.mkdir(parents=True, exist_ok=True)
    cli = Cli(shlex.split(env.get("WATCH_DUCKTAPE", "ducktape")), config.node, workspace, Path(env.get("WATCH_KEYS", "/keys")))
    return config, http, Kube(http), cli, Pager(shlex.split(env.get("WATCH_PAGER", "")))


def main():
    env = os.environ
    config, http, kube, cli, pager = parts(env)
    store = Store(Path(env.get("WATCH_STATE", "/state")) / "state.json")
    match sys.argv[1:]:
        case []:
            Watch(config, http, kube, cli, store, pager).run()
        case ["once"]:
            readings = Watch(config, http, kube, cli, store, pager).sample(int(time.time() * 1000))
            print(json.dumps([reading.__dict__ for reading in readings], indent=2))
        case other:
            sys.exit(f"usage: watch.py [once], not {' '.join(other)}")


if __name__ == "__main__":
    main()
