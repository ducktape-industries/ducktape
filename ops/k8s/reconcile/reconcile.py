#!/usr/bin/env python3
# reconcile: applies what forge repo `infra` holds on `main` to fabrics-1, one duty at a time: the node's egress services (`egress/<node>.toml`) and the pods service's image (`pods.toml`).
# Each open change into `main` gets a dry-run verdict per duty as a commit status and a review comment; each new `main` is applied once, proven, and rolled back when its proof fails. reconcile-test.py tests it against a fake node and a fake Kubernetes.
import http.client
import json
import os
import re
import shlex
import ssl
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlencode

REPO = "infra"
MAIN = "refs/heads/main"
CONTEXT = "reconcile/egress"
RESERVED = ("http", "relay")
ATTEMPTS_PER_LINE = 10
MODES = ("report", "act")
NOT_FOUND = "not_found"
LEVELS = {"report_unposted": "warn", "unread": "warn"}
PODS_PATH = "pods.toml"
PODS_CONTEXT = "reconcile/pods"
PODS_NAMESPACE = "pods"
PODS_DEPLOYMENT = "pods"
PODS_CONTAINER = "pods"
PODS_OUT = "PODS_OUT_IMAGE"
PODS_IMAGE = re.compile(r"localhost:5000/pods:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}@sha256:[0-9a-f]{64}")
DEPLOYMENTS = f"/apis/apps/v1/namespaces/{PODS_NAMESPACE}/deployments"
MANAGER = "reconcile"
SERVICE_ACCOUNT = Path("/var/run/secrets/kubernetes.io/serviceaccount")
CONFLICT = 409
GONE = 410


class Unread(Exception):
    def __init__(self, detail, reason=None):
        super().__init__(detail)
        self.reason = reason


def refusal_reason(said):
    found = re.search(r"\brefused: ([a-z_]+):", said)
    return found.group(1) if found else None


class Refused(Exception):
    pass


class KubeRefused(Exception):
    pass


class Gone(Exception):
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
            said = done.stderr.decode(errors="replace").strip() or f"ducktape exited {done.returncode}"
            if refusal_reason(said) is None:
                raise Unread(said)
            raise Refused(said)
        return json.loads(done.stdout)


class Forge:
    def __init__(self, cli):
        self.cli = cli

    def tip(self):
        query = {"log": {"repo": REPO, "from": {"ref": hexed(MAIN)}, "exclude": None, "page": {"after": None, "limit": 1}}}
        return self.cli.query("forge", query)["log"]["tip"]

    def entry(self, commit, path):
        folder, _, name = path.rpartition("/")
        page = {"after": None, "limit": None}
        while True:
            query = {"tree": {"repo": REPO, "at": commit, "path": hexed(folder), "page": page}}
            try:
                listed = self.cli.query("forge", query)["tree"]["page"]
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
            blob = self.cli.query("forge", {"blob": {"repo": REPO, "oid": oid, "range": rest}})["blob"]["blob"]
            size = blob["size"]
            read.extend(bytes.fromhex(blob["bytes"]))
            if len(read) >= size or not blob["bytes"]:
                return read.decode(errors="surrogateescape")

    def open_changes(self):
        page = {"after": None, "limit": None}
        found = []
        while True:
            query = {"changes": {"repo": REPO, "filter": {"author": None, "involves": None, "state": "open"}, "page": page}}
            listed = self.cli.query("forge", query)["changes"]["page"]
            found.extend(item["n"] for item in listed["items"] if item["into"] == hexed(MAIN))
            if listed["next"] is None:
                return found
            page = {"after": listed["next"], "limit": None}

    def head(self, n):
        query = {"change": {"repo": REPO, "n": n, "page": {"after": None, "limit": 1}}}
        return self.cli.query("forge", query)["change"]["source_head"]

    def judge(self, n, head, passed, body, context):
        state = "passed" if passed else "failed"
        self.cli.op("forge", {"status": {"repo": REPO, "commit": head, "context": context, "state": state, "url": None}})
        review = {"base_oid": None, "body": body, "comments": [], "commit_oid": head, "verdict": "comment"}
        self.cli.op("forge", {"review_submit": {"repo": REPO, "n": n, "review": review}})


class Kube:
    def __init__(self, mount=SERVICE_ACCOUNT, environ=os.environ, opener=urllib.request.urlopen, tls=ssl.create_default_context):
        self.mount = mount
        self.environ = environ
        self.opener = opener
        self.tls = tls

    def open(self, method, path, query=None, body=None):
        token = (self.mount / "token").read_text().strip()
        context = self.tls(cafile=str(self.mount / "ca.crt"))
        host = self.environ["KUBERNETES_SERVICE_HOST"]
        bracketed = f"[{host}]" if ":" in host else host
        target = f"https://{bracketed}:{self.environ['KUBERNETES_SERVICE_PORT']}{path}" + (f"?{urlencode(query)}" if query else "")
        kind = {"Content-Type": "application/strategic-merge-patch+json"} if body is not None else {}
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(target, data=data, method=method, headers={"Authorization": f"Bearer {token}", "Accept": "application/json", **kind})
        try:
            return self.opener(request, context=context)
        except urllib.error.HTTPError as refusal:
            status = json.loads(refusal.read() or b"{}")
            said = f"the Kubernetes API answered {refusal.code} {status.get('reason', '')}: {status.get('message', '')}"
            if refusal.code == CONFLICT:
                raise Unread(said) from refusal
            if refusal.code == GONE:
                raise Gone(said) from refusal
            raise KubeRefused(said) from refusal
        except (OSError, http.client.HTTPException) as error:
            raise Unread(f"the Kubernetes API was not reached: {error}") from error

    def call(self, method, path, query=None, body=None):
        with self.open(method, path, query, body) as answer:
            try:
                return json.load(answer)
            except (OSError, http.client.HTTPException) as error:
                raise Unread(f"the Kubernetes API's answer broke off: {error}") from error

    def deployment(self):
        return self.call("GET", f"{DEPLOYMENTS}/{PODS_DEPLOYMENT}")

    def patch(self, body, dry_run):
        dry = {"dryRun": "All"} if dry_run else {}
        return self.call("PATCH", f"{DEPLOYMENTS}/{PODS_DEPLOYMENT}", {"fieldManager": MANAGER, **dry}, body)

    def watch(self, resource_version):
        query = {"watch": "true", "fieldSelector": f"metadata.name={PODS_DEPLOYMENT}", "resourceVersion": resource_version}
        with self.open("GET", DEPLOYMENTS, query) as answer:
            try:
                for line in answer:
                    yield json.loads(line)
            except (OSError, http.client.HTTPException) as error:
                raise Unread(f"the Kubernetes watch broke off: {error}") from error

    def pods(self):
        return self.call("GET", f"/api/v1/namespaces/{PODS_NAMESPACE}/pods", {"labelSelector": "app=pods"})["items"]


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


@dataclass(frozen=True)
class Rollout:
    image: str
    complete: bool
    deadline_exceeded: bool
    deadline_s: int
    resource_version: str


def container_of(template):
    held = [container for container in template["spec"]["containers"] if container["name"] == PODS_CONTAINER]
    return held[0] if held else None


def container_image(template):
    container = container_of(template)
    return None if container is None else container["image"]


def out_image(template):
    container = container_of(template) or {}
    values = [held.get("value") for held in container.get("env", []) if held["name"] == PODS_OUT]
    return values[0] if values else None


def rollout_of(deployment):
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
    image = container_image(spec["template"])
    if image is None:
        raise KubeRefused(f"deployment {PODS_NAMESPACE}/{PODS_DEPLOYMENT} has no container {PODS_CONTAINER}")
    return Rollout(image, complete, exceeded, spec["progressDeadlineSeconds"], deployment["metadata"]["resourceVersion"])


def pods_patch(image, out, resource_version):
    container = {"name": PODS_CONTAINER, "image": image}
    if out is not None:
        container["env"] = [{"name": PODS_OUT, "value": out}]
    return {"metadata": {"resourceVersion": resource_version}, "spec": {"template": {"spec": {"containers": [container]}}}}


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


def pods_paths(deployment):
    containers = deployment["spec"]["template"]["spec"]["containers"]
    at = [held["name"] for held in containers].index(PODS_CONTAINER)
    names = [held["name"] for held in containers[at].get("env", [])]
    base = f"spec.template.spec.containers.{at}"
    return {f"{base}.image", *([f"{base}.env.{names.index(PODS_OUT)}.value"] if PODS_OUT in names else [])}


def digest_of(image):
    return image.rpartition("@")[2]


@dataclass(frozen=True)
class PodsPlan:
    problems: tuple
    image: str | None = None
    live: str | None = None
    out: str | None = None
    changed: tuple = ()
    body: dict | None = None


def pods_declared(text):
    if not utf8(text):
        return None, "the file is not UTF-8"
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        return None, f"the file does not parse: {error}"
    if sorted(document) != ["image"] or not isinstance(document["image"], str):
        return None, "the file does not parse: pods.toml holds one string, image"
    image = document["image"]
    if not PODS_IMAGE.fullmatch(image):
        return None, f"the image {image} is not localhost:5000/pods:<tag>@sha256:<digest>, a digest already pushed"
    return image, None


def serves_pods(egress_text):
    if egress_text is None:
        return False
    try:
        services = tomllib.loads(egress_text).get("services", {})
    except ValueError:
        return False
    return isinstance(services, dict) and "pods" in services


def pods_plan(text, egress_text, kube):
    image, problem = pods_declared(text)
    if problem is not None:
        return PodsPlan((problem,))
    if not serves_pods(egress_text):
        return PodsPlan(("infra's egress file serves no pods service, so the roll's proof that the node reaches pods cannot run",))
    try:
        deployment = kube.deployment()
        template = deployment["spec"]["template"]
        live = container_image(template)
        if live is None:
            return PodsPlan((f"deployment {PODS_NAMESPACE}/{PODS_DEPLOYMENT} has no container {PODS_CONTAINER}",))
        body = pods_patch(image, image, deployment["metadata"]["resourceVersion"])
        dry = kube.patch(body, dry_run=True)
    except KubeRefused as refused:
        return PodsPlan((f"Kubernetes refuses the dry run: {refused}",))
    changed = changed_paths(deployment, dry)
    stray = [path for path in changed if path not in pods_paths(deployment)]
    if stray:
        return PodsPlan((f"the server-side dry run changes {stray}, beyond the pods container's image and {PODS_OUT}",))
    return PodsPlan((), image, live, out_image(template), tuple(changed), body)


def pods_verdict_body(found):
    if found.problems:
        return "\n".join(["reconcile/pods: FAIL", *[f"- {problem}" for problem in found.problems]])
    if not found.changed:
        return f"reconcile/pods: PASS (the pods service already runs {found.image}, nothing to roll)"
    return "\n".join(
        [
            "reconcile/pods: PASS (server-side dry run, nothing applied)",
            f"rolls {found.live} to {found.image}; the dry run changes {', '.join(found.changed)}",
        ]
    )


def report_status(key):
    kind = key.removeprefix("pods-").rsplit("-", 1)[0]
    if kind == "proven":
        return "passed"
    if kind == "applied":
        return "running"
    if kind.startswith("would-"):
        return "note"
    return "failed"


def report_group(key):
    duty = "pods" if key.startswith("pods-") else "egress"
    return f"{duty}-{key.rsplit('-', 1)[1]}"


def concerns_of(setting):
    return tuple(int(account) for account in setting.split())


def fresh_pods():
    return {"seen_tip": None, "main_text": None, "applied": None, "rolling": None}


def fresh_state():
    return {"seen_tip": None, "main_text": None, "applied": None, "setting": None, "proving": None, "judged": [], "outbox": [], "pods": fresh_pods()}


class Reconcile:
    def __init__(self, mode, node_name, node_key, channel, cli, forge, store, kube, prober, concerns=()):
        if mode not in MODES:
            raise ValueError(f"RECONCILE_MODE is report or act, not {mode}")
        self.mode = mode
        self.path = egress_path(node_name)
        self.node_key = node_key
        self.channel = channel
        self.cli = cli
        self.forge = forge
        self.store = store
        self.kube = kube
        self.prober = prober
        self.concerns = tuple(concerns)
        self.state = store.load()
        if self.state.get("mode") != mode:
            self.state["seen_tip"] = None
            self.state["pods"]["seen_tip"] = None
            self.state["mode"] = mode
        self.unread = 0

    def report(self, key, lines, mention=None):
        say("report", key=key, lines=lines)
        self.state["outbox"].append({"id": f"reconcile-{key}", "key": key, "text": "\n".join(lines), "tried": 0, "mention": mention})

    def notice_op(self, item):
        first, *rest = item["text"].split("\n")
        mention = item["mention"]
        concerns = [*self.concerns, *([] if mention is None or mention in self.concerns else [mention])]
        key = item.get("key", item["id"].removeprefix("reconcile-"))
        notice = {"kind": "infra", "group": report_group(key), "subject": first, "link": None, "status": report_status(key), "detail": "; ".join(rest), "concerns": concerns}
        return {"notify": {"channel_id": self.channel, "message_id": item["id"], "thread": None, "notice": notice}}

    def judge_changes(self):
        main_text = self.state["main_text"]
        for n in self.forge.open_changes():
            head = self.forge.head(n)
            key = f"{n}:{head}"
            if head is None or key in self.state["judged"]:
                continue
            text = self.forge.file(head, self.path)
            pods_text = self.forge.file(head, PODS_PATH)
            verdicts = []
            if text is not None and text != main_text:
                found = plan(text, self.cli)
                verdicts.append((CONTEXT, not found.problems, verdict_body(found)))
            if pods_text is not None and pods_text != self.state["pods"]["main_text"]:
                found = pods_plan(pods_text, main_text if text is None else text, self.kube)
                verdicts.append((PODS_CONTEXT, not found.problems, pods_verdict_body(found)))
            for context, passed, body in verdicts:
                self.forge.judge(n, head, passed, body, context)
                say("judged", change=n, head=head, context=context, passed=passed)
            self.state["judged"].append(key)

    def apply(self, tip, text):
        short = tip[:12]
        found = plan(text, self.cli)
        setting = self.state.get("setting")
        resend = setting is not None and setting["commit"] == tip
        if found.problems and resend and setting["before"] is not None:
            return self.roll_back(short, setting["before"], found.problems)
        if found.problems:
            return self.report(f"refused-{short}", [f"infra main {short}: egress not applied, the node keeps what it serves.", *found.problems])
        if not resend and not any(found.dry["tally"][part] for part in ("added", "removed", "changed")):
            self.state["applied"] = {"commit": tip, "text": text}
            return say("unchanged", commit=tip)
        if self.mode == "report":
            return self.report(f"would-apply-{short}", [f"infra main {short}: would apply egress ({tally_line(found.dry['tally'])}). Report mode, so the node is unchanged."])
        before, planned = (setting["before"], setting["tally"]) if resend else (self.state["applied"], found.dry["tally"])
        self.state["setting"] = {"commit": tip, "before": before, "tally": planned}
        self.store.save(self.state)
        try:
            self.cli.egress_set(text, dry=False)
        except Refused as refused:
            return self.report(f"refused-{short}", [f"infra main {short}: the node refused egress, and keeps what it serves: {refused}"])
        self.state["applied"] = {"commit": tip, "text": text}
        after = plan(text, self.cli)
        if after.problems and before is not None:
            return self.roll_back(short, before, after.problems)
        self.state["proving"] = {"commit": tip, "served": served(tomllib.loads(text))}
        self.report(f"applied-{short}", [f"infra main {short}: egress applied ({tally_line(planned)}). Proven once the chain's record of what the node serves matches."])

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
        row = self.cli.query("egress", {"node": {"key": self.node_key}})["node"]
        if row is None or sorted(row["services"]) != proving["served"]:
            return
        self.state["proving"] = None
        self.report(f"proven-{proving['commit'][:12]}", [f"infra main {proving['commit'][:12]}: proven, the chain records the node serving {', '.join(proving['served'])}."])

    def follow_pods(self, tip, text):
        pods = self.state["pods"]
        found = None if text is None else pods_plan(text, self.state["main_text"], self.kube)
        pods["seen_tip"] = tip
        pods["main_text"] = text
        if found is None:
            return say("pods_absent", commit=tip)
        short = tip[:12]
        if found.problems:
            return self.report(f"pods-refused-{short}", [f"infra main {short}: the pods service is not rolled, and runs what it ran.", *found.problems])
        if not found.changed:
            pods["applied"] = {"commit": tip, "image": found.image}
            return say("pods_unchanged", commit=tip, image=found.image)
        if self.mode == "report":
            return self.report(f"pods-would-roll-{short}", [f"infra main {short}: would roll the pods service from {found.live} to {found.image}. Report mode, so it is unchanged."])
        pods["rolling"] = {"commit": tip, "image": found.image, "before": found.live, "before_out": found.out, "problems": None}
        self.store.save(self.state)
        try:
            self.kube.patch(found.body, dry_run=False)
        except KubeRefused as refused:
            pods["rolling"] = None
            return self.report(f"pods-refused-{short}", [f"infra main {short}: Kubernetes refused the roll, so the pods service runs {found.live}: {refused}"])
        say("pods_rolling", commit=tip, image=found.image, before=found.live)

    def rollouts(self, deployment):
        yield rollout_of(deployment)
        version = deployment["metadata"]["resourceVersion"]
        while True:
            try:
                for event in self.kube.watch(version):
                    held = event["object"]
                    if event["type"] == "ERROR" and held.get("code") == GONE:
                        raise Gone(held.get("message", ""))
                    if event["type"] == "ERROR":
                        raise KubeRefused(f"the watch of deployment {PODS_NAMESPACE}/{PODS_DEPLOYMENT} ended: {held.get('message', held)}")
                    if event["type"] == "DELETED":
                        raise KubeRefused(f"deployment {PODS_NAMESPACE}/{PODS_DEPLOYMENT} was deleted")
                    version = held["metadata"]["resourceVersion"]
                    yield rollout_of(held)
            except Gone:
                deployment = self.kube.deployment()
                version = deployment["metadata"]["resourceVersion"]
                yield rollout_of(deployment)

    def roll(self):
        rolling = self.state["pods"]["rolling"]
        if rolling is None:
            return
        if rolling["problems"] is not None:
            return self.roll_back_pods(rolling)
        short = rolling["commit"][:12]
        try:
            for rollout in self.rollouts(self.kube.deployment()):
                if rollout.image != rolling["image"] or rollout.complete or rollout.deadline_exceeded:
                    break
            if rollout.image != rolling["image"]:
                self.state["pods"]["rolling"] = None
                return self.report(f"pods-moved-{short}", [f"infra main {short}: the pods Deployment runs {rollout.image}, not {rolling['image']}: the roll did not land, or someone changed it since. reconcile sends nothing more for it."])
            if not rollout.complete:
                problems = [f"the rollout passed its progress deadline ({rollout.deadline_s} s) before {rolling['image']} was available"]
            else:
                problems = self.pods_proof(rolling)
        except KubeRefused as refused:
            problems = [f"reconcile could not follow the rollout: {refused}"]
        if problems:
            rolling["problems"] = problems
            self.store.save(self.state)
            return self.roll_back_pods(rolling)
        self.state["pods"]["rolling"] = None
        self.state["pods"]["applied"] = {"commit": rolling["commit"], "image": rolling["image"]}
        lines = [
            f"infra main {short}: the pods service runs {rolling['image']}, proven: the rollout completed, the node reaches pods, and the running pod's imageID is {digest_of(rolling['image'])}.",
            f"Run the functional probe now (one exec, a tmpfs scratch input, one notify post). If it fails, roll back by hand to {rolling['before']}.",
        ]
        self.report(f"pods-proven-{short}", lines, mention=self.prober)

    def pods_proof(self, rolling):
        problems = []
        egress_text = self.state["main_text"]
        if egress_text is None:
            problems.append("infra holds no egress file, so the node's reach to pods cannot be read")
        else:
            try:
                said = self.cli.egress_set(egress_text, dry=True)["reach"].get("pods", "not in the egress file")
            except Refused as refused:
                said = f"the node refused the dry run: {refused}"
            if not said.startswith("answered"):
                problems.append(f"the node does not reach pods: {said}")
        digest = digest_of(rolling["image"])
        running = [pod for pod in self.kube.pods() if not pod["metadata"].get("deletionTimestamp")]
        ids = [held.get("imageID", "") for pod in running for held in pod.get("status", {}).get("containerStatuses", []) if held["name"] == PODS_CONTAINER]
        if not ids or any(not image_id.endswith(f"@{digest}") for image_id in ids):
            problems.append(f"the running pods service runs {', '.join(ids) or 'no image'}, not {digest}")
        return problems

    def roll_back_pods(self, rolling):
        short = rolling["commit"][:12]
        problems = rolling["problems"]
        try:
            deployment = self.kube.deployment()
            if container_image(deployment["spec"]["template"]) != rolling["before"]:
                self.kube.patch(pods_patch(rolling["before"], rolling["before_out"], deployment["metadata"]["resourceVersion"]), dry_run=False)
        except KubeRefused as refused:
            self.state["pods"]["rolling"] = None
            return self.report(f"pods-stuck-{short}", [f"infra main {short}: rolled the pods service to {rolling['image']}, its proof failed, and the rollback to {rolling['before']} was refused: {refused}", *problems])
        self.state["pods"]["rolling"] = None
        self.report(f"pods-rolled-back-{short}", [f"infra main {short}: rolled the pods service to {rolling['image']}, its proof failed, so it runs {rolling['before']} again. Revert {short} in infra.", *problems])

    def follow_main(self):
        tip = self.forge.tip()
        if tip != self.state["seen_tip"]:
            text = self.forge.file(tip, self.path)
            self.state["main_text"] = text
            if text is None:
                self.report(f"absent-{tip[:12]}", [f"infra main {tip[:12]} holds no {self.path}; egress is left as it is."])
            else:
                self.apply(tip, text)
            self.state["setting"] = None
            self.state["seen_tip"] = tip
        if tip != self.state["pods"]["seen_tip"]:
            self.follow_pods(tip, self.forge.file(tip, PODS_PATH))

    def flush(self):
        left = []
        for item in self.state["outbox"]:
            item = {**item, "tried": item["tried"] + 1}
            try:
                landed = self.cli.query("chat", {"message_by_id": {"message_id": item["id"]}}).get("message") is not None
                if not landed:
                    self.cli.op("chat", self.notice_op(item))
            except Unread as error:
                if worth_a_line(item["tried"]):
                    say("report_unposted", id=item["id"], attempts=item["tried"], detail=str(error))
                left.append(item)
        self.state["outbox"] = left

    def tick(self):
        try:
            self.roll()
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
    reconcile = Reconcile(env["RECONCILE_MODE"], env["RECONCILE_NODE_NAME"], node_key(status), env["RECONCILE_CHANNEL"], cli, Forge(cli), store, Kube(), int(env["RECONCILE_PROBER"]), concerns_of(env.get("RECONCILE_CONCERNS", "")))
    say("reconciling", mode=reconcile.mode, path=reconcile.path, pods=PODS_PATH, channel=reconcile.channel, prober=reconcile.prober, concerns=reconcile.concerns)
    while True:
        reconcile.tick()
        time.sleep(block_time_s(status))


if __name__ == "__main__":
    main()
