#!/usr/bin/env python3
# watch: duckpond's alarms, off the chain and read-only. Each sample reads the block rate, the node's and the public /v1/status, ccs, harness's model budget, the Kubernetes API, the disks, the Deployments duckpond runs on, the run container of the warm pod /shared/tools/warm-pod names and GitHub's newest finished run of each release WATCH_RELEASES names; once a UTC day it reads what each app release channel WATCH_CHANGELOG names has published.
# It posts to chat channel `alerts` when a check turns bad and when it is good again, never waiting on a block, and hands an alert that no block carries by the next sample to the pager command `WATCH_PAGER` names; a channel that published posts one entry of the release notes merged since its last one to the changelog's channel.
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
GITHUB = "https://api.github.com"
DECIDED = ("success", "failure")
NOTE = "Release note:"
UPLOAD_PACK = "/info/refs?service=git-upload-pack"


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
        code, _, body = self.answer(url, headers, context)
        return code, body

    def answer(self, url, headers=None, context=None):
        request = urllib.request.Request(url, headers={"User-Agent": "watch", **(headers or {})})
        try:
            with self.opener(request, timeout=SAMPLE_S, context=context) as answer:
                return answer.status, lowered(answer.headers), answer.read()
        except urllib.error.HTTPError as error:
            return error.code, lowered(error.headers), error.read()
        except OSError as error:
            raise Unread(str(error)) from error


def lowered(headers):
    return {name.lower(): value for name, value in (headers or {}).items()}


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
    stat = cli.query("files", {"stat": {"at": None, "path": path}})["stat"]
    file = (stat["entry"] or {"node": {}})["node"].get("file")
    holds_no_bytes = file is None or file["size"] == 0
    if holds_no_bytes:
        return ""
    read = {"read": {"at": stat["snapshot"], "len": file["size"], "offset": 0, "path": path}}
    return bytes.fromhex(cli.query("files", read)["contents"]["bytes"]).decode(errors="replace").strip()


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


@dataclass(frozen=True)
class Release:
    check: str
    label: str
    repo: str
    workflow: str
    runs: tuple

    @staticmethod
    def of(row):
        return Release(row["check"], row["label"], row["repo"], row["workflow"], tuple(tuple(sorted(runs.items())) for runs in row["runs"]))

    def urls(self):
        base = f"{GITHUB}/repos/{self.repo}/actions/workflows/{self.workflow}/runs"
        return [f"{base}?{urllib.parse.urlencode({**dict(runs), 'status': 'completed'})}" for runs in self.runs]


def github_said(body):
    try:
        return json.loads(body).get("message", "")
    except SHAPE_ERRORS:
        return body.decode(errors="replace").strip()


def release_reading(release, answers):
    runs = []
    try:
        for code, body in answers:
            if code != 200:
                return unread(release.check, release.label, f"GitHub answered {code}: {github_said(body)}")
            runs.extend(json.loads(body)["workflow_runs"])
        decided = sorted((run for run in runs if run["conclusion"] in DECIDED), key=lambda run: run["created_at"], reverse=True)
        if not decided:
            return unread(release.check, release.label, f"none of GitHub's {len(runs)} newest finished {release.workflow} runs passed or failed")
        run = decided[0]
        failed = run["conclusion"] == "failure"
        value = f"{release.workflow}'s {run['event']} run on {run['head_branch']} at {run['head_sha'][:8]} {'failed' if failed else 'passed'}: {run['html_url']}"
    except SHAPE_ERRORS as error:
        return unread(release.check, release.label, f"GitHub answered in another shape: {error!r}")
    return judged(release.check, release.label, failed, value)


def next_read_ms(now_ms, headers, calls):
    try:
        remaining = int(headers["x-ratelimit-remaining"])
        reset_ms = int(headers["x-ratelimit-reset"]) * 1000
    except (KeyError, ValueError):
        return now_ms
    left_ms = max(reset_ms - now_ms, 0)
    if remaining < calls:
        return now_ms + left_ms
    return now_ms + left_ms * calls // remaining


def releases_read(http, releases, now_ms):
    readings, headers = [], {}
    for release in releases:
        answers = []
        try:
            for url in release.urls():
                code, headers, body = http.answer(url, {"Accept": "application/vnd.github+json"})
                answers.append((code, body))
            readings.append(release_reading(release, answers))
        except Unread as error:
            readings.append(unread(release.check, release.label, f"GitHub did not answer: {error}"))
    return readings, next_read_ms(now_ms, headers, sum(len(release.runs) for release in releases))


@dataclass(frozen=True)
class Channel:
    check: str
    label: str
    tags: str
    emoji: str
    branch: str
    manifest: str = ""
    refs: str = ""
    prefix: str = ""
    since: str = ""
    since_sequence: int = 0

    @staticmethod
    def of(row):
        fields = (row.get(name, "") for name in ("manifest", "refs", "prefix", "since"))
        channel = Channel(row["check"], row["label"], row["tags"], row["emoji"], row["branch"], *fields, int(row.get("since_sequence", 0)))
        names_one = bool(channel.manifest) != bool(channel.refs)
        if not names_one:
            raise ValueError(f"changelog channel {channel.check} names one of manifest and refs")
        return channel

    def seeded(self):
        return {"sequence": self.since_sequence, "commit": self.since, "read": ""} if self.since else None


@dataclass(frozen=True)
class Changelog:
    channel: str
    repo: str
    channels: tuple

    @staticmethod
    def of(row):
        return Changelog(row["channel"], row["repo"], tuple(Channel.of(each) for each in row["channels"]))


@dataclass(frozen=True)
class Published:
    sequence: int
    commit: str
    version: str
    at: str


def answered(http, url):
    code, body = http.get(url)
    if code != 200:
        raise Unread(f"{url} answered {code}")
    return body


def manifest_published(http, url):
    manifest = json.loads(json.loads(answered(http, url))["manifest"])
    display = manifest["release"]["display"]
    commit = display.rpartition("+")[2]
    named = bool(commit) and not commit.strip("0123456789abcdef")
    if not named:
        raise Unread(f"{url} names no commit in {display!r}")
    return Published(int(manifest["sequence"]), commit, display, manifest["published_at"])


def advertised(body):
    refs, at = {}, 0
    while at + 4 <= len(body):
        size = int(body[at : at + 4], 16)
        line = body[at + 4 : at + size].split(b"\0")[0].rstrip(b"\n")
        at += max(size, 4)
        sha, _, name = line.decode(errors="replace").partition(" ")
        if len(sha) == 40:
            refs[name.removesuffix("^{}")] = sha
    return refs


def tags_published(http, channel):
    marked = f"refs/tags/{channel.prefix}"
    refs = advertised(answered(http, channel.refs + UPLOAD_PACK))
    builds = [(int(name[len(marked) :]), sha) for name, sha in refs.items() if name.startswith(marked) and name[len(marked) :].isdigit()]
    if not builds:
        raise Unread(f"{channel.refs} holds no {marked}<build>")
    build, sha = max(builds)
    return Published(build, sha, f"build {build}", "")


def published_by(http, channel):
    return manifest_published(http, channel.manifest) if channel.manifest else tags_published(http, channel)


def log_query(repo, branch, since, after):
    exclude = None if since is None else {"oid": since}
    return {"log": {"repo": repo, "from": {"ref": f"refs/heads/{branch}".encode().hex()}, "exclude": exclude, "page": {"after": after, "limit": None}}}


def commits_to(cli, repo, branch, commit, since):
    found, after = [], None
    while True:
        page = cli.query("forge", log_query(repo, branch, since, after))["log"]["page"]
        for row in page["items"]:
            if found or row["oid"].startswith(commit):
                found.append((row["oid"], bytes.fromhex(row["message"]).decode(errors="replace")))
        after = page["next"]
        settled = after is None or (since is None and found)
        if settled:
            break
    if not found:
        raise Unread(f"{repo}'s {branch} holds no {commit} past {since}")
    return found


def change_of(subject):
    head, mark, tail = subject.rpartition(" (change ")
    number = tail.removesuffix(")")
    numbered = bool(mark) and tail.endswith(")") and number.isdigit()
    return (head, number) if numbered else (subject, None)


def note_of(message):
    subject, *body = message.splitlines() or [""]
    title, number = change_of(subject)
    said = next((line[len(NOTE) :].strip() for line in body if line.startswith(NOTE)), "") or title
    return number, None if said.rstrip(".").lower() == "none" else said


def digest(changelog, channel, published, commits, since):
    notes = [(oid, *note_of(message)) for oid, message in commits]
    said = [note for _, _, note in notes if note is not None]
    if not said:
        return []
    root = f"release-{channel.check}-{commits[0][0][:12]}"
    seen = f"published {published.at}" if published.at else f"uploaded as {published.version}"
    text = [f"{channel.tags} {channel.emoji} **{channel.label} {published.version}: what's new**", *(f"• {note}" for note in said), f"*{seen} · rollback: the previous build · posted by watch*"]
    named = ", ".join(number or oid[:8] for oid, number, note in notes if note is not None)
    quiet = ", ".join(number or oid[:8] for oid, number, note in notes if note is None)
    details = [
        "**Deploy details**",
        f"• Channel: {channel.label}, {published.version}, sequence {published.sequence}",
        f"• Source: {changelog.repo} {channel.branch} `{commits[0][0]}`",
        f"• Since: `{since}`",
        f"• Changes: {named}",
        *([f"• Left out, release note none: {quiet}"] if quiet else []),
    ]
    item = {"channel": changelog.channel, "markdown": True, "paged": True, "tried": 0}
    return [{**item, "id": root, "text": "\n".join(text)}, {**item, "id": f"{root}-details", "under": root, "text": "\n".join(details)}]


def channel_read(http, cli, repo, channel, held):
    published = published_by(http, channel)
    moved = held is None or published.sequence > held["sequence"]
    since = None if held is None else held["commit"]
    return published, commits_to(cli, repo, channel.branch, published.commit, since) if moved else []


def changelog_failed(failing, channel, why):
    if channel.check not in failing:
        say("changelog_unread", channel=channel.check, detail=why)
    return failing | {channel.check}


def changelog_step(state, changelog, http, cli, now_ms):
    day = time.strftime("%Y-%m-%d", time.gmtime(now_ms / 1000))
    kept = dict(state["changelog"])
    failing = set(state["changelog_unread"])
    posts = list(state["posts"])
    for channel in changelog.channels:
        held = kept.get(channel.check) or channel.seeded()
        if held is not None and held["read"] == day:
            continue
        try:
            published, commits = channel_read(http, cli, changelog.repo, channel, held)
        except Unread as error:
            failing = changelog_failed(failing, channel, str(error))
            continue
        except SHAPE_ERRORS as error:
            failing = changelog_failed(failing, channel, f"answered in another shape: {error!r}")
            continue
        failing.discard(channel.check)
        entry = digest(changelog, channel, published, commits, held["commit"]) if held is not None and commits else []
        posts.extend(entry)
        top = commits[0][0] if commits else held["commit"]
        kept[channel.check] = {"sequence": published.sequence, "commit": top, "read": day}
        say("changelog_read", channel=channel.check, sequence=published.sequence, commit=top, posts=len(entry))
    return {**state, "changelog": kept, "changelog_unread": sorted(failing), "posts": posts}


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
        report = cli.query("harness", "budget_facts")["budget_facts"]["budget"]
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
    releases: tuple = ()
    changelog: object = None

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
            releases=tuple(Release.of(row) for row in json.loads(env.get("WATCH_RELEASES", "[]"))),
            changelog=Changelog.of(json.loads(env["WATCH_CHANGELOG"])) if env.get("WATCH_CHANGELOG") else None,
        )


def fresh_state():
    return {"checks": {}, "unread": [], "streaks": {}, "outbox": [], "posts": [], "changelog": {}, "changelog_unread": []}


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
    paragraph = {"paragraph": [{"text": item["text"], "marks": []}]}
    return {"post_message": {"channel_id": channel, "message_id": item["id"], "blocks": [paragraph], "thread": None}}


def post_of(channel, item, thread):
    if not item.get("markdown"):
        return post_op(channel, item)
    return {"post": {"channel_id": item["channel"], "message_id": item["id"], "text": item["text"], "thread": thread}}


def thread_of(cli, item):
    if "under" not in item:
        return True, None
    found = cli.query("chat", {"message_by_id": {"message_id": item["under"]}}, confirmed=True).get("message")
    return found is not None, None if found is None else found["seq"]


def posted_already(cli, item):
    found = cli.query("chat", {"message_by_id": {"message_id": item["id"]}}, confirmed=True)
    return found.get("message") is not None


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
            ready, thread = thread_of(cli, item)
            if not ready:
                left.append(item)
                continue
            cli.op("chat", post_of(channel, item, thread))
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
        self.released = ([], 0)
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
        readings.extend(self.release_readings(now_ms))
        return readings

    def release_readings(self, now_ms):
        due_ms = self.released[1]
        if self.config.releases and now_ms >= due_ms:
            self.released = releases_read(self.http, self.config.releases, now_ms)
        return self.released[0]

    def tick(self):
        now_ms = int(self.clock() * 1000)
        self.state = decide(self.state, self.sample(now_ms), now_ms)
        if self.config.changelog is not None:
            self.state = changelog_step(self.state, self.config.changelog, self.http, self.cli, now_ms)
        self.store.save(self.state)
        self.state = {**self.state, "outbox": flush(self.cli, self.config.channel, self.state["outbox"], self.pager)}
        self.store.save(self.state)
        self.state = {**self.state, "posts": flush(self.cli, self.config.channel, self.state["posts"], self.pager)}
        self.store.save(self.state)

    def run(self, pause=time.sleep):
        say("watching", channel=self.config.channel, deployments=list(self.config.deployments), warm_pod=self.config.warm_pod_file, disks=[disk.check for disk in self.config.disks], releases=[release.check for release in self.config.releases])
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
