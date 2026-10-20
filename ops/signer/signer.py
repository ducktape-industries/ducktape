#!/usr/bin/env python3
# The signer: an actions account runner whose labels are signing capabilities (sign:<identity>). It claims a check, checks from the network alone that its commit is on an allowed branch and its inputs are what a trusted builder's passed check put, runs its own pinned tool for the operation the check names, and reports the signed outputs with a signature record.
# The keys stay with the tools on the machine it runs on (a keychain); it never runs a check's commands or its commit's code. signer-test.py tests it; README.md says how to install it.
import base64
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ACTIONS = "actions"
FORGE = "forge"
IDENTITY = "identity"
USER_KEY = "ducktape-user-key-v1:"
USER_KEY_LEN = 16 + 12 + 24 + 32 + 32 + 16
PUBKEY_AT = 16 + 12 + 24
OUTPUT = "output"
FOLLOW_PAUSE = 5
STOPPED = "the signer stopped while it held this check; no outputs were recorded for it"
BROKE = "the signer failed while it held this check; its own log says why, and no outputs were recorded for it"
CHECK_STATES = ("queued", "running", "passed", "failed", "errored", "superseded", "waiting", "skipped")
HASHES = {"sha256": (0, 32), "sha1": (1, 20)}
ENV_NAME = re.compile(r"[A-Z][A-Z0-9_]*")


class Refused(Exception):
    def __init__(self, sentence):
        super().__init__(sentence)
        self.sentence = sentence


def say(event, **fields):
    words = " ".join(f"{name}={value}" for name, value in fields.items())
    print(f"event={event} {words}".strip(), file=sys.stderr, flush=True)


def u8(value):
    return bytes([value])


def u32(value):
    return value.to_bytes(4, "little")


def u64(value):
    return value.to_bytes(8, "little")


def borsh_bytes(data):
    return u32(len(data)) + data


def borsh_text(value):
    return borsh_bytes(value.encode())


def borsh_vec(items):
    return u32(len(items)) + b"".join(items)


def borsh_option(value, encode):
    return b"\x00" if value is None else b"\x01" + encode(value)


def blob_id(text):
    kind, _, digest = text.partition(":")
    known = HASHES.get(kind)
    if known is None or not re.fullmatch(r"[0-9a-f]+", digest) or len(digest) != 2 * known[1]:
        raise Refused(f"{text} is not a blob id")
    return u8(known[0]) + bytes.fromhex(digest)


def actions_register(labels):
    return u8(2) + borsh_vec([borsh_text(label) for label in labels]) + borsh_bytes(b"")


def actions_claim(check):
    return u8(3) + u64(check)


def actions_report(check, state, summary):
    return u8(4) + u64(check) + u8(CHECK_STATES.index(state)) + borsh_text(summary) + b"\x00"


def actions_outputs(check, files):
    return u8(7) + u64(check) + borsh_vec([borsh_text(path) + blob_id(blob) + u64(size) for path, blob, size in files])


def actions_check(check):
    return u8(0) + u64(check)


def actions_queued(after):
    return u8(2) + borsh_option(after, borsh_bytes) + b"\x00"


def actions_needs(check):
    return u8(6) + u64(check)


def actions_outputs_of(check):
    return u8(7) + u64(check)


def identity_of_key(key):
    return u8(0) + borsh_bytes(key)


def public_key(line):
    text = line.strip()
    body = text.removeprefix(USER_KEY)
    blob = base64.b64decode(body + "=" * (-len(body) % 4), altchars=b"-_", validate=True) if text.startswith(USER_KEY) else b""
    if len(blob) != USER_KEY_LEN:
        raise Refused(f"the key is not a {USER_KEY} key")
    return blob[PUBKEY_AT : PUBKEY_AT + 32]


def forge_compare(repo, commit, branch):
    revision_oid = u8(1) + borsh_text(commit)
    revision_ref = u8(0) + borsh_bytes(f"refs/heads/{branch}".encode())
    return u8(9) + borsh_text(repo) + revision_oid + revision_ref


class Reader:
    def __init__(self, data):
        self.data = data
        self.at = 0

    def take(self, count):
        piece = self.data[self.at : self.at + count]
        if len(piece) != count:
            raise Refused("a reply ended early")
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

    def text(self):
        return self.bytes().decode()

    def option(self, read):
        return read() if self.u8() else None

    def vec(self, read):
        return [read() for _ in range(self.u32())]

    def blob(self):
        tag = self.u8()
        for name, (index, size) in HASHES.items():
            if index == tag:
                return f"{name}:{self.take(size).hex()}"
        raise Refused(f"a reply names hash kind {tag}")

    def check(self):
        row = {"id": self.u64(), "run": self.u64(), "repo": self.text(), "commit": self.text(), "workflow": self.text(), "attempt": self.u32()}
        job = {"id": self.text(), "runs_on": self.vec(self.text), "image": self.text(), "steps": self.vec(self.text)}
        job["env"] = dict(self.vec(lambda: (self.text(), self.text())))
        self.vec(lambda: (self.text(), self.text()))
        state = self.u8()
        row["job"] = job
        row["state"] = CHECK_STATES[state] if state < len(CHECK_STATES) else f"state {state}"
        row["claimant"] = self.option(self.u64)
        row["reporter"] = self.option(self.u64)
        self.u64(), self.option(self.u64), self.option(self.u64), self.text(), self.option(self.text)
        return row

    def page(self, read):
        self.u64()
        return self.vec(read), self.option(self.bytes)


class Ran:
    def __init__(self, code, out, err):
        self.code, self.out, self.err = code, out, err


def run(argv, stdin=None, env=None):
    done = subprocess.run(argv, input=stdin, capture_output=True, env=env)
    return Ran(done.returncode, done.stdout, done.stderr.decode(errors="replace").strip())


def failed(ran):
    return ran.err or ran.out.decode(errors="replace").strip() or f"exited {ran.code}"


def receipt(ran):
    text = ran.out.decode(errors="replace").strip()
    applied = re.match(r"preconfirmed applied", text)
    rejected = re.match(r"preconfirmed rejected (\w+): (.*)", text, re.S)
    if ran.code == 0 and applied:
        return "applied", "", ""
    if ran.code == 0 and rejected:
        return "rejected", rejected.group(1), rejected.group(2).strip()
    return "failed", "", ran.err or text or f"ducktape exited {ran.code}"


class Following:
    def __init__(self, argv, env):
        self.argv = argv
        self.env = env
        self.why = None

    def __iter__(self):
        with tempfile.TemporaryFile() as err:
            process = subprocess.Popen(self.argv, stdout=subprocess.PIPE, stderr=err, env=self.env)
            with process.stdout:
                yield from process.stdout
            code = process.wait()
            err.seek(0)
            said = err.read().decode(errors="replace").strip()
        self.why = said or f"exited {code}"


class Network:
    def __init__(self, ducktape, keys):
        self.ducktape = ducktape
        self.keys = keys

    def env(self):
        return dict(
            os.environ,
            DUCKTAPE_USER_KEY=str(self.keys / "key"),
            DUCKTAPE_PASSWORD=(self.keys / "password").read_text().strip(),
            NO_COLOR="1",
        )

    def key(self):
        return public_key((self.keys / "key").read_text())

    def query(self, program, request):
        return run([*self.ducktape, "query", program, "--raw"], stdin=request, env=self.env())

    def submit(self, program, payload):
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / "op"
            file.write_bytes(payload)
            return run([*self.ducktape, "submit", program, str(file), "--preconfirm"], env=self.env())

    def put(self, paths, kind):
        return run([*self.ducktape, "blob", "put", "--kind", kind, "--wait", *map(str, paths)], env=self.env())

    def blob(self, blob):
        return run([*self.ducktape, "blob", "get", blob], env=self.env())

    def follow(self, program):
        return Following([*self.ducktape, "changes", program], self.env())


class Signer:
    def __init__(self, network, config, home):
        self.network = network
        self.labels = tuple(config["labels"])
        self.builders = set(config["builders"])
        self.repos = config["repos"]
        self.tools = config["tools"]
        self.account = None
        self.home = home
        self.pending = home / "pending.json"
        self.claimed = home / "claimed.json"
        self.logs = home / "logs"
        self.wake = threading.Event()
        self.pause = time.sleep
        self.unsent = 0
        self.unpulled = 0
        self.unnamed = set()

    def asked(self, program, request, what, tag):
        ran = self.network.query(program, request)
        if ran.code != 0:
            raise Refused(f"{program} did not answer {what}: {failed(ran)}")
        reader = Reader(ran.out)
        if reader.u8() != tag:
            raise Refused(f"{program} answered {what} with another reply")
        return reader

    def sent(self, program, payload):
        outcome, reason, sentence = receipt(self.network.submit(program, payload))
        if outcome == "failed":
            return sentence
        if outcome == "rejected":
            say("signer_refused", program=program, reason=reason, detail=json.dumps(sentence))
        return None

    def written(self, path, text):
        self.home.mkdir(parents=True, exist_ok=True)
        fresh = path.with_name(path.name + ".new")
        fresh.write_text(text)
        os.replace(fresh, path)

    def owed(self):
        try:
            return json.loads(self.pending.read_text())
        except FileNotFoundError:
            return []

    def owe(self, payloads):
        self.written(self.pending, json.dumps(payloads))

    def send_owed(self):
        left, why = [], None
        for payload in self.owed():
            if left:
                left.append(payload)
                continue
            why = self.sent(ACTIONS, bytes.fromhex(payload))
            if why is not None:
                left.append(payload)
        self.owe(left)
        if left:
            self.unsent += 1
            if self.unsent == 1:
                say("signer_unsent", owed=len(left), detail=json.dumps(why))
        elif self.unsent:
            say("signer_sent", attempts=self.unsent + 1)
            self.unsent = 0
        return not left

    def resume(self):
        try:
            self.settle()
        except Refused as refusal:
            say("signer_unsettled", detail=json.dumps(refusal.sentence))

    def settle(self):
        try:
            check = json.loads(self.claimed.read_text())
        except FileNotFoundError:
            return
        reader = self.asked(ACTIONS, actions_check(check), "Check", 0)
        row = reader.option(reader.check)
        shutil.rmtree(self.home / "work" / str(check), ignore_errors=True)
        held = row is not None and row["state"] == "running" and row["claimant"] == self.account
        say("signer_settled", check=check, held=held)
        if held:
            self.report(check, "errored", STOPPED)
        else:
            self.claimed.unlink(missing_ok=True)

    def register(self):
        unsent = self.sent(ACTIONS, actions_register(self.labels))
        say("signer_registered", labels=",".join(self.labels), unsent=json.dumps(unsent))

    def follow(self):
        unheard = 0
        while True:
            following = self.network.follow(ACTIONS)
            self.wake.set()
            heard, height = False, None
            for line in following:
                if not heard and unheard:
                    say("signer_followed", attempts=unheard + 1)
                    unheard = 0
                heard = True
                at = line.split(b"\t", 1)[0]
                if at != height:
                    height = at
                    self.wake.set()
            if heard or not unheard:
                say("signer_unfollowed", heard=heard, detail=json.dumps(following.why))
            if not heard:
                unheard += 1
                self.pause(FOLLOW_PAUSE)

    def identify(self):
        try:
            key = self.network.key()
        except (OSError, ValueError, Refused) as error:
            say("signer_unkeyed", detail=json.dumps(f"keys/key is not a key this signer reads: {error}"))
            return False
        unasked = 0
        while True:
            try:
                reader = self.asked(IDENTITY, identity_of_key(key), "OfKey", 0)
                break
            except Refused as refusal:
                unasked += 1
                if unasked == 1:
                    say("signer_unidentified", detail=json.dumps(refusal.sentence))
                self.pause(FOLLOW_PAUSE)
        self.account = reader.option(reader.u64)
        if self.account is None:
            say("signer_unkeyed", detail=json.dumps(f"identity names no account for key {key.hex()}"))
            return False
        say("signer_identified", account=self.account, attempts=unasked + 1)
        return True

    def serve(self):
        if not self.identify():
            return
        self.register()
        self.resume()
        threading.Thread(target=self.follow, daemon=True).start()
        while True:
            self.wake.wait()
            self.wake.clear()
            self.pull()

    def queued(self):
        checks, after = [], None
        while True:
            reader = self.asked(ACTIONS, actions_queued(after), "Queued", 2)
            items, after = reader.page(reader.check)
            checks.extend(items)
            if after is None:
                return checks

    def pull(self):
        try:
            if not self.send_owed():
                return
            self.settle()
            queued = self.queued()
            if self.unpulled:
                say("signer_pulled", attempts=self.unpulled + 1)
                self.unpulled = 0
            self.unnamed &= {check["id"] for check in queued}
            covered = [check for check in queued if set(check["job"]["runs_on"]) <= set(self.labels) and check["id"] not in self.unnamed]
            for check in sorted(covered, key=lambda check: check["id"]):
                self.written(self.claimed, json.dumps(check["id"]))
                outcome, reason, sentence = receipt(self.network.submit(ACTIONS, actions_claim(check["id"])))
                if outcome == "applied":
                    self.signed(check)
                    if self.owed():
                        return
                    continue
                if outcome == "failed":
                    say("signer_unclaimed", check=check["id"], reason=outcome, detail=json.dumps(sentence))
                    return
                self.claimed.unlink(missing_ok=True)
                if reason == "unauthorized":
                    self.unnamed.add(check["id"])
                    say("signer_unnamed", check=check["id"], detail=json.dumps(sentence))
                if reason not in ("wrong_state", "unauthorized"):
                    say("signer_unclaimed", check=check["id"], reason=reason or outcome, detail=json.dumps(sentence))
                    return
        except Refused as refusal:
            self.unpulled += 1
            if self.unpulled == 1:
                say("signer_unpulled", detail=json.dumps(refusal.sentence))

    def signed(self, check):
        try:
            self.sign(check)
        except Exception as error:
            say("signer_broke", check=check["id"], error=json.dumps(repr(error)))
            shutil.rmtree(self.home / "work" / str(check["id"]), ignore_errors=True)
            self.report(check["id"], "errored", BROKE)

    def report(self, check, state, summary, files=()):
        payloads = []
        if files:
            payloads.append(actions_outputs(check, list(files)).hex())
        payloads.append(actions_report(check, state, summary).hex())
        self.owe(self.owed() + payloads)
        self.claimed.unlink(missing_ok=True)
        self.send_owed()

    def operation(self, check):
        steps = check["job"]["steps"]
        if len(steps) != 1 or steps[0] not in self.tools:
            raise Refused(f"a signing check names one operation, one of {', '.join(sorted(self.tools))}; this one's steps are {steps}")
        operation = steps[0]
        policy = self.repos.get(check["repo"], {}).get(operation)
        if policy is None:
            raise Refused(f"this signer does not {operation} for repo {check['repo']}")
        if isinstance(policy, dict):
            channel = check["job"]["env"].get("CHANNEL")
            if channel not in policy:
                raise Refused(f"{operation} names a channel in CHANNEL, one of {', '.join(sorted(policy))}, not {channel!r}")
            policy = policy[channel]
        return operation, policy

    def reachable(self, check, branches):
        for branch in branches:
            reader = self.asked(FORGE, forge_compare(check["repo"], check["commit"], branch), "Compare", 7)
            reader.u64()
            reader.text(), reader.text(), reader.option(reader.text)
            ahead = reader.u64()
            if ahead == 0:
                return branch
        raise Refused(f"commit {check['commit']} is on none of {check['repo']}'s {', '.join(branches)}")

    def inputs(self, check, root):
        reader = self.asked(ACTIONS, actions_needs(check["id"]), "Needs", 6)
        needs = reader.vec(lambda: (reader.text(), reader.option(reader.u64)))
        if not needs:
            raise Refused("a signing check needs the check that built what it signs")
        taken = []
        for job, met in needs:
            if met is None:
                raise Refused(f"needs `{job}`, which no passed check met")
            reader = self.asked(ACTIONS, actions_check(met), "Check", 0)
            built = reader.option(reader.check)
            same = built is not None and (built["commit"], built["run"]) == (check["commit"], check["run"])
            if not same or built["state"] != "passed":
                raise Refused(f"`{job}`'s check {met} is not a passed check of this run at {check['commit']}")
            if built["reporter"] not in self.builders:
                raise Refused(f"`{job}`'s check {met} was reported by account {built['reporter']}, no builder this signer trusts")
            reader = self.asked(ACTIONS, actions_outputs_of(met), "Outputs", 7)
            row = reader.option(lambda: (reader.u64(), reader.vec(lambda: (reader.text(), reader.blob(), reader.u64()))))
            if row is None or row[0] not in self.builders:
                raise Refused(f"`{job}`'s check {met} has no outputs a trusted builder recorded")
            for path, blob, size in row[1]:
                parts = path.split("/")
                if any(part in ("", ".", "..") for part in parts) or "/" in job:
                    raise Refused(f"`{job}`'s output {path!r} would land outside its folder")
                got = self.network.blob(blob)
                if got.code != 0:
                    raise Refused(f"blob {blob} was not fetched: {failed(got)}")
                if f"sha256:{hashlib.sha256(got.out).hexdigest()}" != blob:
                    raise Refused(f"blob {blob} came back with other bytes than its id names")
                _, _, data = got.out.partition(b"\0")
                if len(data) != size:
                    raise Refused(f"blob {blob} holds {len(data)} bytes, not the {size} its outputs row says")
                target = root.joinpath(job, *parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                taken.append({"job": job, "check": met, "path": path, "blob": blob, "sha256": hashlib.sha256(data).hexdigest()})
        return taken

    def sign(self, check):
        work = self.home / "work" / str(check["id"])
        shutil.rmtree(work, ignore_errors=True)
        try:
            operation, branches = self.operation(check)
            branch = self.reachable(check, branches)
            inputs = self.inputs(check, work / "inputs")
        except Refused as refusal:
            say("signer_refused_check", check=check["id"], detail=json.dumps(refusal.sentence))
            self.report(check["id"], "errored", refusal.sentence)
            shutil.rmtree(work, ignore_errors=True)
            return
        out = work / "out"
        out.mkdir(parents=True)
        env = {name: value for name, value in os.environ.items() if not name.startswith("SIGNER_ENV_")}
        env.update({f"SIGNER_ENV_{name}": value for name, value in check["job"]["env"].items() if ENV_NAME.fullmatch(name)})
        env.update(SIGNER_REPO=check["repo"], SIGNER_COMMIT=check["commit"], SIGNER_BRANCH=branch, SIGNER_CHECK=str(check["id"]))
        tool = run([*self.tools[operation], str(work / "inputs"), str(out)], env=env)
        self.logs.mkdir(parents=True, exist_ok=True)
        (self.logs / f"{check['id']}.log").write_bytes(tool.out + tool.err.encode())
        if tool.code != 0:
            self.report(check["id"], "failed", f"{operation} exited {tool.code}; what it said is in the signer's own log")
            shutil.rmtree(work, ignore_errors=True)
            return
        made = sorted(path for path in out.rglob("*") if path.is_file())
        record = {
            "account": self.account,
            "labels": self.labels,
            "signer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "operation": operation,
            "repo": check["repo"],
            "commit": check["commit"],
            "branch": branch,
            "check": check["id"],
            "inputs": inputs,
            "outputs": [{"path": path.relative_to(out).as_posix(), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()} for path in made],
        }
        (out / "signed.json").write_text(json.dumps(record, indent=2) + "\n")
        made.append(out / "signed.json")
        put = self.network.put(made, OUTPUT)
        ids = [line.strip() for line in put.out.decode(errors="replace").splitlines()[: len(made)]] if put.code == 0 else []
        if len(ids) != len(made) or not all(blob.startswith("sha256:") for blob in ids):
            self.report(check["id"], "errored", f"the signed outputs were not put: {failed(put)}")
        else:
            files = [(path.relative_to(out).as_posix(), blob, path.stat().st_size) for path, blob in zip(made, ids)]
            self.report(check["id"], "passed", f"{operation} for {check['repo']} at {check['commit']} ({branch})", files)
        shutil.rmtree(work, ignore_errors=True)


def main():
    home = Path(os.environ.get("SIGNER_HOME") or Path.home() / "Library/Application Support/ducktape-signer")
    try:
        config = json.loads((home / "config.json").read_text())
        config["labels"], config["builders"], config["repos"], config["tools"]
    except FileNotFoundError as error:
        say("signer_unconfigured", home=str(home), detail=json.dumps(repr(error)))
        return
    except (OSError, ValueError, KeyError, TypeError) as error:
        say("signer_unconfigured", home=str(home), detail=json.dumps(repr(error)))
        sys.exit(1)
    network = Network(shlex.split(os.environ.get("SIGNER_DUCKTAPE", "ducktape")), home / "keys")
    say("signer_serving", labels=",".join(config["labels"]), home=str(home))
    Signer(network, config, home).serve()


if __name__ == "__main__":
    main()
