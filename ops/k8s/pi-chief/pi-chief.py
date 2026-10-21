#!/usr/bin/env python3
# pi-chief: duckpond's recovery daemon, off the chain. It rolls a program that fills no role back to the code it ran before a change after which it failed, or the node back to the image it ran before an image change after which the chain stopped, and reports everything else to orthory (15) and chief (17) in DMs.
# ops/k8s/pi-chief/pi-chief.yaml runs it on fabrics-1 and pi-chief-test.py tests it; `dry-run node` and `dry-run program PROGRAM CODE` show a rollback without making it.
import copy
import hashlib
import json
import os
import re
import shlex
import ssl
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
from urllib.parse import urlencode

PROGRAMS = "programs"
FORGE = "forge"
NOTICE_FOLDER = b"crates/system/programs/src"
NOTICE_FILE = b"lib.rs"
NOTICE_LINE = re.compile(r"^pub const NOTICE: u64 = ([0-9]+);$", re.M)
CHAT = "chat"
IDENTITY = "identity"
HARNESS = "harness"
HOST_FAULTS = ("trap", "protocol")
HASHES = {"sha256": (0, 32), "sha1": (1, 20)}
ATTEMPTS_PER_LINE = 10
MANAGER = "pi-chief"
SERVICE_ACCOUNT = Path("/var/run/secrets/kubernetes.io/serviceaccount")
LEVELS = {"unreached": "warn", "report_unposted": "warn", "kube_unread": "warn", "stopped": "error"}
CHANGES = ("set", "remove", "set_view", "remove_view", "set_hook", "remove_hook", "publish", "rename")


class Unreached(Exception):
    pass


class Failed(Exception):
    pass


def say(event, **fields):
    print(json.dumps({"level": LEVELS.get(event, "info"), "event": event, **fields}, sort_keys=True), flush=True)


def worth_a_line(attempts):
    return attempts == 1 or attempts % ATTEMPTS_PER_LINE == 0


@dataclass(frozen=True)
class Ran:
    code: int
    out: bytes
    err: str


def run(argv, stdin=None, env=None):
    done = subprocess.run(argv, input=stdin, capture_output=True, env=env)
    return Ran(done.returncode, done.stdout, done.stderr.decode(errors="replace").strip())


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
    index, size = HASHES[kind]
    data = bytes.fromhex(digest)
    if len(data) != size:
        raise ValueError(f"{text} is not a {kind} blob id")
    return u8(index) + data


class Reader:
    def __init__(self, data):
        self.data = data
        self.at = 0

    def take(self, count):
        piece = self.data[self.at : self.at + count]
        if len(piece) != count:
            raise Failed("a reply ended early")
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
        return read() if self.u8() == 1 else None

    def vec(self, read):
        return [read() for _ in range(self.u32())]

    def blob(self):
        tag = self.u8()
        for name, (index, size) in HASHES.items():
            if index == tag:
                return f"{name}:{self.take(size).hex()}"
        raise Failed(f"a reply names hash kind {tag}")

    def tagged(self, wanted, what):
        tag = self.u8()
        if tag != wanted:
            raise Failed(f"{what} came back as variant {tag}")


@dataclass(frozen=True)
class Status:
    network: str
    block_time_ms: int
    epoch_length: int
    height: int
    roles: dict
    contract: int


@dataclass(frozen=True)
class Outcome:
    program: str
    applied: bool
    reason: str


@dataclass(frozen=True)
class Tx:
    position: int
    signer: bytes
    target: str
    size: int
    payload: bytes | None
    outcomes: tuple


@dataclass(frozen=True)
class Block:
    height: int
    time: int
    txs: tuple


@dataclass(frozen=True)
class Change:
    verb: str
    program: str
    code: str | None


@dataclass(frozen=True)
class Pending:
    height: int
    change: Change
    by: int | None


def status_of(data):
    reader = Reader(data)
    network = reader.text()
    reader.u64()
    block_time_ms = reader.u64()
    epoch_length = reader.u64()
    height = reader.u64()
    reader.take(32)
    reader.take(32)
    reader.u64()
    reader.bytes()
    contract = reader.u32()
    reader.take(32)
    roles = dict(reader.vec(lambda: (reader.text(), reader.text())))
    return Status(network, block_time_ms, epoch_length, height, roles, contract)


def table_of(data):
    reader = Reader(data)
    return dict(reader.vec(lambda: (reader.text(), reader.blob())))


def outcomes_of(reader):
    program = reader.text()
    applied = reader.u8() == 0
    reason = ""
    if applied:
        reader.bytes()
    else:
        reason = reader.text()
        reader.text()
    reader.vec(reader.bytes)
    nested = reader.vec(lambda: outcomes_of(reader))
    return (Outcome(program, applied, reason), *[outcome for inner in nested for outcome in inner])


def tx_of(reader, position):
    reader.take(32)
    signer = reader.bytes()
    reader.u64()
    reader.take(32)
    target = reader.text()
    size = reader.u32()
    payload = reader.option(reader.bytes)
    outcomes = reader.option(lambda: outcomes_of(reader)) or ()
    return Tx(position, signer, target, size, payload, outcomes)


def block_of(reader):
    height = reader.u64()
    reader.take(32)
    reader.take(32)
    time_ms = reader.u64()
    reader.u64()
    reader.option(reader.bytes)
    txs = tuple(tx_of(reader, position) for position in range(reader.u32()))
    return Block(height, time_ms, txs)


def page_of(data):
    reader = Reader(data)
    blocks = reader.vec(lambda: block_of(reader))
    listed_from = reader.option(reader.u64)
    return blocks, listed_from


def located_of(data):
    reader = Reader(data)

    def located():
        reader.u64()
        position = reader.u32()
        reader.u64()
        return tx_of(reader, position)

    return reader.option(located)


def blocks_ask(before, limit, most):
    return borsh_option(before, u64) + u32(limit) + u32(most)


def tx_ask(height, position, most):
    return u8(0) + u64(height) + u32(position) + u32(most)


def change_of(reader):
    tag = reader.u8()
    verb = CHANGES[tag]
    match verb:
        case "set":
            program, code = reader.text(), reader.blob()
            reader.bytes()
            return Change(verb, program, code)
        case "publish":
            program, code = reader.text(), reader.blob()
            reader.bytes()
            reader.vec(reader.text)
            return Change(verb, program, code)
        case "set_view":
            name, view = reader.text(), reader.blob()
            return Change(verb, name, view)
        case "remove" | "remove_view" | "set_hook" | "remove_hook":
            return Change(verb, reader.text(), None)
        case "rename":
            program = reader.text()
            reader.text()
            return Change(verb, program, None)


def schedule_of(payload):
    reader = Reader(payload)
    if reader.u8() not in (0, 3):
        return None
    height = reader.u64()
    return Pending(height, change_of(reader), None)


def scheduled_query(after=None):
    return u8(1) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def scheduled_page(reply):
    reader = Reader(reply)
    reader.tagged(1, "programs' Scheduled")
    reader.u64()
    items = reader.vec(lambda: Pending(reader.u64(), change_of(reader), reader.option(reader.u64)))
    return items, reader.option(reader.bytes)


def scheduled_of(reply):
    return scheduled_page(reply)[0]


def scheduler_key(program, code):
    return f"{program} {code}"


def program_query(program):
    return u8(2) + borsh_text(program)


def entry_of(reply):
    reader = Reader(reply)
    reader.tagged(2, "programs' Program")
    reader.u64()
    return reader.option(lambda: (reader.text(), reader.blob(), reader.bytes()))


def sources_query():
    return u8(7)


def sources_of(reply):
    reader = Reader(reply)
    reader.tagged(7, "programs' Sources")
    reader.u64()

    def built():
        program = reader.text()
        code = reader.blob()
        match reader.u8():
            case 0:
                return program, code, (reader.text(), reader.text())
            case 1:
                return program, code, None
            case 2:
                reader.text()
                return program, code, None
            case tag:
                raise Failed(f"programs' Sources names provenance {tag}")

    return {program: (code, source) for program, code, source in reader.vec(built)}


def tree_query(repo, at, path, after):
    return u8(6) + borsh_text(repo) + borsh_text(at) + borsh_bytes(path) + borsh_option(after, borsh_bytes) + borsh_option(None, u64)


def tree_of(reply):
    reader = Reader(reply)
    reader.tagged(4, "forge's Tree")
    reader.u64()
    reader.text()
    reader.u64()
    entries = reader.vec(lambda: (reader.bytes(), reader.text(), reader.u8()))
    return entries, reader.option(reader.bytes)


def blob_query(repo, oid):
    return u8(7) + borsh_text(repo) + borsh_text(oid) + borsh_option(None, u64)


def blob_of(reply):
    reader = Reader(reply)
    reader.tagged(5, "forge's Blob")
    reader.u64()
    reader.text()
    size = reader.u64()
    reader.u8()
    reader.u64()
    reader.u64()
    data = reader.bytes()
    if len(data) != size:
        raise Failed(f"forge answered {len(data)} of the file's {size} bytes")
    return data


def schedule_over_set(height, program, code, params, expected):
    return u8(3) + u64(height) + u8(0) + borsh_text(program) + blob_id(code) + borsh_bytes(params) + borsh_option(expected, blob_id)


def dm_open(counterpart, name):
    return u8(1) + u64(counterpart) + borsh_text(name)


def dm_channel(a, b):
    return f"dm-{min(a, b)}-{max(a, b)}"


def post_message(channel, message_id, lines):
    paragraphs = [u8(0) + borsh_vec([borsh_text(line) + borsh_vec([])]) for line in lines]
    return u8(4) + borsh_text(channel) + borsh_text(message_id) + borsh_vec(paragraphs) + borsh_option(None, u64)


def message_query(message_id):
    return u8(2) + borsh_text(message_id)


def seq_of(reply):
    reader = Reader(reply)
    reader.tagged(2, "chat's Message")
    found = reader.option(lambda: (reader.text(), reader.u64()))
    return None if found is None else found[1]


def reactors_query(channel, seqs):
    return u8(11) + borsh_text(channel) + borsh_vec([u64(seq) for seq in seqs])


def reactors_of(reply):
    reader = Reader(reply)
    reader.tagged(11, "chat's Reactors")

    def principal():
        return reader.u64() if reader.u8() == 0 else None

    rows = reader.vec(lambda: (reader.u64(), reader.text(), reader.vec(principal)))
    return {account for _, _, by in rows for account in by if account is not None}


def of_key_query(key):
    return u8(0) + borsh_bytes(key)


def number_of(reply):
    reader = Reader(reply)
    reader.tagged(0, "identity's Number")
    return reader.option(reader.u64)


def account_query(number):
    return u8(5) + u64(number)


def manager_of(reply):
    reader = Reader(reply)
    reader.tagged(3, "identity's Account")

    def manager():
        reader.u64()
        reader.text()
        reader.option(reader.blob)
        reader.option(reader.text)
        reader.u64()
        return reader.u64() if reader.u8() == 1 else None

    return reader.option(manager)


def receipt(ran):
    text = ran.out.decode(errors="replace").strip()
    pooled = re.match(r"pooled ([0-9a-f]+)\n?", text)
    verdict = text[pooled.end():] if pooled else text
    applied = re.match(r"landed (\d+) applied ?([0-9a-f]*)", verdict)
    rejected = re.match(r"landed (\d+) rejected (\w+): (.*)", verdict, re.S)
    unlanded = re.fullmatch(r"unlanded", verdict)
    if ran.code == 0 and applied:
        return "applied", applied.group(2), "", int(applied.group(1))
    if ran.code == 0 and rejected:
        return "rejected", rejected.group(2), rejected.group(3).strip(), int(rejected.group(1))
    if ran.code == 0 and unlanded:
        return "unlanded", "", "no block carried the frame before its anchor aged out", None
    return "unknown", "", ran.err or text or f"ducktape exited {ran.code}", None


def short(code_or_image):
    digest = code_or_image.rsplit(":", 1)[-1]
    return re.sub(r"[^a-z0-9]", "", digest.lower())[:12]


@dataclass(frozen=True)
class Rollout:
    image: str
    complete: bool
    deadline_exceeded: bool
    deadline_s: int
    resource_version: str


def container_image(template, container):
    images = [held["image"] for held in template["spec"]["containers"] if held["name"] == container]
    return images[0] if images else None


def rollout_of(deployment, container):
    spec = deployment["spec"]
    status = deployment.get("status", {})
    replicas = spec.get("replicas", 1)
    observed = status.get("observedGeneration", 0) >= deployment["metadata"]["generation"]
    counts = (status.get("replicas", 0), status.get("updatedReplicas", 0), status.get("availableReplicas", 0))
    complete = observed and counts == (replicas, replicas, replicas)
    exceeded = any(
        held["type"] == "Progressing" and held["status"] == "False" and held.get("reason") == "ProgressDeadlineExceeded"
        for held in status.get("conditions", [])
    )
    image = container_image(spec["template"], container)
    if image is None:
        raise Failed(f"deployment {deployment['metadata']['name']} has no container {container}")
    return Rollout(image, complete, exceeded, spec["progressDeadlineSeconds"], deployment["metadata"]["resourceVersion"])


def revision(held):
    return int(held["metadata"].get("annotations", {}).get("deployment.kubernetes.io/revision", "0"))


def image_before(deployment, replicasets, container):
    uid = deployment["metadata"]["uid"]
    current = container_image(deployment["spec"]["template"], container)
    owned = [held for held in replicasets if any(owner.get("uid") == uid for owner in held["metadata"].get("ownerReferences", []))]
    older = sorted((held for held in owned if revision(held) < revision(deployment)), key=revision, reverse=True)
    for held in older:
        image = container_image(held["spec"]["template"], container)
        if image is not None and image != current:
            return image
    return None


def image_patch(container, image, resource_version):
    return {"metadata": {"resourceVersion": resource_version}, "spec": {"template": {"spec": {"containers": [{"name": container, "image": image}]}}}}


def differences(before, after, path=""):
    if isinstance(before, dict) and isinstance(after, dict):
        keys = sorted(set(before) | set(after))
        return [found for key in keys for found in differences(before.get(key), after.get(key), f"{path}.{key}" if path else key)]
    if isinstance(before, list) and isinstance(after, list) and len(before) == len(after):
        return [found for index, pair in enumerate(zip(before, after)) for found in differences(*pair, f"{path}.{index}")]
    return [] if before == after else [path]


def compared(held):
    meta = held["metadata"]
    return {"spec": held["spec"], "labels": meta.get("labels", {}), "annotations": meta.get("annotations", {})}


def changed_paths(live, dry):
    return differences(compared(live), compared(dry))


def image_path(deployment, container):
    names = [held["name"] for held in deployment["spec"]["template"]["spec"]["containers"]]
    return f"spec.template.spec.containers.{names.index(container)}.image"


@dataclass(frozen=True)
class Table:
    programs: dict
    low: int
    high: int


@dataclass(frozen=True)
class Seen:
    now: int
    status: Status | None
    blocks: tuple
    table: Table | None
    rollout: Rollout | None
    scheduled: tuple | None


@dataclass(frozen=True)
class NodeFailed:
    key: str
    image: str
    previous: str
    cause: str
    height: int
    stalled_s: int
    contract: int | None
    previous_contract: int | None


@dataclass(frozen=True)
class ProgramFailed:
    key: str
    program: str
    code: str
    previous: str
    since: int
    failures: int


@dataclass(frozen=True)
class HaltedAtSwitch:
    key: str
    program: str
    height: int
    stalled_s: int


@dataclass(frozen=True)
class Stalled:
    key: str
    height: int
    stalled_s: int


def fresh_state():
    return {"reported": [], "outbox": [], "programs": {}, "own_programs": {}, "own_image": None, "held": None, "schedulers": None}


def stalled_for(state, now):
    tip = state.get("tip")
    deadline = state.get("deadline_s")
    if tip is None or deadline is None:
        return None
    waited = (now - tip["time"]) // 1000
    return waited if waited >= deadline else None


def track_programs(state, table):
    records = state["programs"]
    schedulers = state.get("schedulers") or {}
    for program in list(records):
        if program not in table.programs:
            del records[program]
    for program, code in table.programs.items():
        record = records.get(program)
        if record is None:
            records[program] = {"code": code, "since": table.high, "seen_at": table.low, "applied": 0, "failed": 0, "before": None, "own": False, "scheduler": None}
            continue
        if record["code"] == code:
            record["seen_at"] = table.low
            continue
        own = state["own_programs"].pop(program, None) == code
        before = {"code": record["code"], "until": record["seen_at"], "applied": record["applied"] > 0}
        scheduled = schedulers.pop(scheduler_key(program, code), None)
        records[program] = {"code": code, "since": table.high, "seen_at": table.low, "applied": 0, "failed": 0, "before": before, "own": own, "scheduler": None, "scheduled": scheduled}
        say("program_switched", program=program, before=record["code"], code=code, since=table.high, own=own)
    for key, held in list(schedulers.items()):
        if held["height"] <= table.low:
            del schedulers[key]


def track_schedulers(state, scheduled):
    schedulers = state.get("schedulers") or {}
    for pending in scheduled:
        if pending.change.verb in ("set", "publish"):
            schedulers[scheduler_key(pending.change.program, pending.change.code)] = {"account": pending.by, "height": pending.height}
    state["schedulers"] = schedulers


def count_receipts(state, blocks):
    records = state["programs"]
    for block in blocks:
        for tx in block.txs:
            for outcome in tx.outcomes:
                record = records.get(outcome.program)
                if record is None:
                    continue
                runs_current = block.height >= record["since"]
                ran_before = record["before"] is not None and block.height <= record["before"]["until"]
                if runs_current and outcome.applied:
                    record["applied"] += 1
                if runs_current and outcome.reason in HOST_FAULTS:
                    record["failed"] += 1
                if ran_before and outcome.applied:
                    record["before"]["applied"] = True


def track_node(state, rollout, stalled, contract):
    node = state.get("node")
    if node is None:
        node = {"image": rollout.image, "proven": True, "previous": None, "complete_at": None, "came_in_stall": False, "own": False, "contract": None, "previous_contract": None}
        state["node"] = node
    if rollout.image != node["image"]:
        last_proven = node["image"] if node["proven"] else node["previous"]
        last_contract = node.get("contract") if node["proven"] else node.get("previous_contract")
        own = state.get("own_image") == rollout.image
        state["own_image"] = None
        node = {"image": rollout.image, "proven": False, "previous": last_proven, "complete_at": None, "came_in_stall": stalled is not None, "own": own, "contract": None, "previous_contract": last_contract}
        state["node"] = node
        say("node_image_changed", image=rollout.image, previous=last_proven, own=own, came_in_stall=stalled is not None)
    if rollout.complete and contract is not None and node.get("contract") is None:
        node["contract"] = contract
        say("node_contract_seen", image=node["image"], contract=contract)
    tip = state.get("tip")
    if node["proven"] or tip is None:
        return
    if rollout.complete and node["complete_at"] is None:
        node["complete_at"] = tip["height"]
    proven = node["complete_at"] is not None and tip["height"] >= node["complete_at"] + state["chain"]["epoch_length"]
    if proven:
        node["proven"] = True
        say("node_image_proven", image=node["image"], height=tip["height"])


def node_watched(state):
    node = state.get("node")
    return node is not None and not node["proven"] and not node["own"] and not node["came_in_stall"] and node["previous"] is not None


def switch_at_tip(state):
    tip = state["tip"]["height"]
    switched = [program for program, record in sorted(state["programs"].items()) if record["before"] is not None and record["since"] == tip]
    due = state.get("due") or {}
    landing = due.get("programs", []) if due.get("at") == tip else []
    return (switched + landing + [None])[0]


def stall_finding(state, stalled):
    tip = state["tip"]["height"]
    program = switch_at_tip(state)
    if program is not None:
        return HaltedAtSwitch(f"halt-{tip}", program, tip, stalled)
    if node_watched(state):
        node = state["node"]
        return NodeFailed(f"node-{short(node['image'])}", node["image"], node["previous"], "stall", tip, stalled, node.get("contract"), node.get("previous_contract"))
    return Stalled(f"stall-{tip}", tip, stalled)


def program_findings(state):
    found = []
    for program, record in sorted(state["programs"].items()):
        failing = record["before"] is not None and not record["own"] and record["applied"] == 0 and record["failed"] > 0
        if failing:
            key = f"program-{program}-{short(record['code'])}"
            found.append(ProgramFailed(key, program, record["code"], record["before"]["code"], record["since"], record["failed"]))
    return found


def judge(state, seen, stalled):
    found = []
    deadline_passed = seen.rollout is not None and seen.rollout.deadline_exceeded and node_watched(state)
    if stalled is not None:
        found.append(stall_finding(state, stalled))
    elif deadline_passed:
        node = state["node"]
        height = state["tip"]["height"] if state.get("tip") else 0
        found.append(NodeFailed(f"node-{short(node['image'])}", node["image"], node["previous"], "deadline", height, 0, node.get("contract"), node.get("previous_contract")))
    found.extend(program_findings(state))
    return [finding for finding in found if finding.key not in state["reported"]]


def decide(state, seen):
    state = copy.deepcopy(state)
    previous_tip = (state.get("tip") or {}).get("height")
    if seen.status is not None:
        state["chain"] = {"block_time_ms": seen.status.block_time_ms, "epoch_length": seen.status.epoch_length, "roles": seen.status.roles}
    if seen.rollout is not None:
        state["deadline_s"] = seen.rollout.deadline_s
    if seen.table is not None:
        track_programs(state, seen.table)
    if seen.blocks:
        newest = seen.blocks[-1]
        state["tip"] = {"height": newest.height, "time": newest.time}
        state["done"] = newest.height
        count_receipts(state, seen.blocks)
    state["advanced"] = previous_tip is not None and (state.get("tip") or {}).get("height", previous_tip) > previous_tip
    stalled = stalled_for(state, seen.now)
    if seen.rollout is not None:
        track_node(state, seen.rollout, stalled, None if seen.status is None else seen.status.contract)
    if seen.scheduled is not None:
        track_schedulers(state, seen.scheduled)
    if seen.scheduled is not None and stalled is not None:
        tip = state["tip"]["height"]
        state["due"] = {"at": tip, "programs": sorted({pending.change.program for pending in seen.scheduled if pending.height == tip + 1})}
    return state, judge(state, seen, stalled)


@dataclass(frozen=True)
class Held:
    key: str


@dataclass(frozen=True)
class Refused:
    reason: str
    detail: str


@dataclass(frozen=True)
class Waiting:
    reason: str


@dataclass(frozen=True)
class Would:
    evidence: dict


@dataclass(frozen=True)
class Acted:
    evidence: dict


@dataclass(frozen=True)
class Unknown:
    detail: str


def program_refusal(finding, record, roles):
    if finding.program in roles.values():
        return Refused("role", f"{finding.program} fills a role the kernel calls, so only the validators' vote changes it")
    if not record["before"]["applied"]:
        return Refused("never_applied", f"pi-chief never saw code {finding.previous} apply an op of {finding.program}, so it cannot say that code worked")
    return None


def scheduler_refusal(finding, scheduler, hands_off):
    if scheduler.get("vote"):
        return Refused("vote", f"programs listed the change of {finding.program} to {finding.code} at height {scheduler['height']} as the validators' vote's, so pi-chief leaves it to the vote")
    if scheduler["account"] is None:
        return Refused("scheduler_unknown", f"pi-chief saw no change of {finding.program} to {finding.code} waiting in programs' Scheduled and found no Schedule or ScheduleOver that set it in the blocks before the switch, so it does not know who made the change")
    hands_off_change = hands_off in (scheduler["account"], scheduler["manager"])
    if hands_off_change:
        return Refused("hands_off", f"account {scheduler['account']} (managed by {scheduler['manager']}) scheduled the change at height {scheduler['height']}, and changes by account {hands_off} or its agents are theirs to undo")
    return None


def finding_lines(finding):
    match finding:
        case NodeFailed(cause="stall"):
            return [f"The node Deployment's image changed from {finding.previous} to {finding.image}, and then the chain made no block for {finding.stalled_s} s after height {finding.height} (the Deployment's progressDeadlineSeconds)."]
        case NodeFailed(cause="deadline"):
            return [f"The node Deployment's image changed from {finding.previous} to {finding.image}, and Kubernetes reports the rollout passed its progress deadline (ProgressDeadlineExceeded)."]
        case ProgramFailed():
            return [f"{finding.program} switched from code {finding.previous} to {finding.code} at height {finding.since}; since then the host refused {finding.failures} of its receipts as {' or '.join(HOST_FAULTS)} and applied none, while the old code applied ops before."]
        case HaltedAtSwitch():
            return [
                f"The chain has made no block for {finding.stalled_s} s and stopped at height {finding.height}, right at a change of {finding.program}.",
                "Neither rollback helps: no block can carry a program change, and a node rollback does not change a program's code. pi-chief took no action.",
            ]
        case Stalled():
            return [f"The chain has made no block for {finding.stalled_s} s after height {finding.height}. pi-chief saw no node image or program change it can name before it, so it took no action."]


def verdict_lines(verdict, action):
    match verdict:
        case Held(key):
            return [f"pi-chief would {action}, but it is held: it takes no action until orthory (15) or chief (17) reacts to its report of {key}."]
        case Refused(reason, detail):
            return [f"pi-chief does not {action}: {detail} ({reason})."]
        case Would(evidence):
            return [f"pi-chief would {action}. Actions are off (report mode), so nothing changed.", f"Dry run: {json.dumps(evidence, sort_keys=True)}"]
        case Acted(evidence):
            return [
                f"pi-chief did {action}: {json.dumps(evidence, sort_keys=True)}",
                "It takes no further action until orthory (15) or chief (17) reacts to this message.",
            ]
        case Unknown(detail):
            return [
                f"pi-chief tried to {action}, but the outcome is unknown, so it does not try again: {detail}",
                "It takes no further action until orthory (15) or chief (17) reacts to this message.",
            ]


def landing_line(evidence, lands):
    height = evidence["height"]
    minutes = round((height - evidence["tip"]) * evidence["block_time_ms"] / 60_000)
    return f"It {lands} at block {height} (about {minutes} min); Cancel before {height} if this revert is wrong."


def harness_lines(program, line):
    if program != HARNESS:
        return []
    return [line]


def revert_lines(program, verdict):
    match verdict:
        case Would(evidence):
            return [landing_line(evidence, "would land"), *harness_lines(program, "harness would be reverted, so chief would sweep threads.")]
        case Acted(evidence):
            return [landing_line(evidence, "lands"), *harness_lines(program, "harness reverted: chief, sweep threads")]
        case Held() | Refused() | Unknown() | Waiting():
            return []


def pending_changes(cli):
    items, after = [], None
    while True:
        page, after = scheduled_page(cli.query(PROGRAMS, scheduled_query(after)))
        items.extend(page)
        if after is None:
            return items


def programs_moved(blocks):
    return any(outcome.program == PROGRAMS and outcome.applied for block in blocks for tx in block.txs for outcome in tx.outcomes)


class Node:
    def __init__(self, url):
        self.url = url.rstrip("/")

    def call(self, route, body=None):
        request = urllib.request.Request(self.url + route, data=body, method="GET" if body is None else "POST", headers={"User-Agent": "pi-chief"})
        try:
            with urllib.request.urlopen(request) as answer:
                return answer.read()
        except urllib.error.HTTPError as error:
            raise Unreached(f"{route} answered {error.code}: {error.read()[:400]!r}") from error
        except OSError as error:
            raise Unreached(f"{route}: {error}") from error

    def status(self):
        return status_of(self.call("/v1/status"))

    def table(self):
        return table_of(self.call("/v1/programs"))

    def blocks(self, before, limit):
        return page_of(self.call("/v1/blocks", blocks_ask(before, limit, 0)))

    def tx(self, height, position, most):
        return located_of(self.call("/v1/tx", tx_ask(height, position, most)))


class Cli:
    def __init__(self, ducktape, node, workspace, keys=None, runner=run):
        self.ducktape = ducktape
        self.node = node
        self.workspace = workspace
        self.keys = keys
        self.runner = runner

    def env(self):
        env = dict(os.environ, NO_COLOR="1", DUCKTAPE_DEVICE_KEY_STORE="file")
        if self.keys is not None:
            env["DUCKTAPE_USER_KEY"] = str(self.keys / "key")
            env["DUCKTAPE_PASSWORD"] = (self.keys / "password").read_text().strip()
        return env

    def argv(self, node, *args):
        return [*self.ducktape, "--workspace", str(self.workspace), "--node", node, *args]

    def query(self, program, request):
        ran = self.runner(self.argv(self.node, "query", program, "--raw"), stdin=request, env=self.env())
        if ran.code != 0:
            raise Unreached(f"{program} query: {ran.err}")
        return ran.out

    def submit(self, program, payload, node=None):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "op"
            path.write_bytes(payload)
            land = [] if node else ["--land"]
            return self.runner(self.argv(node or self.node, "submit", program, str(path), *land), env=self.env())


class Kube:
    def __init__(self, namespace, deployment, container, mount=SERVICE_ACCOUNT, environ=os.environ, opener=urllib.request.urlopen):
        self.namespace = namespace
        self.name = deployment
        self.container = container
        self.mount = mount
        self.environ = environ
        self.opener = opener

    def call(self, method, resource, query=None, body=None):
        token = (self.mount / "token").read_text().strip()
        context = ssl.create_default_context(cafile=str(self.mount / "ca.crt"))
        host = self.environ["KUBERNETES_SERVICE_HOST"]
        bracketed = f"[{host}]" if ":" in host else host
        path = f"/apis/apps/v1/namespaces/{self.namespace}/{resource}"
        target = f"https://{bracketed}:{self.environ['KUBERNETES_SERVICE_PORT']}{path}" + (f"?{urlencode(query)}" if query else "")
        kind = {"Content-Type": "application/strategic-merge-patch+json"} if body is not None else {}
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(target, data=data, method=method, headers={"Authorization": f"Bearer {token}", "Accept": "application/json", **kind})
        try:
            with self.opener(request, context=context) as answer:
                return json.load(answer)
        except urllib.error.HTTPError as refusal:
            status = json.loads(refusal.read() or b"{}")
            raise Failed(f"the Kubernetes API answered {refusal.code} {status.get('reason', '')}: {status.get('message', '')}") from refusal
        except urllib.error.URLError as error:
            raise Failed(f"the Kubernetes API was not reached: {error.reason}") from error

    def deployment(self):
        return self.call("GET", f"deployments/{self.name}")

    def replicasets(self):
        return self.call("GET", "replicasets")["items"]

    def patch(self, body, dry_run):
        dry = {"dryRun": "All"} if dry_run else {}
        return self.call("PATCH", f"deployments/{self.name}", {"fieldManager": MANAGER, **dry}, body)


class Capture:
    def __init__(self, node):
        self.node = node
        self.frames = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def handler(self):
        capture = self

        class Handler(BaseHTTPRequestHandler):
            def answer(self, status, body):
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                if self.path != "/v1/status":
                    return self.answer(404, b"")
                self.answer(200, capture.node.call("/v1/status"))

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                if self.path != "/v1/submit":
                    return self.answer(404, b"")
                capture.frames.append(Reader(body).bytes())
                self.answer(200, u8(0))

            def log_message(self, form, *args):
                return

        return Handler

    def __enter__(self):
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()


@dataclass(frozen=True)
class Config:
    mode: str
    account: int
    report_to: tuple
    hands_off: int
    anchor_window: int

    @staticmethod
    def from_env(env):
        return Config(
            mode=env["PI_CHIEF_MODE"],
            account=int(env["PI_CHIEF_ACCOUNT"]),
            report_to=tuple(int(number) for number in env["PI_CHIEF_REPORT_TO"].split()),
            hands_off=int(env["PI_CHIEF_HANDS_OFF"]),
            anchor_window=int(env["PI_CHIEF_ANCHOR_WINDOW"]),
        )


class Store:
    def __init__(self, path):
        self.path = path

    def load(self):
        return json.loads(self.path.read_text()) if self.path.exists() else fresh_state()

    def save(self, state):
        temp = self.path.with_suffix(".tmp")
        temp.write_text(json.dumps(state, sort_keys=True))
        temp.replace(self.path)


class Rollbacks:
    def __init__(self, config, node, cli, kube):
        self.config = config
        self.node = node
        self.cli = cli
        self.kube = kube

    def node_dry_run(self, image, previous, contract, previous_contract):
        if contract is not None and previous_contract is not None and contract != previous_contract:
            return Refused("contract_differs", f"{image} runs node contract {contract} and {previous} ran contract {previous_contract}, so {previous} would refuse the state {image} stamped")
        deployment = self.kube.deployment()
        rollout = rollout_of(deployment, self.kube.container)
        if rollout.image != image:
            return Refused("image_moved", f"the Deployment now runs {rollout.image}, not {image}")
        before = image_before(deployment, self.kube.replicasets(), self.kube.container)
        if before != previous:
            return Refused("replicasets_disagree", f"the newest older ReplicaSet with another image runs {before}, and pi-chief last saw {previous} proven")
        body = image_patch(self.kube.container, previous, rollout.resource_version)
        dry = self.kube.patch(body, dry_run=True)
        changed = changed_paths(deployment, dry)
        wanted = [image_path(deployment, self.kube.container)]
        if changed != wanted:
            return Refused("patch_changes_more", f"the server-side dry run changes {changed}, not only {wanted}")
        return Would({"deployment": f"{self.kube.namespace}/{self.kube.name}", "from": image, "to": previous, "changes": changed, "resource_version": rollout.resource_version, "body": body})

    def node_rollback(self, image, previous, contract, previous_contract):
        checked = self.node_dry_run(image, previous, contract, previous_contract)
        if not isinstance(checked, Would):
            return checked
        try:
            patched = self.kube.patch(checked.evidence["body"], dry_run=False)
        except Failed as failed:
            return Unknown(str(failed))
        return Acted({**checked.evidence, "image_now": container_image(patched["spec"]["template"], self.kube.container)})

    def source_file(self, repo, commit, folder, name):
        after = None
        while True:
            entries, after = tree_of(self.cli.query(FORGE, tree_query(repo, commit, folder, after)))
            oid = next((oid for entry, oid, kind in entries if entry == name), None)
            if oid is not None:
                return blob_of(self.cli.query(FORGE, blob_query(repo, oid)))
            if after is None:
                return None

    def notice(self, code):
        held = sources_of(self.cli.query(PROGRAMS, sources_query())).get(PROGRAMS)
        if held is None or held[0] != code:
            return Refused("notice_source", f"programs' Sources names {held[0] if held else 'nothing'} for programs, not the live {code}")
        if held[1] is None:
            return Refused("notice_source", f"the programs code {code} names no source commit, so its NOTICE can't be read")
        repo, commit = held[1]
        where = f"{repo} {commit} {(NOTICE_FOLDER + b'/' + NOTICE_FILE).decode()}"
        source = self.source_file(repo, commit, NOTICE_FOLDER, NOTICE_FILE)
        if source is None:
            return Refused("notice_source", f"forge holds no {where}")
        found = NOTICE_LINE.findall(source.decode(errors="replace"))
        if len(found) != 1:
            return Refused("notice_source", f"{where} names NOTICE {len(found)} times, not once")
        return int(found[0]), f"{repo} {commit}"

    def program_op(self, program, code, previous):
        live = self.node.table()
        read = self.notice(live.get(PROGRAMS))
        if isinstance(read, Refused):
            return read
        notice, notice_source = read
        if live.get(program) != code:
            return Refused("code_moved", f"{program} now runs {live.get(program)}, not {code}")
        status = self.node.status()
        waiting = [item for item in pending_changes(self.cli) if item.change.program == program and item.height > status.height]
        if waiting:
            return Refused("pending", f"a change to {program} already waits at height {waiting[0].height}")
        entry = entry_of(self.cli.query(PROGRAMS, program_query(program)))
        if entry is None or entry[1] != code:
            return Refused("code_moved", f"programs answers {entry} for {program}")
        height = status.height + self.config.anchor_window + notice
        op = schedule_over_set(height, program, previous, entry[2], code)
        evidence = {"program": program, "from": code, "to": previous, "height": height, "tip": status.height, "block_time_ms": status.block_time_ms, "params_hex": entry[2].hex(), "op_hex": op.hex(), "notice": notice, "notice_source": notice_source}
        return Would(evidence)

    def program_dry_run(self, program, code, previous):
        built = self.program_op(program, code, previous)
        if not isinstance(built, Would):
            return built
        op = bytes.fromhex(built.evidence["op_hex"])
        with Capture(self.node) as capture:
            ran = self.cli.submit(PROGRAMS, op, node=capture.url)
        if ran.code != 0 or len(capture.frames) != 1:
            return Refused("unsigned", ran.err or ran.out.decode(errors="replace"))
        frame = capture.frames[0]
        signed = {"frame_bytes": len(frame), "frame_sha256": hashlib.sha256(frame).hexdigest(), "carries_op": op in frame, "sent": False}
        return Would({**built.evidence, **signed})

    def program_rollback(self, program, code, previous):
        built = self.program_op(program, code, previous)
        if not isinstance(built, Would):
            return built
        outcome, said, sentence, block = receipt(self.cli.submit(PROGRAMS, bytes.fromhex(built.evidence["op_hex"])))
        match outcome:
            case "applied":
                return Acted({**built.evidence, "scheduled_in": block})
            case "rejected":
                return Refused(said, f"programs refused the Set in block {block}: {sentence}")
            case "unlanded":
                return Refused("unlanded", f"the ScheduleOver never landed, so nothing changed: {sentence}")
            case "unknown":
                return Unknown(sentence)


class Chief:
    def __init__(self, config, node, cli, kube, store, clock=time.time, pause=time.sleep):
        self.config = config
        self.node = node
        self.cli = cli
        self.kube = kube
        self.store = store
        self.clock = clock
        self.pause = pause
        self.rollbacks = Rollbacks(config, node, cli, kube)
        self.state = store.load()
        self.unreached = 0
        self.dm_open = set()

    def now(self):
        return int(self.clock() * 1000)

    def read_rollout(self):
        try:
            return rollout_of(self.kube.deployment(), self.kube.container)
        except Failed as failed:
            say("kube_unread", detail=str(failed))
            return None

    def new_blocks(self, status):
        done = self.state.get("done", status.height - 1)
        blocks = []
        before = status.height + 1
        while before - 1 > done:
            page, _ = self.node.blocks(before, min(status.epoch_length, before - 1 - done))
            if not page:
                break
            blocks.extend(page)
            before = page[-1].height
        return tuple(sorted(blocks, key=lambda block: block.height))

    def stalled_now(self, blocks, now):
        state = dict(self.state)
        if blocks:
            state["tip"] = {"height": blocks[-1].height, "time": blocks[-1].time}
        return stalled_for(state, now) is not None

    def read(self):
        now = self.now()
        rollout = self.read_rollout()
        try:
            status = self.node.status()
            blocks = self.new_blocks(status)
            programs = self.node.table()
            after = self.node.status()
            tip = blocks[-1].height if blocks else (self.state.get("tip") or {}).get("height")
            unread_at_tip = (self.state.get("due") or {}).get("at") != tip
            stalled_unread = self.stalled_now(blocks, now) and unread_at_tip
            unread = self.state.get("schedulers") is None
            scheduled = None
            if stalled_unread or unread or programs_moved(blocks):
                scheduled = tuple(pending_changes(self.cli))
        except (Unreached, Failed) as error:
            self.unreached += 1
            if worth_a_line(self.unreached):
                say("unreached", attempts=self.unreached, detail=str(error))
            return Seen(now, None, (), None, rollout, None)
        if self.unreached:
            say("reached", attempts=self.unreached)
            self.unreached = 0
        return Seen(now, status, blocks, Table(programs, status.height, after.height), rollout, scheduled)

    def report(self, key, lines):
        say("report", key=key, lines=lines)
        self.state["reported"].append(key)
        self.state["outbox"].append({"key": key, "lines": lines, "to": list(self.config.report_to), "posted": []})

    def hold(self, key):
        self.state["held"] = {"key": key, "posts": []}

    def node_verdict(self, finding):
        if self.state["held"]:
            return Held(self.state["held"]["key"])
        if self.config.mode == "act":
            return self.rollbacks.node_rollback(finding.image, finding.previous, finding.contract, finding.previous_contract)
        return self.rollbacks.node_dry_run(finding.image, finding.previous, finding.contract, finding.previous_contract)

    def node_failed(self, finding):
        action = f"set the node container's image back to {finding.previous}"
        verdict = self.node_verdict(finding)
        if isinstance(verdict, Acted | Unknown):
            self.state["own_image"] = finding.previous
            self.hold(finding.key)
        self.report(finding.key, finding_lines(finding) + verdict_lines(verdict, action))

    def named(self, account, height):
        manager = None if account is None else manager_of(self.cli.query(IDENTITY, account_query(account)))
        return {"account": account, "manager": manager, "height": height}

    def scheduler(self, program, code, since, scheduled):
        if scheduled is not None and scheduled["account"] is None:
            return {"account": None, "manager": None, "height": scheduled["height"], "vote": True}
        if scheduled is not None:
            return self.named(scheduled["account"], scheduled["height"])
        listed_from = None
        before = since + 1
        epoch = self.state["chain"]["epoch_length"]
        while listed_from is None or before > listed_from:
            page, listed_from = self.node.blocks(before, epoch)
            if not page:
                break
            for block in page:
                for tx in block.txs:
                    found = self.scheduled_by(block, tx, program, code)
                    if found is not None:
                        return found
            before = page[-1].height
        return {"account": None, "manager": None, "height": None}

    def scheduled_by(self, block, tx, program, code):
        carries = tx.target == PROGRAMS and tx.outcomes and tx.outcomes[0].program == PROGRAMS and tx.outcomes[0].applied
        if not carries:
            return None
        located = self.node.tx(block.height, tx.position, tx.size)
        if located is None or located.payload is None:
            return None
        scheduled = schedule_of(located.payload)
        sets = scheduled is not None and scheduled.change.verb in ("set", "publish")
        if not sets or (scheduled.change.program, scheduled.change.code) != (program, code):
            return None
        return self.named(number_of(self.cli.query(IDENTITY, of_key_query(located.signer))), block.height)

    def program_verdict(self, finding):
        record = self.state["programs"][finding.program]
        refused = program_refusal(finding, record, self.state["chain"]["roles"])
        if refused is not None:
            return refused
        if record["scheduler"] is None:
            record["scheduler"] = self.scheduler(finding.program, finding.code, finding.since, record.get("scheduled"))
        refused = scheduler_refusal(finding, record["scheduler"], self.config.hands_off)
        if refused is not None:
            return refused
        if self.state["held"]:
            return Held(self.state["held"]["key"])
        if not self.state.get("advanced"):
            return Waiting("chain_not_advancing")
        if self.config.mode == "act":
            return self.rollbacks.program_rollback(finding.program, finding.code, finding.previous)
        return self.rollbacks.program_dry_run(finding.program, finding.code, finding.previous)

    def program_failed(self, finding):
        action = f"schedule {finding.program} back to code {finding.previous}"
        verdict = self.program_verdict(finding)
        if isinstance(verdict, Waiting):
            say("waiting", key=finding.key, reason=verdict.reason)
            return
        if isinstance(verdict, Acted | Unknown):
            self.state["own_programs"][finding.program] = finding.previous
            self.hold(finding.key)
        self.report(finding.key, finding_lines(finding) + verdict_lines(verdict, action) + revert_lines(finding.program, verdict))

    def halted(self, finding):
        self.report(finding.key, finding_lines(finding))

    def stalled(self, finding):
        self.report(finding.key, finding_lines(finding))

    def handle(self, finding):
        match finding:
            case NodeFailed():
                self.node_failed(finding)
            case ProgramFailed():
                self.program_failed(finding)
            case HaltedAtSwitch():
                self.halted(finding)
            case Stalled():
                self.stalled(finding)

    def sent(self, op):
        outcome, said, sentence, _ = receipt(self.cli.submit(CHAT, op))
        return outcome == "applied" or said == "already_exists", said or outcome, sentence

    def unposted(self, item, to, reason, sentence):
        item["attempts"] = item.get("attempts", 0) + 1
        if worth_a_line(item["attempts"]):
            say("report_unposted", key=item["key"], to=to, attempts=item["attempts"], reason=reason, detail=sentence)

    def post(self, item, to):
        channel = dm_channel(self.config.account, to)
        message_id = f"pi-chief-{item['key']}-{to}"
        if to not in self.dm_open:
            opened, reason, sentence = self.sent(dm_open(to, MANAGER))
            if not opened:
                self.unposted(item, to, reason, sentence)
                return None
            self.dm_open.add(to)
        posted, reason, sentence = self.sent(post_message(channel, message_id, item["lines"]))
        if not posted:
            self.unposted(item, to, reason, sentence)
            return None
        say("report_posted", key=item["key"], to=to, channel=channel, message=message_id)
        return {"channel": channel, "message_id": message_id, "seq": None}

    def flush(self):
        for item in list(self.state["outbox"]):
            for to in [to for to in item["to"] if to not in item["posted"]]:
                post = self.post(item, to)
                if post is None:
                    continue
                item["posted"].append(to)
                held = self.state["held"]
                if held is not None and held["key"] == item["key"]:
                    held["posts"].append(post)
            if len(item["posted"]) == len(item["to"]):
                self.state["outbox"].remove(item)

    def check_ack(self):
        held = self.state["held"]
        if held is None:
            return
        for post in held["posts"]:
            if post["seq"] is None:
                post["seq"] = seq_of(self.cli.query(CHAT, message_query(post["message_id"])))
            if post["seq"] is None:
                continue
            reactors = reactors_of(self.cli.query(CHAT, reactors_query(post["channel"], [post["seq"]])))
            ackers = sorted(reactors & set(self.config.report_to))
            if ackers:
                say("acknowledged", key=held["key"], by=ackers)
                self.state["held"] = None
                return

    def tick(self):
        seen = self.read()
        self.state, findings = decide(self.state, seen)
        for finding in findings:
            try:
                self.handle(finding)
            except (Unreached, Failed) as error:
                say("finding_unread", key=finding.key, detail=str(error))
        if seen.status is not None:
            try:
                self.flush()
                self.check_ack()
            except (Unreached, Failed) as error:
                say("chat_unread", detail=str(error))
        self.store.save(self.state)

    def run(self):
        say("watching", mode=self.config.mode, account=self.config.account, report_to=list(self.config.report_to))
        while True:
            self.tick()
            chain = self.state.get("chain")
            if chain is None:
                say("stopped", reason="chain_never_read")
                sys.exit(1)
            self.pause(chain["block_time_ms"] / 1000)


def parts(env):
    node = Node(env["PI_CHIEF_NODE"])
    keys = Path(env.get("PI_CHIEF_KEYS", "/keys"))
    workspace = Path(env.get("PI_CHIEF_WORKSPACE", tempfile.mkdtemp()))
    workspace.mkdir(parents=True, exist_ok=True)
    cli = Cli(shlex.split(env.get("PI_CHIEF_DUCKTAPE", "ducktape")), env["PI_CHIEF_NODE"], workspace, keys)
    kube = Kube(env["PI_CHIEF_NAMESPACE"], env["PI_CHIEF_DEPLOYMENT"], env["PI_CHIEF_CONTAINER"])
    return node, cli, kube


def node_dry_run_recorded(rollbacks, record):
    kube = rollbacks.kube
    deployment = kube.deployment()
    image = rollout_of(deployment, kube.container).image
    previous = image_before(deployment, kube.replicasets(), kube.container)
    if previous is None:
        return Refused("no_previous", "no older ReplicaSet runs another image")
    contract = record.get("contract") if record.get("image") == image else None
    previous_contract = record.get("previous_contract") if record.get("previous") == previous else None
    return rollbacks.node_dry_run(image, previous, contract, previous_contract)


def dry_run(args, env):
    node, cli, kube = parts(env)
    rollbacks = Rollbacks(Config.from_env(env), node, cli, kube)
    match args:
        case ["node"]:
            record = Store(Path(env.get("PI_CHIEF_STATE", "/state")) / "state.json").load().get("node") or {}
            verdict = node_dry_run_recorded(rollbacks, record)
        case ["program", program, code]:
            verdict = rollbacks.program_dry_run(program, node.table().get(program), code)
        case _:
            sys.exit("usage: pi-chief.py dry-run node | dry-run program PROGRAM CODE")
    print(json.dumps({"verdict": type(verdict).__name__, **verdict.__dict__}, indent=2, sort_keys=True, default=str))


def main():
    env = os.environ
    match sys.argv[1:]:
        case []:
            node, cli, kube = parts(env)
            store = Store(Path(env.get("PI_CHIEF_STATE", "/state")) / "state.json")
            Chief(Config.from_env(env), node, cli, kube, store).run()
        case ["dry-run", *args]:
            dry_run(args, env)
        case other:
            sys.exit(f"usage: pi-chief.py [dry-run node | dry-run program PROGRAM CODE], not {' '.join(other)}")


if __name__ == "__main__":
    main()
