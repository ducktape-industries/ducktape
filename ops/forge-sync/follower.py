#!/usr/bin/env python3
# The forge-sync follower: brings GitHub's dev and main of ducktape, programs and app to duckpond forge's heads, forge winning a divergence, woken by each block that writes one of those forge refs or posts in #sync.
# forge-sync.service runs it on orthorymain, pushing over SSH with orthory's key; follower-test.py tests it.
import base64
import json
import os
import queue
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

FORGE = "forge"
CHAT = "chat"


class Stopped(Exception):
    pass


def say(event, **fields):
    print(json.dumps({"event": event, **fields}, sort_keys=True), flush=True)


def u8(value):
    return value.to_bytes(1, "little")


def u64(value):
    return value.to_bytes(8, "little")


def borsh_bytes(data):
    return len(data).to_bytes(4, "little") + data


def borsh_text(value):
    return borsh_bytes(value.encode())


def borsh_vec(items):
    return len(items).to_bytes(4, "little") + b"".join(items)


class Reader:
    def __init__(self, data):
        self.data = data
        self.at = 0

    def take(self, count):
        piece = self.data[self.at : self.at + count]
        if len(piece) != count:
            raise Stopped("a frame from the node ended early")
        self.at += count
        return piece

    def u8(self):
        return self.take(1)[0]

    def u32(self):
        return int.from_bytes(self.take(4), "little")

    def u64(self):
        return int.from_bytes(self.take(8), "little")

    def bytes(self):
        return self.take(self.u32())

    def option_bytes(self):
        return self.bytes() if self.u8() == 1 else None


def key_part(raw):
    return raw.replace(b"\x00", b"\x00\xff") + b"\x00\x00"


@dataclass(frozen=True)
class Pair:
    forge: str
    github: str
    branch: str


def ref_key(pair):
    return b"r/" + key_part(pair.forge.encode()) + key_part(f"refs/heads/{pair.branch}".encode())


def channel_head_key(channel):
    return b"head/" + key_part(channel.encode())


def message_key(message_id):
    return b"message-id/" + key_part(message_id.encode())


def oid_of(value):
    return None if value is None else Reader(value).bytes().hex()


def post_message(channel, message_id, line):
    paragraph = u8(0) + borsh_vec([borsh_text(line) + borsh_vec([])])
    return u8(4) + borsh_text(channel) + borsh_text(message_id) + borsh_vec([paragraph]) + u8(0)


@dataclass(frozen=True)
class Change:
    height: int
    writes: list


def decode_change(frame):
    reader = Reader(frame)
    height = reader.u64()
    reader.take(32)
    writes = [(reader.bytes(), reader.option_bytes()) for _ in range(reader.u32())]
    return Change(height, writes)


def handshake(host, path):
    key = base64.b64encode(os.urandom(16)).decode()
    lines = [
        f"GET {path} HTTP/1.1",
        f"Host: {host}",
        "Upgrade: websocket",
        "Connection: Upgrade",
        f"Sec-WebSocket-Key: {key}",
        "Sec-WebSocket-Version: 13",
        "User-Agent: forge-sync",
    ]
    return ("\r\n".join(lines) + "\r\n\r\n").encode()


def masked(opcode, payload):
    mask = os.urandom(4)
    body = bytes(byte ^ mask[at % 4] for at, byte in enumerate(payload))
    return bytes([0x80 | opcode, 0x80 | len(payload)]) + mask + body


class Changes:
    def __init__(self, url, program):
        parts = urlsplit(url)
        secure = parts.scheme == "https"
        plain = socket.create_connection((parts.hostname, parts.port or (443 if secure else 80)))
        self.socket = ssl.create_default_context().wrap_socket(plain, server_hostname=parts.hostname) if secure else plain
        self.stream = self.socket.makefile("rb")
        self.socket.sendall(handshake(parts.netloc, f"{parts.path.rstrip('/')}/v1/changes/{program}"))
        status = self.stream.readline().split()
        if status[1:2] != [b"101"]:
            self.stream.close()
            self.socket.close()
            raise Stopped(f"the node answered {b' '.join(status).decode(errors='replace')} to following {program}")
        while self.stream.readline() not in (b"\r\n", b""):
            pass

    def frame(self):
        head = self.stream.read(2)
        if len(head) < 2:
            return None
        length = head[1] & 0x7F
        if length == 126:
            length = int.from_bytes(self.stream.read(2), "big")
        elif length == 127:
            length = int.from_bytes(self.stream.read(8), "big")
        payload = self.stream.read(length)
        if len(payload) < length:
            return None
        return head[0] & 0x80, head[0] & 0x0F, payload

    def __iter__(self):
        try:
            yield from self.messages()
        finally:
            self.stream.close()
            self.socket.close()

    def messages(self):
        message = b""
        while True:
            frame = self.frame()
            if frame is None:
                return
            final, opcode, payload = frame
            match opcode:
                case 0x8:
                    return
                case 0x9:
                    self.socket.sendall(masked(0xA, payload))
                case 0xA:
                    pass
                case _:
                    message += payload
                    if final:
                        yield decode_change(message)
                        message = b""


class Node:
    def __init__(self, url):
        self.url = url.rstrip("/")

    def get(self, program, key):
        body = u8(0) + borsh_text(program) + borsh_bytes(key)
        request = urllib.request.Request(f"{self.url}/v1/get", data=body, method="POST", headers={"User-Agent": "forge-sync"})
        with urllib.request.urlopen(request) as answer:
            return Reader(answer.read()).option_bytes()

    def follow(self, program):
        return Changes(self.url, program)


class Poster:
    def __init__(self, ducktape, node, keys, home):
        self.ducktape = ducktape
        self.node = node
        self.keys = Path(keys)
        self.home = home

    def post(self, channel, message_id, line):
        env = dict(
            os.environ,
            DUCKTAPE_HOME=self.home,
            DUCKTAPE_USER_KEY=str(self.keys / "key"),
            DUCKTAPE_PASSWORD=(self.keys / "password").read_text().strip(),
            NO_COLOR="1",
        )
        with tempfile.TemporaryDirectory() as temp:
            op = Path(temp) / "op"
            op.write_bytes(post_message(channel, message_id, line))
            ran = subprocess.run([self.ducktape, "--node", self.node, "submit", CHAT, str(op), "--preconfirm"], capture_output=True, env=env)
        said = ran.stdout.decode(errors="replace").strip()
        if ran.returncode == 0 and said.startswith("preconfirmed applied"):
            return "applied"
        if ran.returncode == 0 and said.startswith("preconfirmed rejected already_exists"):
            return "exists"
        raise Stopped(f"posting {message_id} to #{channel}: {said or ran.stderr.decode(errors='replace').strip()}")


def relation(forge, github, contains):
    if forge is None:
        return "forge_absent"
    if github is None:
        return "github_absent"
    if forge == github:
        return "equal"
    if contains(forge, github):
        return "behind"
    if contains(github, forge):
        return "ahead"
    return "diverged"


@dataclass(frozen=True)
class Report:
    message_id: str
    pair: Pair
    old: str
    new: str
    dropped: list

    def line(self):
        pair = self.pair
        dropped = ", ".join(self.dropped)
        return f"forge wins on {pair.github} {pair.branch}: GitHub's head was {self.old} and is now {self.new}, forge {pair.forge}'s {pair.branch}. Dropped commits: {dropped}."

    def to_json(self):
        return {"message_id": self.message_id, "forge": self.pair.forge, "github": self.pair.github, "branch": self.pair.branch, "old": self.old, "new": self.new, "dropped": self.dropped}

    @staticmethod
    def from_json(data):
        return Report(data["message_id"], Pair(data["forge"], data["github"], data["branch"]), data["old"], data["new"], data["dropped"])


def report_id(pair, old, new):
    return f"forge-sync-{pair.forge}-{pair.branch.replace('/', '-')}-{old[:12]}-{new[:12]}-{os.urandom(4).hex()}"


class Journal:
    def __init__(self, root):
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)

    def write(self, report):
        path = self.root / f"{report.message_id}.json"
        held = path.with_suffix(".new")
        held.write_text(json.dumps(report.to_json()))
        held.replace(path)

    def drop(self, report):
        (self.root / f"{report.message_id}.json").unlink(missing_ok=True)

    def held(self):
        return [Report.from_json(json.loads(path.read_text())) for path in sorted(self.root.glob("*.json"))]


class Mirror:
    def __init__(self, root, pair, config):
        self.path = Path(root) / f"{pair.forge}.git"
        self.forge_url = config.forge_url.format(forge=pair.forge)
        self.github_url = config.github_url.format(github=pair.github)
        self.chain_node = f"duck.{config.chain}.node={config.node}"
        if not self.path.exists():
            subprocess.run(["git", "init", "--quiet", "--bare", str(self.path)], check=True, capture_output=True)

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.path), "-c", self.chain_node, *args], capture_output=True, text=True)

    def must(self, *args):
        ran = self.git(*args)
        if ran.returncode != 0:
            raise Stopped(f"git {' '.join(args[:2])} in {self.path.name}: {' / '.join(ran.stderr.strip().splitlines())}")
        return ran.stdout

    def github_head(self, branch):
        listed = self.must("ls-remote", self.github_url, f"refs/heads/{branch}").split()
        return listed[0] if listed else None

    def has(self, oid):
        return self.git("cat-file", "-e", f"{oid}^{{commit}}").returncode == 0

    def fetch_forge(self, branch, oid):
        if self.has(oid):
            return
        self.must("fetch", "--quiet", "--no-tags", "--no-write-fetch-head", self.forge_url, f"+refs/heads/{branch}:refs/forge/{branch}")
        if not self.has(oid):
            raise Stopped(f"forge {self.path.stem} {branch} holds {oid}, which its fetch did not bring")

    def fetch_github(self, branch, oid):
        if self.has(oid):
            return True
        self.must("fetch", "--quiet", "--no-tags", "--no-write-fetch-head", self.github_url, f"+refs/heads/{branch}:refs/github/{branch}")
        return self.has(oid)

    def contains(self, descendant, ancestor):
        return self.git("merge-base", "--is-ancestor", ancestor, descendant).returncode == 0

    def only_in(self, tip, base):
        return self.must("rev-list", tip, f"^{base}").split()

    def push(self, branch, oid, lease=None):
        guard = [] if lease is None else [f"--force-with-lease=refs/heads/{branch}:{lease}"]
        return self.git("push", "--quiet", "--porcelain", *guard, self.github_url, f"{oid}:refs/heads/{branch}")


@dataclass(frozen=True)
class Config:
    node: str
    chain: str
    channel: str
    state: Path
    pairs: tuple
    forge_url: str
    github_url: str

    @staticmethod
    def from_env(env):
        repos = [entry.split("=", 1) for entry in env["FORGE_SYNC_REPOS"].split()]
        branches = env["FORGE_SYNC_BRANCHES"].split()
        chain = env["FORGE_SYNC_CHAIN"]
        return Config(
            node=env["FORGE_SYNC_NODE"],
            chain=chain,
            channel=env["FORGE_SYNC_CHANNEL"],
            state=Path(env["FORGE_SYNC_STATE"]),
            pairs=tuple(Pair(forge, github, branch) for forge, github in repos for branch in branches),
            forge_url=env.get("FORGE_SYNC_FORGE_URL", f"duck://{chain}/forge/{{forge}}"),
            github_url=env.get("FORGE_SYNC_GITHUB_URL", "git@github.com:{github}.git"),
        )


@dataclass(frozen=True)
class Wrote:
    program: str
    change: Change


@dataclass(frozen=True)
class Ended:
    program: str


def pump(program, changes, inbox):
    try:
        for change in changes:
            inbox.put(Wrote(program, change))
    except OSError as error:
        say("follow_broke", program=program, reason=str(error))
    finally:
        inbox.put(Ended(program))


class Follower:
    def __init__(self, config, node, poster):
        self.config = config
        self.node = node
        self.poster = poster
        self.journal = Journal(config.state / "reports")
        self.mirrors = {}
        self.pair_of_key = {ref_key(pair): pair for pair in config.pairs}
        self.asked_key = channel_head_key(config.channel)
        self.inbox = queue.Queue()
        self.seen = {}
        self.pending = set()
        self.asked = None

    def mirror(self, pair):
        if pair.forge not in self.mirrors:
            self.mirrors[pair.forge] = Mirror(self.config.state / "git", pair, self.config)
        return self.mirrors[pair.forge]

    def forge_head(self, pair):
        return oid_of(self.node.get(FORGE, ref_key(pair)))

    def follow(self, program):
        changes = self.node.follow(program)
        threading.Thread(target=pump, args=(program, changes, self.inbox), daemon=True).start()

    def run(self):
        for program in (FORGE, CHAT):
            self.follow(program)
        say("following", node=self.config.node, pairs=[f"{pair.forge}:{pair.branch}" for pair in self.config.pairs], channel=self.config.channel)
        self.asked = self.node.get(CHAT, self.asked_key)
        self.settle_reports()
        self.sync_all("start")
        while True:
            events = [self.inbox.get()]
            while not self.inbox.empty():
                events.append(self.inbox.get())
            self.wake(events)

    def wake(self, events):
        moved = {}
        asked = []
        for event in events:
            match event:
                case Ended(program):
                    self.refollow(program, moved, asked)
                case Wrote(program, change):
                    self.read(program, change, moved, asked)
        if not moved and not asked:
            return
        self.settle_reports()
        if asked:
            self.sync_all(asked[-1])
            return
        for pair, (cause, head) in moved.items():
            self.sync(pair, head, cause)
        for pair in sorted(self.pending - moved.keys(), key=self.config.pairs.index):
            self.sync(pair, self.forge_head(pair), "retry")

    def read(self, program, change, moved, asked):
        for key, value in change.writes:
            if program == FORGE and key in self.pair_of_key:
                moved[self.pair_of_key[key]] = (f"forge wrote at {change.height}", oid_of(value))
            if program == CHAT and key == self.asked_key:
                self.asked = value
                asked.append(f"#{self.config.channel} posted at {change.height}")

    def refollow(self, program, moved, asked):
        self.follow(program)
        say("refollowed", program=program)
        match program:
            case "forge":
                self.missed_moves(moved)
            case "chat":
                self.missed_asks(asked)

    def missed_moves(self, moved):
        for pair in self.config.pairs:
            head = self.forge_head(pair)
            if head != self.seen.get(pair):
                moved[pair] = ("forge moved while the follow was down", head)

    def missed_asks(self, asked):
        value = self.node.get(CHAT, self.asked_key)
        if value != self.asked:
            self.asked = value
            asked.append(f"#{self.config.channel} posted while the follow was down")

    def sync_all(self, cause):
        for pair in self.config.pairs:
            self.sync(pair, self.forge_head(pair), cause)

    def sync(self, pair, forge, cause):
        try:
            outcome = self.settled(pair, forge, cause)
        except Stopped as failed:
            self.failed(pair, forge, cause, str(failed))
            return "failed"
        self.pending.discard(pair)
        self.seen[pair] = forge
        return outcome

    def settled(self, pair, forge, cause):
        while True:
            outcome = self.attempt(pair, forge)
            say("synced", forge=pair.forge, github=pair.github, branch=pair.branch, head=forge, outcome=outcome, cause=cause)
            if outcome != "raced":
                return outcome

    def failed(self, pair, forge, cause, reason):
        self.pending.add(pair)
        say("failed", forge=pair.forge, github=pair.github, branch=pair.branch, head=forge, cause=cause, reason=reason)
        message_id = f"forge-sync-failed-{pair.forge}-{pair.branch.replace('/', '-')}-{(forge or 'none')[:12]}"
        line = f"forge-sync could not bring GitHub {pair.github} {pair.branch} to forge {pair.forge}'s {forge}: {reason}. It tries again at the next forge push or #{self.config.channel} post."
        try:
            say("reported", message_id=message_id, outcome=self.poster.post(self.config.channel, message_id, line))
        except Stopped as unposted:
            say("unreported", message_id=message_id, reason=str(unposted))

    def attempt(self, pair, forge):
        if forge is None:
            return "forge_absent"
        mirror = self.mirror(pair)
        github = mirror.github_head(pair.branch)
        if github == forge:
            return "equal"
        fetched = github is None or mirror.fetch_github(pair.branch, github)
        if not fetched:
            return "raced"
        mirror.fetch_forge(pair.branch, forge)
        step = relation(forge, github, mirror.contains)
        match step:
            case "forge_absent" | "equal" | "ahead":
                return step
            case "github_absent" | "behind":
                return self.fast_forward(mirror, pair, forge, github)
            case "diverged":
                return self.reset(mirror, pair, forge, github)

    def fast_forward(self, mirror, pair, forge, github):
        pushed = mirror.push(pair.branch, forge)
        if pushed.returncode == 0:
            return "pushed"
        return self.refused(mirror, pair, github, pushed)

    def reset(self, mirror, pair, forge, github):
        report = Report(report_id(pair, github, forge), pair, github, forge, mirror.only_in(github, forge))
        self.journal.write(report)
        pushed = mirror.push(pair.branch, forge, lease=github)
        if pushed.returncode != 0:
            self.journal.drop(report)
            return self.refused(mirror, pair, github, pushed)
        say("reset", forge=pair.forge, github=pair.github, branch=pair.branch, old=github, new=forge, dropped=report.dropped)
        self.report(report)
        return "reset"

    def refused(self, mirror, pair, github, pushed):
        now = mirror.github_head(pair.branch)
        if now != github:
            return "raced"
        said = " / ".join((pushed.stdout + pushed.stderr).strip().splitlines())
        raise Stopped(f"GitHub refused the push: {said}")

    def report(self, report):
        outcome = self.poster.post(self.config.channel, report.message_id, report.line())
        say("reported", message_id=report.message_id, outcome=outcome)
        self.journal.drop(report)

    def settle_reports(self):
        for report in self.journal.held():
            self.settle(report)

    def settle(self, report):
        if self.node.get(CHAT, message_key(report.message_id)) is not None:
            self.journal.drop(report)
            return
        mirror = self.mirror(report.pair)
        github = mirror.github_head(report.pair.branch)
        fetched = github is not None and mirror.fetch_github(report.pair.branch, github)
        happened = fetched and mirror.contains(github, report.new) and not mirror.contains(github, report.old)
        if happened:
            self.report(report)
            return
        self.journal.drop(report)


def main():
    config = Config.from_env(os.environ)
    home = config.state / "ducktape"
    home.mkdir(parents=True, exist_ok=True)
    os.environ["DUCKTAPE_HOME"] = str(home)
    poster = Poster(os.environ.get("FORGE_SYNC_DUCKTAPE", "ducktape"), config.node, config.state / "keys", str(home))
    try:
        Follower(config, Node(config.node), poster).run()
    except Stopped as stopped:
        say("stopped", reason=str(stopped))
        sys.exit(1)


if __name__ == "__main__":
    main()
