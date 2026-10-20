"""Exercise reconcile against a simulated forge, node and Kubernetes: the refusal of secrets, verdicts on open changes, apply once per main, proof from the chain's record, the rollback when a base stops answering, and the pods image roll with its watch, proof and rollback; run by `make test`."""
import copy
import importlib.util
import io
import json
import pathlib
import urllib.error
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("reconcile", HERE / "reconcile.py")
rc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rc)
rc.say = lambda event, **fields: None

STATUS = "080000006475636b706f6e64c036714aa1010000e8030000000000004000000000000000dc13020000000000dfc0f2967d951a2b07c824d9ab7806acc4819901eddb9cc59c9682cb42bd2596ea3b3309239a3defceeb6060eefeb6a8b5e446f8c8fdcf7a6357c417943a9d984f080000000000002000000075bf265d1e35103677d009f4fbba0401996c437f56e0481b3a9f9c1e53afe37d09000000725bcad16a1005eef8efe8db1d84ec5d58d00a6b4fd38d6446e5a3d845f71fb5090000000600000061636365737306000000616363657373060000006177616974730600000061776169747305000000626c6f627305000000626c6f627306000000656772657373060000006567726573730400000068616c6f0400000068616c6f080000006964656e74697479080000006964656e746974790800000070726f6772616d730800000070726f6772616d73090000007363686564756c6572090000007363686564756c65720a00000076616c696461746f72730600000076616c736574"
KEY = "75bf265d1e35103677d009f4fbba0401996c437f56e0481b3a9f9c1e53afe37d"
LIVE = 'relay = true\n[services.pods]\nbase = "http://10.43.80.80:8080"\n'
SECRETED = LIVE + '[services.anthropic]\nbase = "http://10.43.41.41:4141"\nsecrets = { x-api-key = "CCS_KEY" }\n'
WITH_MAC = LIVE + '[services.mac]\nbase = "http://10.43.80.81:8080"\n'
NO_PODS = 'relay = true\n[services.mac]\nbase = "http://10.43.80.81:8080"\n'
PROBER = 17


def image(letter):
    return f"localhost:5000/pods:{letter * 12}@sha256:{letter * 64}"


POD_A, POD_B, POD_C = image("a"), image("b"), image("c")


def pods_toml(declared):
    return f'image = "{declared}"\n'


def services(text):
    return rc.served(rc.tomllib.loads(text))


class FakeNode:
    def __init__(self, text=LIVE):
        self.text = text
        self.recorded = services(text)
        self.down = set()
        self.refuse = None
        self.sets = []
        self.silent = False
        self.lose = 0
        self.cut = rc.Unread

    def egress_set(self, text, dry):
        if self.silent:
            raise rc.Unread("error sending request: connection refused")
        if self.refuse:
            raise rc.Refused(self.refuse)
        document = rc.tomllib.loads(text)
        was, now = set(services(self.text)), set(services(text))
        tally = {"served": len(now), "added": len(now - was), "removed": len(was - now), "changed": 0}
        reach = {name: "failed: refused" if name in self.down else "answered 404" for name in document.get("services", {})}
        if not dry:
            self.sets.append(text)
            self.text = text
            if self.lose:
                self.lose -= 1
                raise self.cut("the node's answer broke off")
        return {"tally": tally, "reach": reach}


def deployment_of(running):
    env = [{"name": "PODS_OUT_IMAGE", "value": running}, {"name": "PODS_NAMESPACE", "value": "agents"}]
    return {
        "metadata": {"name": "pods", "generation": 1, "resourceVersion": "100", "labels": {"app": "pods"}, "annotations": {}},
        "spec": {"replicas": 1, "progressDeadlineSeconds": 600, "template": {"spec": {"containers": [{"name": "pods", "image": running, "env": env}]}}},
        "status": {"observedGeneration": 1, "replicas": 1, "updatedReplicas": 1, "availableReplicas": 1, "conditions": []},
    }


class FakeKube:
    def __init__(self, running=POD_A):
        self.live = deployment_of(running)
        self.pod_image = running
        self.patches = []
        self.dry_runs = 0
        self.streams = []
        self.watched = []
        self.refuse = None
        self.stray = False
        self.version = 1000

    def deployment(self):
        return copy.deepcopy(self.live)

    def patch(self, body, dry_run):
        if self.refuse:
            raise rc.KubeRefused(self.refuse)
        if body["metadata"]["resourceVersion"] != self.live["metadata"]["resourceVersion"]:
            raise rc.Unread("the Kubernetes API answered 409 Conflict")
        held = copy.deepcopy(self.live)
        asked = body["spec"]["template"]["spec"]["containers"][0]
        container = held["spec"]["template"]["spec"]["containers"][0]
        container["image"] = asked["image"]
        for env in asked.get("env", []):
            for have in container["env"]:
                if have["name"] == env["name"]:
                    have["value"] = env["value"]
        if self.stray:
            held["spec"]["replicas"] = 2
        if dry_run:
            self.dry_runs += 1
            return held
        held["metadata"]["generation"] += 1
        held["metadata"]["resourceVersion"] = str(int(held["metadata"]["resourceVersion"]) + 1)
        self.patches.append(asked["image"])
        self.live = held
        return copy.deepcopy(held)

    def progress(self, counts=(1, 1, 1), exceeded=False):
        held = copy.deepcopy(self.live)
        self.version += 1
        held["metadata"]["resourceVersion"] = str(self.version)
        conditions = [{"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded"}] if exceeded else []
        held["status"] = {"observedGeneration": held["metadata"]["generation"], "replicas": counts[0], "updatedReplicas": counts[1], "availableReplicas": counts[2], "conditions": conditions}
        return {"type": "MODIFIED", "object": copy.deepcopy(held)}

    def watch(self, resource_version):
        self.watched.append(resource_version)
        if not self.streams:
            raise AssertionError("a watch opened with no stream scripted")
        yield from self.streams.pop(0)

    def pods(self):
        status = {"containerStatuses": [{"name": "pods", "imageID": f"localhost:5000/pods@{rc.digest_of(self.pod_image)}"}]}
        return [{"metadata": {"name": "pods-1"}, "status": status}, {"metadata": {"name": "pods-0", "deletionTimestamp": "now"}, "status": {}}]


class FakeForge:
    def __init__(self, main_files):
        self.main = "a" * 40
        self.files = {self.main: main_files}
        self.changes = {}
        self.verdicts = []
        self.contexts = []

    def merge(self, commit, files):
        self.main = commit
        self.files[commit] = files

    def tip(self):
        return self.main

    def file(self, commit, path):
        return self.files[commit].get(path)

    def open_changes(self):
        return list(self.changes)

    def head(self, n):
        return self.changes[n]

    def judge(self, n, head, passed, body, context):
        self.verdicts.append((n, head, passed, body))
        self.contexts.append(context)


class FakeCli:
    def __init__(self, node):
        self.node = node
        self.posts = []
        self.mentions = []

    def egress_set(self, text, dry):
        return self.node.egress_set(text, dry)

    def query(self, program, query):
        if program == "egress":
            return {"Node": {"key": KEY, "services": self.node.recorded, "since": 1}}
        return {"Message": None}

    def op(self, program, op):
        spans = op["PostMessage"]["blocks"][0]["Paragraph"]
        self.posts.append("".join(span["text"] for span in spans))
        self.mentions.append([mark["Mention"]["Account"] for span in spans for mark in span["marks"] if "Mention" in mark])
        return {"landed": 1}


PATH = "egress/fabrics-1.toml"
PODS = "pods.toml"


def world(main_text=LIVE, mode="act", pods_text=None):
    node = FakeNode()
    cli = FakeCli(node)
    files = {PATH: main_text, **({} if pods_text is None else {PODS: pods_text})}
    forge = FakeForge(files)
    kube = FakeKube()
    temp = tempfile.TemporaryDirectory()
    store = rc.Store(pathlib.Path(temp.name) / "state.json")
    reconcile = rc.Reconcile(mode, "fabrics-1", KEY, "infra", cli, forge, store, kube, PROBER)
    return node, cli, forge, kube, reconcile, temp


class Ran:
    def __init__(self, code, stdout=b"", stderr=b""):
        self.returncode = code
        self.stdout = stdout
        self.stderr = stderr


class RealForge(unittest.TestCase):
    def forge(self, answer):
        temp = tempfile.TemporaryDirectory()
        keys = pathlib.Path(temp.name)
        (keys / "password").write_text("p")
        cli = rc.Cli(["ducktape"], "http://node", keys, keys, lambda argv, capture_output, env: answer)
        return rc.Forge(cli), temp

    def test_a_folder_forge_has_no_entry_for_reads_as_an_absent_file(self):
        forge, temp = self.forge(Ran(1, stderr=b"refused: not_found: no entry at this path"))
        with temp:
            self.assertIsNone(forge.file("a" * 40, PATH))

    def test_a_forge_that_does_not_answer_stays_unread(self):
        forge, temp = self.forge(Ran(1, stderr=b"error sending request: connection refused"))
        with temp:
            with self.assertRaises(rc.Unread) as raised:
                forge.file("a" * 40, PATH)
            self.assertIsNone(raised.exception.reason)

    def test_a_main_with_no_egress_folder_still_judges_the_change_that_adds_it(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            real = forge.file

            def foldered(commit, path):
                if commit == forge.main:
                    return None
                return real(commit, path)

            forge.file = foldered
            forge.files["3" * 40] = {PATH: WITH_MAC}
            forge.changes[9] = "3" * 40
            reconcile.tick()
            self.assertTrue(forge.verdicts[0][2])


class RealCli(unittest.TestCase):
    def cli(self, answer):
        temp = tempfile.TemporaryDirectory()
        keys = pathlib.Path(temp.name)
        (keys / "password").write_text("p")
        return rc.Cli(["ducktape"], "http://node", keys, keys, lambda argv, capture_output, env: answer), temp

    def test_a_node_that_does_not_answer_a_set_is_unread_and_a_refusal_is_refused(self):
        cli, temp = self.cli(Ran(1, stderr=b"error sending request for url (http://node/v1/egress/set): connection refused"))
        with temp:
            with self.assertRaises(rc.Unread):
                cli.egress_set(LIVE, dry=True)
        cli, temp = self.cli(Ran(1, stderr=b"refused: invalid_input: echo carries secrets"))
        with temp:
            with self.assertRaises(rc.Refused):
                cli.egress_set(LIVE, dry=True)


class Answer(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


class RealKube(unittest.TestCase):
    def kube(self, *answers):
        temp = tempfile.TemporaryDirectory()
        mount = pathlib.Path(temp.name)
        (mount / "token").write_text("t\n")
        asked = []
        left = list(answers)

        def opener(request, context):
            asked.append(request)
            answer = left.pop(0)
            if isinstance(answer, Exception):
                raise answer
            return Answer(answer)

        environ = {"KUBERNETES_SERVICE_HOST": "10.43.0.1", "KUBERNETES_SERVICE_PORT": "443"}
        return rc.Kube(mount, environ, opener, lambda cafile: None), asked, temp

    def refusal(self, code):
        status = json.dumps({"reason": "Why", "message": f"answered {code}"}).encode()
        return urllib.error.HTTPError("https://10.43.0.1", code, "Why", {}, io.BytesIO(status))

    def test_a_dry_run_patch_is_a_strategic_merge_patch_with_its_field_manager_and_bearer_token(self):
        kube, asked, temp = self.kube(json.dumps(deployment_of(POD_B)).encode())
        with temp:
            held = kube.patch(rc.pods_patch(POD_B, POD_B, "100"), dry_run=True)
            self.assertEqual(rc.container_image(held["spec"]["template"]), POD_B)
            request = asked[0]
            self.assertEqual(request.get_method(), "PATCH")
            self.assertEqual(request.full_url, "https://10.43.0.1:443/apis/apps/v1/namespaces/pods/deployments/pods?fieldManager=reconcile&dryRun=All")
            self.assertEqual(request.get_header("Content-type"), "application/strategic-merge-patch+json")
            self.assertEqual(request.get_header("Authorization"), "Bearer t")
            self.assertEqual(json.loads(request.data)["metadata"]["resourceVersion"], "100")

    def test_a_watch_names_the_deployment_and_its_version_and_reads_one_event_a_line(self):
        lines = b"".join(json.dumps({"type": "MODIFIED", "object": {"n": n}}).encode() + b"\n" for n in (1, 2))
        kube, asked, temp = self.kube(lines)
        with temp:
            self.assertEqual([event["object"]["n"] for event in kube.watch("7")], [1, 2])
            self.assertTrue(asked[0].full_url.endswith("/deployments?watch=true&fieldSelector=metadata.name%3Dpods&resourceVersion=7"))

    def test_each_refusal_reads_as_what_reconcile_does_with_it(self):
        kube, asked, temp = self.kube(self.refusal(409), self.refusal(410), self.refusal(403), urllib.error.URLError("connection refused"))
        with temp:
            with self.assertRaises(rc.Unread):
                kube.deployment()
            with self.assertRaises(rc.Gone):
                list(kube.watch("1"))
            with self.assertRaises(rc.KubeRefused) as refused:
                kube.deployment()
            self.assertIn("403 Why: answered 403", str(refused.exception))
            with self.assertRaises(rc.Unread):
                kube.pods()


class Status(unittest.TestCase):
    def test_the_node_key_and_block_time_read_from_a_recorded_status(self):
        status = bytes.fromhex(STATUS)
        self.assertEqual(rc.node_key(status), KEY)
        self.assertEqual(rc.block_time_s(status), 1.0)


class Secrets(unittest.TestCase):
    def test_every_service_that_carries_secrets_is_named(self):
        filed = SECRETED + '[services.mac]\nbase = "http://10.43.80.81:8080"\nsecrets = { authorization = { file = "mac.key" } }\n'
        self.assertEqual(
            rc.secreted(rc.tomllib.loads(filed)),
            ["anthropic carries secrets, which the node never takes through egress set", "mac carries secrets, which the node never takes through egress set"],
        )
        self.assertEqual(rc.secreted(rc.tomllib.loads(LIVE)), [])


class Mode(unittest.TestCase):
    def test_report_mode_sends_only_dry_sets_and_says_what_it_would_apply(self):
        node, cli, forge, kube, reconcile, temp = world(mode="report")
        with temp:
            sent = []
            real = node.egress_set

            def watched(text, dry):
                sent.append(dry)
                return real(text, dry)

            cli.egress_set = watched
            reconcile.tick()
            forge.merge("7" * 40, {PATH: WITH_MAC})
            forge.files["6" * 40] = {PATH: WITH_MAC}
            forge.changes[7] = "6" * 40
            reconcile.tick()
            self.assertTrue(sent)
            self.assertTrue(all(sent))
            self.assertEqual(node.sets, [])
            self.assertIn("would apply egress (3 served: 1 added", cli.posts[0])
            self.assertIsNone(reconcile.state["proving"])

    def test_a_switch_to_act_applies_the_main_report_mode_only_reported_once(self):
        node, cli, forge, kube, reconcile, temp = world(mode="report")
        with temp:
            reconcile.tick()
            forge.merge("7" * 40, {PATH: WITH_MAC})
            reconcile.tick()
            self.assertEqual(node.sets, [])
            acting = rc.Reconcile("act", "fabrics-1", KEY, "infra", cli, forge, rc.Store(reconcile.store.path), kube, PROBER)
            acting.tick()
            acting.tick()
            self.assertEqual(node.sets, [WITH_MAC])
            self.assertIn("egress applied (3 served: 1 added", cli.posts[1])
            again = rc.Reconcile("act", "fabrics-1", KEY, "infra", cli, forge, rc.Store(reconcile.store.path), kube, PROBER)
            again.tick()
            self.assertEqual(node.sets, [WITH_MAC])
            self.assertEqual(len(cli.posts), 2)

    def test_a_switch_to_act_rolls_the_image_report_mode_only_reported(self):
        node, cli, forge, kube, reconcile, temp = world(pods_text=pods_toml(POD_A), mode="report")
        with temp:
            reconcile.tick()
            forge.merge("e" * 40, {PATH: LIVE, PODS: pods_toml(POD_B)})
            reconcile.tick()
            self.assertEqual(kube.patches, [])
            acting = rc.Reconcile("act", "fabrics-1", KEY, "infra", cli, forge, rc.Store(reconcile.store.path), kube, PROBER)
            acting.tick()
            self.assertEqual(kube.patches, [POD_B])

    def test_a_main_reported_in_this_mode_is_not_followed_again_at_a_restart(self):
        node, cli, forge, kube, reconcile, temp = world(mode="report")
        with temp:
            forge.merge("7" * 40, {PATH: WITH_MAC})
            reconcile.tick()
            sent = []
            real = node.egress_set

            def watched(text, dry):
                sent.append(dry)
                return real(text, dry)

            cli.egress_set = watched
            again = rc.Reconcile("report", "fabrics-1", KEY, "infra", cli, forge, rc.Store(reconcile.store.path), kube, PROBER)
            again.tick()
            self.assertEqual(sent, [])

    def test_an_unknown_mode_is_refused_at_start(self):
        with self.assertRaises(ValueError):
            rc.Reconcile("acts", "fabrics-1", KEY, "infra", None, None, None, None, PROBER)


class Shape(unittest.TestCase):
    def test_a_misshapen_file_fails_its_change_and_reconcile_keeps_running(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            for n, text in ((4, "services = 1\n"), (5, '[services]\npods = "x"\n'), (6, '[services.pods]\nbase = "http://x"\nsecrets = 1\n')):
                forge.files[str(n) * 40] = {PATH: text}
                forge.changes[n] = str(n) * 40
            reconcile.tick()
            self.assertEqual([passed for _, _, passed, _ in forge.verdicts], [False, False, False])
            self.assertTrue(all("does not parse" in body for _, _, _, body in forge.verdicts))
            self.assertEqual(node.sets, [])

    def test_a_file_that_is_not_utf8_fails_its_change_and_its_main_and_the_state_saves(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            raw = b"\xff\xfe".decode(errors="surrogateescape")
            forge.files["5" * 40] = {PATH: raw}
            forge.changes[8] = "5" * 40
            reconcile.tick()
            forge.merge("4" * 40, {PATH: raw})
            reconcile.tick()
            self.assertEqual(forge.verdicts[0][2:], (False, "reconcile/egress: FAIL\n- the file is not UTF-8"))
            self.assertIn("the file is not UTF-8", cli.posts[0])
            self.assertEqual(node.sets, [])
            self.assertEqual(rc.Store(reconcile.store.path).load()["seen_tip"], "4" * 40)


class Unreadable(unittest.TestCase):
    def test_a_forge_that_stays_unread_logs_at_attempts_1_10_and_20_and_once_when_read_again(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            def down():
                raise rc.Unread("forge did not answer")

            tip = forge.tip
            forge.tip = down
            logged = []
            rc.say = lambda event, **fields: logged.append((event, fields.get("attempts")))
            try:
                for _ in range(25):
                    reconcile.tick()
                forge.tip = tip
                reconcile.tick()
                reconcile.tick()
            finally:
                rc.say = lambda event, **fields: None
            self.assertEqual([attempts for event, attempts in logged if event == "unread"], [1, 10, 20])
            self.assertEqual([attempts for event, attempts in logged if event == "read"], [25])


class Flush(unittest.TestCase):
    def test_a_chat_that_stays_down_logs_at_attempts_1_10_and_20(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            def down(program, query):
                raise rc.Unread("connection refused")

            cli.query = down
            reconcile.report("k", ["line"])
            logged = []
            rc.say = lambda event, **fields: logged.append((event, fields.get("attempts")))
            try:
                for _ in range(25):
                    reconcile.flush()
            finally:
                rc.say = lambda event, **fields: None
            self.assertEqual([attempts for event, attempts in logged if event == "report_unposted"], [1, 10, 20])
            self.assertEqual(cli.posts, [])
            self.assertEqual(reconcile.state["outbox"][0]["tried"], 25)


class Changes(unittest.TestCase):
    def test_an_open_change_gets_one_verdict_per_head_and_nothing_is_applied(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            forge.files["b" * 40] = {PATH: WITH_MAC}
            forge.changes[1] = "b" * 40
            reconcile.tick()
            reconcile.tick()
            self.assertEqual(len(forge.verdicts), 1)
            n, head, passed, body = forge.verdicts[0]
            self.assertTrue(passed)
            self.assertIn("1 added", body)
            self.assertIn("mac: answered 404", body)
            self.assertEqual(forge.contexts, ["reconcile/egress"])
            self.assertEqual(node.sets, [])

    def test_a_change_carrying_a_secret_fails_without_asking_the_node(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            forge.files["c" * 40] = {PATH: SECRETED}
            forge.changes[2] = "c" * 40
            node.refuse = "the node must not be asked"
            reconcile.tick()
            self.assertFalse(forge.verdicts[0][2])
            self.assertIn("anthropic carries secrets", forge.verdicts[0][3])

    def test_a_base_that_does_not_answer_fails_the_change(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            node.down.add("mac")
            forge.files["d" * 40] = {PATH: WITH_MAC}
            forge.changes[3] = "d" * 40
            reconcile.tick()
            self.assertFalse(forge.verdicts[0][2])


class Main(unittest.TestCase):
    def test_main_as_it_runs_applies_nothing(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            self.assertEqual(node.sets, [])
            self.assertEqual(reconcile.state["applied"]["commit"], forge.main)

    def test_a_new_main_applies_once_and_is_proven_by_the_chains_record(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("e" * 40, {PATH: WITH_MAC})
            reconcile.tick()
            reconcile.tick()
            self.assertEqual(node.sets, [WITH_MAC])
            self.assertIsNotNone(reconcile.state["proving"])
            node.recorded = services(WITH_MAC)
            reconcile.tick()
            self.assertIsNone(reconcile.state["proving"])
            self.assertEqual(len(cli.posts), 2)
            self.assertIn("proven", cli.posts[1])

    def test_a_base_that_stops_answering_after_the_set_rolls_back(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("f" * 40, {PATH: WITH_MAC})
            sets = []
            real = node.egress_set

            def flaky(text, dry):
                if dry and sets:
                    node.down.add("mac")
                if not dry:
                    sets.append(text)
                return real(text, dry)

            cli.egress_set = flaky
            reconcile.tick()
            self.assertEqual(node.sets, [WITH_MAC, LIVE])
            self.assertEqual(reconcile.state["applied"]["text"], LIVE)
            self.assertIn("serves aaaaaaaaaaaa again", cli.posts[0])

    def test_a_refused_set_leaves_the_node_and_reports(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("9" * 40, {PATH: WITH_MAC})
            real = node.egress_set

            def refusing(text, dry):
                if not dry:
                    raise rc.Refused("mac's base does not parse")
                return real(text, dry)

            cli.egress_set = refusing
            reconcile.tick()
            self.assertEqual(node.text, LIVE)
            self.assertIn("refused", cli.posts[0])

    def test_a_main_merged_while_the_node_does_not_answer_applies_once_it_answers(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("e" * 40, {PATH: WITH_MAC})
            node.silent = True
            reconcile.tick()
            self.assertEqual((node.sets, cli.posts), ([], []))
            node.silent = False
            reconcile.tick()
            self.assertEqual(node.sets, [WITH_MAC])
            self.assertIn("egress applied (3 served: 1 added", cli.posts[0])

    def test_a_set_whose_answer_was_lost_is_sent_again_and_then_reported(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("e" * 40, {PATH: WITH_MAC})
            node.lose = 1
            reconcile.tick()
            self.assertEqual((node.sets, cli.posts), ([WITH_MAC], []))
            reconcile.tick()
            reconcile.tick()
            self.assertEqual(node.sets, [WITH_MAC, WITH_MAC])
            self.assertEqual(len(cli.posts), 1)
            self.assertIn("egress applied (3 served: 1 added", cli.posts[0])
            self.assertEqual(reconcile.state["proving"]["commit"], "e" * 40)

    def test_a_reconcile_killed_right_after_its_set_sends_it_again_at_the_restart_and_reports_it_once(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("e" * 40, {PATH: WITH_MAC})
            node.lose, node.cut = 1, RuntimeError
            with self.assertRaises(RuntimeError):
                reconcile.tick()
            again = rc.Reconcile("act", "fabrics-1", KEY, "infra", cli, forge, rc.Store(reconcile.store.path), kube, PROBER)
            again.tick()
            again.tick()
            self.assertEqual(node.sets, [WITH_MAC, WITH_MAC])
            self.assertEqual(len(cli.posts), 1)
            self.assertIn("egress applied (3 served: 1 added", cli.posts[0])
            self.assertEqual(again.state["proving"]["commit"], "e" * 40)

    def test_a_resend_whose_base_stops_answering_after_it_rolls_back_to_the_file_the_first_send_replaced(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("e" * 40, {PATH: WITH_MAC})
            steps = ["answer", "unread", "answer", "down"]
            real = node.egress_set

            def scripted(text, dry):
                step = steps.pop(0) if dry and steps else "answer"
                if step == "unread":
                    raise rc.Unread("the node's answer broke off")
                if step == "down":
                    node.down.add("mac")
                return real(text, dry)

            cli.egress_set = scripted
            reconcile.tick()
            reconcile.tick()
            self.assertEqual(node.sets, [WITH_MAC, WITH_MAC, LIVE])
            self.assertEqual(reconcile.state["applied"]["text"], LIVE)
            self.assertIn("serves aaaaaaaaaaaa again", cli.posts[0])

    def test_a_lost_set_whose_base_then_stops_answering_rolls_back_to_the_file_it_replaced(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            reconcile.tick()
            forge.merge("e" * 40, {PATH: WITH_MAC})
            node.lose = 1
            reconcile.tick()
            node.down.add("mac")
            reconcile.tick()
            self.assertEqual(node.sets, [WITH_MAC, LIVE])
            self.assertEqual(reconcile.state["applied"]["text"], LIVE)
            self.assertIn("serves aaaaaaaaaaaa again", cli.posts[0])

    def test_a_main_without_the_file_reports_and_touches_nothing(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            forge.merge("8" * 40, {})
            reconcile.tick()
            self.assertEqual(node.sets, [])
            self.assertIn("holds no egress/fabrics-1.toml", cli.posts[0])


class PodsChanges(unittest.TestCase):
    def change(self, forge, n, pods_text, egress_text=None):
        files = {PODS: pods_text, **({} if egress_text is None else {PATH: egress_text})}
        forge.files[str(n) * 40] = files
        forge.changes[n] = str(n) * 40

    def test_a_change_to_the_image_passes_on_a_dry_run_that_changes_only_the_image_and_its_out_image(self):
        node, cli, forge, kube, reconcile, temp = world(pods_text=pods_toml(POD_A))
        with temp:
            self.change(forge, 1, pods_toml(POD_B))
            reconcile.tick()
            reconcile.tick()
            self.assertEqual(forge.contexts, ["reconcile/pods"])
            n, head, passed, body = forge.verdicts[0]
            self.assertTrue(passed)
            self.assertIn(f"rolls {POD_A} to {POD_B}", body)
            self.assertIn("spec.template.spec.containers.0.env.0.value, spec.template.spec.containers.0.image", body)
            self.assertEqual(kube.patches, [])

    def test_an_image_that_is_not_a_pushed_digest_fails_without_asking_kubernetes(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            kube.refuse = "Kubernetes must not be asked"
            self.change(forge, 2, pods_toml("localhost:5000/pods:latest"))
            self.change(forge, 3, 'image = "x"\nreplicas = 2\n')
            reconcile.tick()
            self.assertEqual([passed for _, _, passed, _ in forge.verdicts], [False, False])
            self.assertIn("is not localhost:5000/pods:<tag>@sha256:<digest>", forge.verdicts[0][3])
            self.assertIn("holds one string, image", forge.verdicts[1][3])

    def test_a_dry_run_that_changes_more_than_the_image_fails(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            kube.stray = True
            self.change(forge, 4, pods_toml(POD_B))
            reconcile.tick()
            self.assertFalse(forge.verdicts[0][2])
            self.assertIn("spec.replicas", forge.verdicts[0][3])

    def test_a_change_whose_egress_serves_no_pods_fails_since_the_proof_could_not_run(self):
        node, cli, forge, kube, reconcile, temp = world()
        with temp:
            self.change(forge, 5, pods_toml(POD_B), NO_PODS)
            reconcile.tick()
            pods_verdicts = [verdict for verdict, context in zip(forge.verdicts, forge.contexts) if context == "reconcile/pods"]
            self.assertFalse(pods_verdicts[0][2])
            self.assertIn("serves no pods service", pods_verdicts[0][3])


class PodsRoll(unittest.TestCase):
    def rolled(self, mode="act"):
        node, cli, forge, kube, reconcile, temp = world(pods_text=pods_toml(POD_A), mode=mode)
        reconcile.tick()
        forge.merge("e" * 40, {PATH: LIVE, PODS: pods_toml(POD_B)})
        reconcile.tick()
        return node, cli, forge, kube, reconcile, temp

    def test_main_as_the_deployment_runs_it_rolls_nothing(self):
        node, cli, forge, kube, reconcile, temp = world(pods_text=pods_toml(POD_A))
        with temp:
            reconcile.tick()
            self.assertEqual(kube.patches, [])
            self.assertEqual(reconcile.state["pods"]["applied"]["image"], POD_A)
            self.assertEqual(cli.posts, [])

    def test_a_new_image_rolls_once_and_is_proven_by_the_watch_the_node_and_the_image_id(self):
        node, cli, forge, kube, reconcile, temp = self.rolled()
        with temp:
            self.assertEqual(kube.patches, [POD_B])
            self.assertEqual(rc.out_image(kube.live["spec"]["template"]), POD_B)
            kube.pod_image = POD_B
            kube.streams = [[kube.progress(counts=(2, 1, 0)), kube.progress()]]
            reconcile.tick()
            self.assertEqual(kube.watched, [kube.live["metadata"]["resourceVersion"]])
            self.assertEqual(kube.patches, [POD_B])
            self.assertIsNone(reconcile.state["pods"]["rolling"])
            self.assertEqual(reconcile.state["pods"]["applied"]["image"], POD_B)
            self.assertIn(f"runs {POD_B}, proven", cli.posts[0])
            self.assertIn(f"roll back by hand to {POD_A}", cli.posts[0])
            self.assertEqual(cli.mentions[0], [PROBER])
            reconcile.tick()
            self.assertEqual(len(cli.posts), 1)

    def test_a_rollout_past_its_progress_deadline_patches_back_the_image_it_replaced(self):
        node, cli, forge, kube, reconcile, temp = self.rolled()
        with temp:
            kube.streams = [[kube.progress(counts=(2, 1, 0), exceeded=True)]]
            reconcile.tick()
            self.assertEqual(kube.patches, [POD_B, POD_A])
            self.assertEqual(rc.out_image(kube.live["spec"]["template"]), POD_A)
            self.assertIn("progress deadline (600 s)", cli.posts[0])
            self.assertIn(f"runs {POD_A} again. Revert eeeeeeeeeeee in infra.", cli.posts[0])
            self.assertEqual(cli.mentions[0], [])
            self.assertIsNone(reconcile.state["pods"]["rolling"])

    def test_a_node_that_does_not_reach_the_new_pod_rolls_back(self):
        node, cli, forge, kube, reconcile, temp = self.rolled()
        with temp:
            kube.pod_image = POD_B
            node.down.add("pods")
            kube.streams = [[kube.progress()]]
            reconcile.tick()
            self.assertEqual(kube.patches, [POD_B, POD_A])
            self.assertIn("the node does not reach pods: failed: refused", cli.posts[0])

    def test_a_running_pod_on_another_digest_rolls_back(self):
        node, cli, forge, kube, reconcile, temp = self.rolled()
        with temp:
            kube.streams = [[kube.progress()]]
            reconcile.tick()
            self.assertEqual(kube.patches, [POD_B, POD_A])
            self.assertIn(f"runs localhost:5000/pods@sha256:{'a' * 64}, not sha256:{'b' * 64}", cli.posts[0])

    def test_a_watch_whose_version_is_gone_reads_the_deployment_again_and_watches_on(self):
        node, cli, forge, kube, reconcile, temp = self.rolled()
        with temp:
            kube.pod_image = POD_B
            gone = {"type": "ERROR", "object": {"kind": "Status", "code": 410, "message": "too old resource version"}}
            kube.streams = [[gone], [kube.progress()]]
            reconcile.tick()
            self.assertEqual(len(kube.watched), 2)
            self.assertIn("proven", cli.posts[0])

    def test_a_restart_during_the_rollout_follows_it_from_the_saved_state(self):
        node, cli, forge, kube, reconcile, temp = self.rolled()
        with temp:
            again = rc.Reconcile("act", "fabrics-1", KEY, "infra", cli, forge, rc.Store(reconcile.store.path), kube, PROBER)
            kube.pod_image = POD_B
            kube.streams = [[kube.progress()]]
            again.tick()
            self.assertEqual(kube.patches, [POD_B])
            self.assertIn("proven", cli.posts[0])

    def test_a_deployment_someone_else_changed_is_left_alone(self):
        node, cli, forge, kube, reconcile, temp = self.rolled()
        with temp:
            kube.live["spec"]["template"]["spec"]["containers"][0]["image"] = POD_C
            reconcile.tick()
            self.assertEqual(kube.patches, [POD_B])
            self.assertIn(f"runs {POD_C}, not {POD_B}", cli.posts[0])
            self.assertIsNone(reconcile.state["pods"]["rolling"])

    def test_a_refused_roll_leaves_the_deployment_and_reports(self):
        node, cli, forge, kube, reconcile, temp = world(pods_text=pods_toml(POD_A))
        with temp:
            reconcile.tick()
            forge.merge("9" * 40, {PATH: LIVE, PODS: pods_toml(POD_B)})
            real = kube.patch

            def refusing(body, dry_run):
                if not dry_run:
                    raise rc.KubeRefused("the Kubernetes API answered 403 Forbidden")
                return real(body, dry_run)

            kube.patch = refusing
            reconcile.tick()
            self.assertEqual(kube.patches, [])
            self.assertIn("Kubernetes refused the roll", cli.posts[0])
            self.assertIsNone(reconcile.state["pods"]["rolling"])

    def test_report_mode_dry_runs_the_roll_and_says_what_it_would_do(self):
        node, cli, forge, kube, reconcile, temp = self.rolled(mode="report")
        with temp:
            reconcile.tick()
            self.assertEqual(kube.patches, [])
            self.assertGreater(kube.dry_runs, 0)
            self.assertIn(f"would roll the pods service from {POD_A} to {POD_B}", cli.posts[0])


if __name__ == "__main__":
    unittest.main()
