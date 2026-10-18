#!/usr/bin/env python3
# forge-mirror: pushes duckpond forge's dev and main of core, programs and app to GitHub's ducktape, programs and app, forge always winning, woken by the blocks that write them.
# ops/k8s/forge-mirror/forge-mirror.yaml runs it on fabrics-1 as the GitHub App in the Secret github-app; mirror-test.py tests it.
import base64
import json
import os
import queue
import socket
import ssl
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from urllib.parse import urlsplit

FORGE = "forge"
API = "https://api.github.com"
GITHUB_HEADER = "http.https://github.com/.extraheader"
ATTEMPTS_PER_LINE = 10
READ = object()


class Stopped(Exception):
    def __init__(self, reason, detail):
        super().__init__(f"{reason}: {detail}")
        self.reason = reason
        self.detail = detail


def say(event, **fields):
    print(json.dumps({"event": event, **fields}, sort_keys=True), flush=True)


def worth_a_line(attempts):
    return attempts == 1 or attempts % ATTEMPTS_PER_LINE == 0


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
            raise Stopped("short_frame", "a frame from the node ended early")
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


def oid_of(value):
    return None if value is None else Reader(value).bytes().hex()


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
        "User-Agent: forge-mirror",
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
            raise Stopped("follow_refused", f"the node answered {b' '.join(status).decode(errors='replace')} to following {program}")
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
        request = urllib.request.Request(f"{self.url}/v1/get", data=body, method="POST", headers={"User-Agent": "forge-mirror"})
        with urllib.request.urlopen(request) as answer:
            return Reader(answer.read()).option_bytes()

    def follow(self, program):
        return Changes(self.url, program)


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def git_auth(token):
    basic = base64.b64encode(f"x-access-token:{token}".encode()).decode()
    return {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": GITHUB_HEADER, "GIT_CONFIG_VALUE_0": f"AUTHORIZATION: basic {basic}"}


def issuer(app_id):
    return int(app_id) if app_id.isdigit() else app_id


class App:
    def __init__(self, app_id, installation, key_pem, api=API, clock=time.time):
        from cryptography.hazmat.primitives import serialization

        self.app_id = app_id
        self.installation = installation
        self.key = serialization.load_pem_private_key(key_pem, password=None)
        self.api = api
        self.clock = clock
        self.token = None
        self.expires = 0.0

    @staticmethod
    def from_dir(path, api):
        path = Path(path)
        return App((path / "app_id").read_text().strip(), (path / "installation_id").read_text().strip(), (path / "private_key").read_bytes(), api)

    def jwt(self):
        from cryptography.hazmat.primitives import hashes
        from cryptography.hazmat.primitives.asymmetric import padding

        now = int(self.clock())
        header = b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
        claims = b64url(json.dumps({"iat": now - 60, "exp": now + 540, "iss": issuer(self.app_id)}).encode())
        signing = f"{header}.{claims}".encode()
        return f"{header}.{claims}.{b64url(self.key.sign(signing, padding.PKCS1v15(), hashes.SHA256()))}"

    def mint(self):
        request = urllib.request.Request(
            f"{self.api}/app/installations/{self.installation}/access_tokens",
            data=b"",
            method="POST",
            headers={
                "Authorization": f"Bearer {self.jwt()}",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "forge-mirror",
            },
        )
        try:
            with urllib.request.urlopen(request) as answer:
                minted = json.load(answer)
        except urllib.error.HTTPError as refused:
            code = refused.code
            refused.close()
            raise Stopped("token_refused", f"GitHub answered {code} to minting an installation token") from None
        self.token = minted["token"]
        self.expires = datetime.fromisoformat(minted["expires_at"]).timestamp()

    def auth(self):
        expired = self.token is None or self.clock() >= self.expires
        if expired:
            self.mint()
        return git_auth(self.token)


class Mirror:
    def __init__(self, root, pair, config):
        self.path = Path(root) / f"{pair.forge}.git"
        self.forge_url = config.forge_url.format(forge=pair.forge)
        self.github_url = config.github_url.format(github=pair.github)
        self.chain_node = f"duck.{config.chain}.node={config.node}"
        if not self.path.exists():
            subprocess.run(["git", "init", "--quiet", "--bare", str(self.path)], check=True, capture_output=True)

    def git(self, *args, auth=None):
        command = ["git", "-C", str(self.path), "-c", self.chain_node, *args]
        return subprocess.run(command, capture_output=True, text=True, env=dict(os.environ, **(auth or {})))

    def must(self, *args, auth=None):
        ran = self.git(*args, auth=auth)
        if ran.returncode != 0:
            raise Stopped("git_failed", f"git {' '.join(args[:2])} in {self.path.name}: {' / '.join(ran.stderr.strip().splitlines())}")
        return ran.stdout

    def github_head(self, branch, auth):
        listed = self.must("ls-remote", self.github_url, f"refs/heads/{branch}", auth=auth).split()
        return listed[0] if listed else None

    def has(self, oid):
        return self.git("cat-file", "-e", f"{oid}^{{commit}}").returncode == 0

    def fetch_github(self, branch, oid, auth):
        if self.has(oid):
            return True
        self.must("fetch", "--quiet", "--no-tags", "--no-write-fetch-head", self.github_url, f"+refs/heads/{branch}:refs/github/{branch}", auth=auth)
        return self.has(oid)

    def fetch_forge(self, branch, oid):
        if self.has(oid):
            return
        self.must("fetch", "--quiet", "--no-tags", "--no-write-fetch-head", self.forge_url, f"+refs/heads/{branch}:refs/forge/{branch}")
        if not self.has(oid):
            raise Stopped("forge_moved", f"forge {self.path.stem} {branch} holds {oid}, which its fetch did not bring")

    def only_in(self, tip, base):
        return self.must("rev-list", tip, f"^{base}").split()

    def push(self, branch, oid, lease, auth):
        guard = f"--force-with-lease=refs/heads/{branch}:{lease or ''}"
        return self.git("push", "--quiet", "--porcelain", guard, self.github_url, f"{oid}:refs/heads/{branch}", auth=auth)


@dataclass(frozen=True)
class Config:
    node: str
    chain: str
    clock: str
    state: Path
    app: Path
    pairs: tuple
    forge_url: str
    github_url: str
    api: str

    @staticmethod
    def from_env(env):
        repos = [entry.split("=", 1) for entry in env["FORGE_MIRROR_REPOS"].split()]
        branches = env["FORGE_MIRROR_BRANCHES"].split()
        chain = env["FORGE_MIRROR_CHAIN"]
        return Config(
            node=env["FORGE_MIRROR_NODE"],
            chain=chain,
            clock=env["FORGE_MIRROR_CLOCK"],
            state=Path(env["FORGE_MIRROR_STATE"]),
            app=Path(env["FORGE_MIRROR_APP"]),
            pairs=tuple(Pair(forge, github, branch) for forge, github in repos for branch in branches),
            forge_url=env.get("FORGE_MIRROR_FORGE_URL", f"duck://{chain}/forge/{{forge}}"),
            github_url=env.get("FORGE_MIRROR_GITHUB_URL", "https://github.com/{github}.git"),
            api=env.get("FORGE_MIRROR_API", API),
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
        say("follow_broke", program=program, reason="connection_lost", detail=str(error))
    finally:
        inbox.put(Ended(program))


class Follower:
    def __init__(self, config, node, app):
        self.config = config
        self.node = node
        self.app = app
        self.mirrors = {}
        self.pair_of_key = {ref_key(pair): pair for pair in config.pairs}
        self.inbox = queue.Queue()
        self.seen = {}
        self.attempts = {}

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
        for program in (FORGE, self.config.clock):
            self.follow(program)
        say("following", node=self.config.node, pairs=[f"{pair.forge}:{pair.branch}" for pair in self.config.pairs], clock=self.config.clock)
        for pair in self.config.pairs:
            self.sync(pair, "start")
        while True:
            events = [self.inbox.get()]
            while not self.inbox.empty():
                events.append(self.inbox.get())
            self.wake(events)

    def wake(self, events):
        moved = {}
        for event in events:
            match event:
                case Ended(program):
                    self.refollow(program, moved)
                case Wrote(program, change):
                    self.read(program, change, moved)
        for pair, (cause, head) in moved.items():
            self.sync(pair, cause, head)
        for pair in sorted(self.attempts.keys() - moved.keys(), key=self.config.pairs.index):
            self.sync(pair, "retry")

    def read(self, program, change, moved):
        if program != FORGE:
            return
        for key, value in change.writes:
            if key in self.pair_of_key:
                moved[self.pair_of_key[key]] = (f"forge wrote at {change.height}", oid_of(value))

    def refollow(self, program, moved):
        self.follow(program)
        say("refollowed", program=program)
        if program == FORGE:
            self.missed_moves(moved)

    def missed_moves(self, moved):
        for pair in self.config.pairs:
            head = self.forge_head(pair)
            if head != self.seen.get(pair):
                moved[pair] = ("forge moved while the follow was down", head)

    def sync(self, pair, cause, head=READ):
        try:
            forge = self.forge_head(pair) if head is READ else head
            outcome = self.settled(pair, forge, cause)
        except Stopped as failed:
            self.failed(pair, cause, failed.reason, failed.detail)
            return "failed"
        except OSError as unreachable:
            self.failed(pair, cause, "unreachable", str(unreachable))
            return "failed"
        self.recovered(pair)
        self.seen[pair] = forge
        return outcome

    def settled(self, pair, forge, cause):
        while True:
            outcome = self.attempt(pair, forge)
            say("synced", forge=pair.forge, github=pair.github, branch=pair.branch, head=forge, outcome=outcome, cause=cause)
            if outcome != "raced":
                return outcome

    def failed(self, pair, cause, reason, detail):
        attempts = self.attempts.get(pair, 0) + 1
        self.attempts[pair] = attempts
        if worth_a_line(attempts):
            say("push_failed", forge=pair.forge, github=pair.github, branch=pair.branch, cause=cause, attempts=attempts, reason=reason, detail=detail)

    def recovered(self, pair):
        attempts = self.attempts.pop(pair, None)
        if attempts is not None:
            say("push_recovered", forge=pair.forge, github=pair.github, branch=pair.branch, attempts=attempts)

    def attempt(self, pair, forge):
        if forge is None:
            return "forge_absent"
        mirror = self.mirror(pair)
        auth = self.app.auth()
        github = mirror.github_head(pair.branch, auth)
        if github == forge:
            return "equal"
        fetched = github is None or mirror.fetch_github(pair.branch, github, auth)
        if not fetched:
            return "raced"
        mirror.fetch_forge(pair.branch, forge)
        dropped = [] if github is None else mirror.only_in(github, forge)
        pushed = mirror.push(pair.branch, forge, github, auth)
        if pushed.returncode != 0:
            return self.refused(mirror, pair, github, pushed, auth)
        if not dropped:
            return "pushed"
        say("reset", forge=pair.forge, github=pair.github, branch=pair.branch, old=github, new=forge, dropped=dropped)
        return "reset"

    def refused(self, mirror, pair, github, pushed, auth):
        now = mirror.github_head(pair.branch, auth)
        if now != github:
            return "raced"
        said = " / ".join((pushed.stdout + pushed.stderr).strip().splitlines())
        raise Stopped("push_refused", f"GitHub refused the push: {said}")


def main():
    config = Config.from_env(os.environ)
    home = config.state / "ducktape"
    home.mkdir(parents=True, exist_ok=True)
    os.environ["DUCKTAPE_HOME"] = str(home)
    app = App.from_dir(config.app, config.api)
    try:
        Follower(config, Node(config.node), app).run()
    except Stopped as stopped:
        say("stopped", reason=stopped.reason, detail=stopped.detail)
        sys.exit(1)


if __name__ == "__main__":
    main()
