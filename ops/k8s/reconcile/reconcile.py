#!/usr/bin/env python3
# reconcile: applies what forge repo `infra` holds on `main` to fabrics-1, one duty at a time; today the node's egress services (`egress/<node>.toml`).
# Each open change into `main` gets a dry-run verdict as a commit status and a review comment; each new `main` is applied once, proven, and rolled back when a base stops answering. reconcile-test.py tests it against a fake node.
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request
from dataclasses import dataclass
from pathlib import Path

REPO = "infra"
MAIN = "refs/heads/main"
CONTEXT = "reconcile/egress"
RESERVED = ("http", "relay")
ATTEMPTS_PER_LINE = 10
MODES = ("report", "act")
NOT_FOUND = "not_found"
LEVELS = {"report_unposted": "warn", "unread": "warn"}


class Unread(Exception):
    def __init__(self, detail, reason=None):
        super().__init__(detail)
        self.reason = reason


def refusal_reason(said):
    found = re.search(r"\brefused: ([a-z_]+):", said)
    return found.group(1) if found else None


class Refused(Exception):
    pass


def say(event, **fields):
    print(json.dumps({"level": LEVELS.get(event, "info"), "event": event, **fields}, sort_keys=True), flush=True)


def worth_a_line(attempts):
    return attempts == 1 or attempts % ATTEMPTS_PER_LINE == 0


def hexed(text):
    return text.encode().hex()


def egress_path(node):
    return f"egress/{node}.toml"


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

    def run(self, *args):
        argv = [*self.ducktape, "--workspace", str(self.workspace), "--node", self.node, *args]
        return self.runner(argv, capture_output=True, env=self.env())

    def call(self, *args):
        done = self.run(*args)
        if done.returncode != 0:
            said = done.stderr.decode(errors="replace").strip() or f"ducktape exited {done.returncode}"
            raise Unread(said, refusal_reason(said))
        return json.loads(done.stdout)

    def query(self, program, query):
        return self.call("query", program, json.dumps(query))

    def op(self, program, op):
        return self.call("op", program, json.dumps(op), "--land")

    def egress_set(self, text, dry):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "egress.toml"
            path.write_text(text)
            done = self.run("egress", "set", str(path), *(["--dry"] if dry else []))
        if done.returncode != 0:
            raise Refused(done.stderr.decode(errors="replace").strip() or f"ducktape exited {done.returncode}")
        return json.loads(done.stdout)


class Forge:
    def __init__(self, cli):
        self.cli = cli

    def tip(self):
        query = {"Log": {"repo": REPO, "from": {"Ref": hexed(MAIN)}, "exclude": None, "page": {"after": None, "limit": 1}}}
        return self.cli.query("forge", query)["Log"]["tip"]

    def entry(self, commit, path):
        folder, _, name = path.rpartition("/")
        page = {"after": None, "limit": None}
        while True:
            query = {"Tree": {"repo": REPO, "at": commit, "path": hexed(folder), "page": page}}
            try:
                listed = self.cli.query("forge", query)["Tree"]["page"]
            except Unread as unread:
                if unread.reason == NOT_FOUND:
                    return None
                raise
            for item in listed["items"]:
                if bytes.fromhex(item["name"]) == name.encode():
                    return item["oid"]
            if listed["next"] is None:
                return None
            page = {"after": listed["next"], "limit": None}

    def file(self, commit, path):
        oid = self.entry(commit, path)
        if oid is None:
            return None
        read = bytearray()
        size = 0
        while True:
            rest = {"offset": len(read), "len": size - len(read)} if read else None
            blob = self.cli.query("forge", {"Blob": {"repo": REPO, "oid": oid, "range": rest}})["Blob"]["blob"]
            size = blob["size"]
            read.extend(bytes.fromhex(blob["bytes"]))
            if len(read) >= size or not blob["bytes"]:
                return read.decode(errors="surrogateescape")

    def open_changes(self):
        page = {"after": None, "limit": None}
        found = []
        while True:
            query = {"Changes": {"repo": REPO, "filter": {"author": None, "involves": None, "state": "Open"}, "page": page}}
            listed = self.cli.query("forge", query)["Changes"]["page"]
            found.extend(item["n"] for item in listed["items"] if item["into"] == hexed(MAIN))
            if listed["next"] is None:
                return found
            page = {"after": listed["next"], "limit": None}

    def head(self, n):
        query = {"Change": {"repo": REPO, "n": n, "page": {"after": None, "limit": 1}}}
        return self.cli.query("forge", query)["Change"]["source_head"]

    def judge(self, n, head, passed, body):
        state = "Passed" if passed else "Failed"
        self.cli.op("forge", {"Status": {"repo": REPO, "commit": head, "context": CONTEXT, "state": state, "url": None}})
        review = {"base_oid": None, "body": body, "comments": [], "commit_oid": head, "verdict": "Comment"}
        self.cli.op("forge", {"ReviewSubmit": {"repo": REPO, "n": n, "review": review}})


def misshapen(document):
    services = document.get("services", {})
    if not isinstance(services, dict):
        return ["the file does not parse: services is not a table"]
    odd = [name for name, service in sorted(services.items()) if not isinstance(service, dict) or not isinstance(service.get("secrets", {}), dict)]
    return [f"the file does not parse: services.{name} is not a table with a secrets table" for name in odd]


def secreted(document):
    services = sorted(document.get("services", {}).items())
    return [f"{name} carries secrets, which the node never takes through egress set" for name, service in services if service.get("secrets")]


def served(document):
    own = [name for name in RESERVED if document.get(name) is True]
    return sorted([*own, *document.get("services", {})])


@dataclass(frozen=True)
class Plan:
    problems: tuple
    dry: dict | None


def utf8(text):
    try:
        text.encode()
    except UnicodeEncodeError:
        return False
    return True


def plan(text, cli):
    if not utf8(text):
        return Plan(("the file is not UTF-8",), None)
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        return Plan((f"the file does not parse: {error}",), None)
    problems = misshapen(document) or secreted(document)
    if problems:
        return Plan(tuple(problems), None)
    try:
        dry = cli.egress_set(text, dry=True)
    except Refused as refused:
        return Plan((f"the node refuses it: {refused}",), None)
    unreached = [f"{name}'s base does not answer from the node: {said}" for name, said in sorted(dry["reach"].items()) if not said.startswith("answered")]
    return Plan(tuple(unreached), dry)


def tally_line(tally):
    return f"{tally['served']} served: {tally['added']} added, {tally['removed']} removed, {tally['changed']} changed"


def verdict_body(found):
    if found.problems:
        return "\n".join(["reconcile/egress: FAIL", *[f"- {problem}" for problem in found.problems]])
    reach = [f"- {name}: {said}" for name, said in sorted(found.dry["reach"].items())]
    return "\n".join(["reconcile/egress: PASS (dry run on the node, nothing applied)", tally_line(found.dry["tally"]), *reach])


def fresh_state():
    return {"seen_tip": None, "main_text": None, "applied": None, "proving": None, "judged": [], "outbox": []}


class Reconcile:
    def __init__(self, mode, node_name, node_key, channel, cli, forge, store):
        if mode not in MODES:
            raise ValueError(f"RECONCILE_MODE is report or act, not {mode}")
        self.mode = mode
        self.path = egress_path(node_name)
        self.node_key = node_key
        self.channel = channel
        self.cli = cli
        self.forge = forge
        self.store = store
        self.state = store.load()
        self.unread = 0

    def report(self, key, lines):
        say("report", key=key, lines=lines)
        self.state["outbox"].append({"id": f"reconcile-{key}", "text": "\n".join(lines), "tried": 0})

    def judge_changes(self):
        main_text = self.state["main_text"]
        for n in self.forge.open_changes():
            head = self.forge.head(n)
            key = f"{n}:{head}"
            if head is None or key in self.state["judged"]:
                continue
            text = self.forge.file(head, self.path)
            if text is not None and text != main_text:
                found = plan(text, self.cli)
                self.forge.judge(n, head, not found.problems, verdict_body(found))
                say("judged", change=n, head=head, passed=not found.problems)
            self.state["judged"].append(key)

    def apply(self, tip, text):
        short = tip[:12]
        found = plan(text, self.cli)
        if found.problems:
            return self.report(f"refused-{short}", [f"infra main {short}: egress not applied, the node keeps what it serves.", *found.problems])
        if not any(found.dry["tally"][part] for part in ("added", "removed", "changed")):
            self.state["applied"] = {"commit": tip, "text": text}
            return say("unchanged", commit=tip)
        if self.mode == "report":
            return self.report(f"would-apply-{short}", [f"infra main {short}: would apply egress ({tally_line(found.dry['tally'])}). Report mode, so the node is unchanged."])
        try:
            tally = self.cli.egress_set(text, dry=False)["tally"]
        except Refused as refused:
            return self.report(f"refused-{short}", [f"infra main {short}: the node refused egress, and keeps what it serves: {refused}"])
        before = self.state["applied"]
        self.state["applied"] = {"commit": tip, "text": text}
        after = plan(text, self.cli)
        if after.problems and before is not None:
            return self.roll_back(short, before, after.problems)
        self.state["proving"] = {"commit": tip, "served": served(tomllib.loads(text))}
        self.report(f"applied-{short}", [f"infra main {short}: egress applied ({tally_line(tally)}). Proven once the chain's record of what the node serves matches."])

    def roll_back(self, short, before, problems):
        try:
            self.cli.egress_set(before["text"], dry=False)
        except Refused as refused:
            return self.report(f"stuck-{short}", [f"infra main {short}: applied, then a base stopped answering, and the rollback to {before['commit'][:12]} was refused: {refused}", *problems])
        self.state["applied"] = before
        self.report(f"rolled-back-{short}", [f"infra main {short}: applied, then a base stopped answering, so the node serves {before['commit'][:12]} again. Revert {short} in infra.", *problems])

    def prove(self):
        proving = self.state["proving"]
        if proving is None:
            return
        row = self.cli.query("egress", {"Node": {"key": self.node_key}})["Node"]
        if row is None or sorted(row["services"]) != proving["served"]:
            return
        self.state["proving"] = None
        self.report(f"proven-{proving['commit'][:12]}", [f"infra main {proving['commit'][:12]}: proven, the chain records the node serving {', '.join(proving['served'])}."])

    def follow_main(self):
        tip = self.forge.tip()
        if tip == self.state["seen_tip"]:
            return
        text = self.forge.file(tip, self.path)
        self.state["seen_tip"] = tip
        self.state["main_text"] = text
        if text is None:
            return self.report(f"absent-{tip[:12]}", [f"infra main {tip[:12]} holds no {self.path}; egress is left as it is."])
        self.apply(tip, text)

    def flush(self):
        left = []
        for item in self.state["outbox"]:
            item = {**item, "tried": item["tried"] + 1}
            try:
                landed = self.cli.query("chat", {"MessageById": {"message_id": item["id"]}}).get("Message") is not None
                if not landed:
                    paragraph = {"Paragraph": [{"text": item["text"], "marks": []}]}
                    self.cli.op("chat", {"PostMessage": {"channel_id": self.channel, "message_id": item["id"], "blocks": [paragraph], "thread": None}})
            except Unread as error:
                if worth_a_line(item["tried"]):
                    say("report_unposted", id=item["id"], attempts=item["tried"], detail=str(error))
                left.append(item)
        self.state["outbox"] = left

    def tick(self):
        try:
            self.follow_main()
            self.prove()
            self.judge_changes()
        except Unread as error:
            self.unread += 1
            if worth_a_line(self.unread):
                say("unread", attempts=self.unread, detail=str(error))
        else:
            if self.unread:
                say("read", attempts=self.unread)
            self.unread = 0
        self.store.save(self.state)
        self.flush()
        self.store.save(self.state)


class Store:
    def __init__(self, path):
        self.path = path

    def load(self):
        return json.loads(self.path.read_text()) if self.path.exists() else fresh_state()

    def save(self, state):
        temp = self.path.with_suffix(".tmp")
        temp.write_text(json.dumps(state, sort_keys=True))
        temp.replace(self.path)


def node_key(status):
    size = int.from_bytes(status[:4], "little")
    at = 4 + size + 32 + 64 + 8
    length = int.from_bytes(status[at : at + 4], "little")
    return status[at + 4 : at + 4 + length].hex()


def block_time_s(status):
    size = int.from_bytes(status[:4], "little")
    at = 4 + size + 8
    return int.from_bytes(status[at : at + 8], "little") / 1000


def read_status(node):
    request = urllib.request.Request(node + "/v1/status", headers={"User-Agent": "reconcile"})
    with urllib.request.urlopen(request) as answer:
        return answer.read()


def main():
    env = os.environ
    if sys.argv[1:]:
        sys.exit(f"usage: reconcile.py, not {' '.join(sys.argv[1:])}")
    node = env["RECONCILE_NODE"].rstrip("/")
    workspace = Path(env.get("RECONCILE_WORKSPACE") or tempfile.mkdtemp())
    workspace.mkdir(parents=True, exist_ok=True)
    cli = Cli(shlex.split(env.get("RECONCILE_DUCKTAPE", "ducktape")), node, workspace, Path(env.get("RECONCILE_KEYS", "/keys")))
    store = Store(Path(env.get("RECONCILE_STATE", "/state")) / "state.json")
    status = read_status(node)
    reconcile = Reconcile(env["RECONCILE_MODE"], env["RECONCILE_NODE_NAME"], node_key(status), env["RECONCILE_CHANNEL"], cli, Forge(cli), store)
    say("reconciling", mode=reconcile.mode, path=reconcile.path, channel=reconcile.channel)
    while True:
        reconcile.tick()
        time.sleep(block_time_s(status))


if __name__ == "__main__":
    main()
