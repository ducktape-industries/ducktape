"""Exercise watch against a recorded /v1/status and simulated node, Kubernetes API, disks and CLI: each check, each transition, and posts that land, fail or land unseen; run by `make test`."""
import importlib.util
import json
import pathlib
import tempfile
import unittest
from collections import namedtuple

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("watch", HERE / "watch.py")
watch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(watch)
watch.say = lambda event, **fields: None

STATUS = "080000006475636b706f6e64c036714aa1010000e8030000000000004000000000000000dc13020000000000dfc0f2967d951a2b07c824d9ab7806acc4819901eddb9cc59c9682cb42bd2596ea3b3309239a3defceeb6060eefeb6a8b5e446f8c8fdcf7a6357c417943a9d984f080000000000002000000075bf265d1e35103677d009f4fbba0401996c437f56e0481b3a9f9c1e53afe37d09000000725bcad16a1005eef8efe8db1d84ec5d58d00a6b4fd38d6446e5a3d845f71fb5090000000600000061636365737306000000616363657373060000006177616974730600000061776169747305000000626c6f627305000000626c6f627306000000656772657373060000006567726573730400000068616c6f0400000068616c6f080000006964656e74697479080000006964656e746974790800000070726f6772616d730800000070726f6772616d73090000007363686564756c6572090000007363686564756c65720a00000076616c696461746f72730600000076616c736574"
NODE = "http://node"
PUBLIC = "https://public"
CCS = "http://ccs"
Stat = namedtuple("Stat", "f_blocks f_bfree f_bavail f_frsize")
WARM_FILE = "/shared/tools/warm-pod"
WARM = "duckpond-warm-20261021"
WARM_PATH = f"/api/v1/namespaces/agents/pods/{WARM}"
SNAPSHOT = {"Sha256": "a3cce76a042711ab8d3bad219c9d95a1e32ea3ce754c7d25fe17ff0a24a0c8e3"}
RUNNING = {"running": {"startedAt": "2026-10-21T07:45:12Z"}}
OOM = {"terminated": {"exitCode": 137, "reason": "OOMKilled", "startedAt": "2026-10-20T08:02:41Z", "finishedAt": "2026-10-21T07:30:06Z", "containerID": "containerd://5e1c"}}
NOT_FOUND = json.dumps({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure", "message": f'pods "{WARM}" not found', "reason": "NotFound", "details": {"name": WARM, "kind": "pods"}, "code": 404}).encode()


HOVER = [
    (1792575516746, 9_900_000_000),
    (1792575526765, 10_300_000_000),
    (1792578379568, 9_990_000_000),
    (1792578878133, 10_100_000_000),
    (1792578938200, 9_900_000_000),
    (1792578988255, 10_200_000_000),
    (1792579038288, 9_990_000_000),
    (1792579148261, 10_200_000_000),
    (1792579398542, 9_990_000_000),
    (1792579478625, 10_100_000_000),
    (1792579658739, 9_990_000_000),
    (1792579718799, 10_100_000_000),
    (1792579877985, 9_900_000_000),
    (1792579928038, 10_100_000_000),
    (1792579963110, 9_900_000_000),
    (1792580088243, 10_100_000_000),
    (1792580278565, 9_900_000_000),
    (1792580288580, 10_100_000_000),
    (1792580328619, 9_900_000_000),
    (1792580348643, 10_100_000_000),
    (1792580518830, 9_900_000_000),
    (1792580558874, 10_100_000_000),
    (1792580658698, 9_990_000_000),
]
NODE_LOW = {"check": "disk-node-low", "label": "node volume", "path": "/disks/node", "rule": "free_bytes_under", "limit": 10_000_000_000, "clear": 10_500_000_000}


def hover_samples():
    for (at, free), (until, _) in zip(HOVER, HOVER[1:]):
        yield from [free] * round((until - at) / (watch.SAMPLE_S * 1000))
    yield HOVER[-1][1]


class Killed(Exception):
    pass


def status(height):
    data = bytes.fromhex(STATUS)
    return data[:36] + height.to_bytes(8, "little") + data[44:]


def account(label, status="allowed", windows=()):
    rows = [{"name": name, "status": seen, "utilization": used} for name, seen, used in windows]
    return {"account": label, "label": label, "status": status, "windows": rows}


BUDGET = {"accounts": [account("anthropic3-eph", windows=[("5h", "allowed", "0.26"), ("7d", "allowed", "0.33"), ("Fable weekly_scoped", None, "0")])]}


def deployment(ready, reason="MinimumReplicasAvailable"):
    return {"status": {"conditions": [{"type": "Available", "status": "True" if ready else "False", "reason": reason, "message": "m"}]}}


def file_entry(path, held):
    if held is None:
        return None
    return {"meta": [], "node": {"File": {"exec": False, "file": {"Sha256": "2fa87f0d297ac133358c60908e6e726948d2c768c35b6bff2ae0627b4d818e6c"}, "size": len(held)}}, "path": path}


def warm_pod(run):
    containers = [
        {"name": "out", "ready": True, "restartCount": 0, "state": RUNNING, "lastState": {}},
        {"name": "run", "ready": "running" in run, "restartCount": 0, "state": run, "lastState": {}},
    ]
    pod = {"kind": "Pod", "apiVersion": "v1", "metadata": {"name": WARM, "namespace": "agents"}, "status": {"phase": "Running", "containerStatuses": containers}}
    return json.dumps(pod).encode()


class FakeHttp:
    def __init__(self, answers):
        self.answers = answers

    def get(self, url, headers=None, context=None):
        answer = self.answers[url]
        if isinstance(answer, Exception):
            raise answer
        return answer


class FakeKube:
    def __init__(self, answers):
        self.answers = answers

    def get(self, path):
        answer = self.answers[path]
        if isinstance(answer, Exception):
            raise answer
        return answer


class FakeCli:
    def __init__(self, budget=BUDGET, files=None):
        self.budget = budget
        self.files = {} if files is None else files
        self.reads = []
        self.posted = {}
        self.ops = []
        self.failing = False
        self.landing_unseen = False
        self.dying = False
        self.stalled = False

    def query(self, program, query, confirmed=False):
        if program == "harness":
            if isinstance(self.budget, Exception):
                raise self.budget
            return {"Budget": self.budget}
        if program == "files":
            return self.files_answer(query)
        found = self.posted.get(query["MessageById"]["message_id"])
        return {"Message": None if found is None else {"text": found}}

    def files_answer(self, query):
        if isinstance(self.files, Exception):
            raise self.files
        self.reads.append(query)
        match query:
            case {"Stat": {"at": None, "path": path}}:
                return {"Stat": {"entry": file_entry(path, self.files.get(path)), "snapshot": SNAPSHOT}}
            case {"Read": {"at": at, "len": length, "offset": offset, "path": path}}:
                held = self.files[path]
                part = held[offset : offset + length]
                return {"Contents": {"bytes": part.hex(), "eof": offset + length >= len(held), "offset": offset, "size": len(held), "snapshot": at}}

    def op(self, program, op):
        self.ops.append(op)
        posted = op["PostMessage"]
        if self.dying:
            self.posted[posted["message_id"]] = posted["blocks"][0]["Paragraph"][0]["text"]
            raise Killed()
        if self.landing_unseen:
            self.posted[posted["message_id"]] = posted["blocks"][0]["Paragraph"][0]["text"]
            raise watch.Unread("the answer was lost")
        if self.failing:
            raise watch.Unread("unlanded")
        if self.stalled:
            return {"pooled": posted["message_id"]}
        self.posted[posted["message_id"]] = posted["blocks"][0]["Paragraph"][0]["text"]
        return {"pooled": posted["message_id"]}


def ready_kube(**more):
    return FakeKube({"/readyz": (200, b"ok"), "/apis/apps/v1/namespaces/duckpond/deployments/node": (200, json.dumps(deployment(True)).encode()), **more})


class Height(unittest.TestCase):
    def test_the_height_reads_from_a_recorded_status(self):
        self.assertEqual(watch.height_of(bytes.fromhex(STATUS)), 136156)

    def test_a_status_too_short_for_a_height_is_bad(self):
        reading, height = watch.status_reading("node", "node", FakeHttp({NODE + "/v1/status": (200, b"\x08\0\0\0duckpond")}), NODE)
        self.assertEqual((reading.seen, height), (watch.BAD, None))

    def test_an_unanswered_or_refused_status_is_bad(self):
        for answer in (watch.Unread("refused"), (503, b"")):
            reading, _ = watch.status_reading("node", "node", FakeHttp({NODE + "/v1/status": answer}), NODE)
            self.assertEqual(reading.seen, watch.BAD)


class Rate(unittest.TestCase):
    def test_the_rate_waits_for_a_whole_window(self):
        samples = []
        for second in (0, 10, 20, 30):
            samples, reading = watch.rate_reading(samples, second * 1000, 100 + second)
            self.assertEqual(reading.seen, watch.UNREAD)
        samples, reading = watch.rate_reading(samples, 40_000, 140)
        self.assertEqual(reading.seen, watch.OK)
        self.assertIn("1.00 blocks/s over the last 40 s", reading.value)

    def test_a_rate_under_the_floor_is_bad_and_old_samples_go(self):
        samples = [(0, 100), (10_000, 103), (20_000, 106), (30_000, 109)]
        samples, reading = watch.rate_reading(samples, 40_000, 112)
        self.assertEqual(reading.seen, watch.BAD)
        self.assertIn("0.30 blocks/s", reading.value)
        samples, _ = watch.rate_reading(samples, 50_000, 115)
        self.assertEqual(samples[0], (10_000, 103))

    def test_a_stall_reads_as_zero(self):
        samples = [(0, 100), (40_000, 100)]
        _, reading = watch.rate_reading(samples, 80_000, 100)
        self.assertEqual((reading.seen, reading.value[:4]), (watch.BAD, "0.00"))

    def test_no_height_is_unread_and_starts_the_window_again(self):
        samples, reading = watch.rate_reading([(0, 1)], 50_000, None)
        self.assertEqual((reading.seen, samples), (watch.UNREAD, []))

    def test_a_node_back_from_a_restart_posts_no_stale_stall(self):
        samples = []
        for second, height in ((0, 100), (40, 140)):
            samples, reading = watch.rate_reading(samples, second * 1000, height)
        self.assertEqual(reading.seen, watch.OK)
        samples, reading = watch.rate_reading(samples, 640_000, None)
        for second, height in ((650, 150), (680, 180)):
            samples, reading = watch.rate_reading(samples, second * 1000, height)
            self.assertEqual(reading.seen, watch.UNREAD)
        samples, reading = watch.rate_reading(samples, 690_000, 190)
        self.assertEqual(reading.seen, watch.OK)


class Public(unittest.TestCase):
    def test_the_public_url_is_judged_only_while_the_node_answers(self):
        http = FakeHttp({PUBLIC + "/v1/status": watch.Unread("tunnel down")})
        down = watch.Reading("node", "node", watch.BAD, "")
        up = watch.Reading("node", "node", watch.OK, "")
        self.assertEqual(watch.public_reading(http, PUBLIC, down).seen, watch.UNREAD)
        self.assertEqual(watch.public_reading(http, PUBLIC, up).seen, watch.BAD)


class Ccs(unittest.TestCase):
    def test_ccs_is_up_while_it_answers_under_500(self):
        cases = [((401, b""), watch.OK), ((200, b"{}"), watch.OK), ((503, b""), watch.BAD), (watch.Unread("refused"), watch.BAD)]
        for answer, seen in cases:
            self.assertEqual(watch.ccs_reading(FakeHttp({CCS + "/ccs/status": answer}), CCS).seen, seen)


class Budget(unittest.TestCase):
    def test_every_allowed_window_is_ok(self):
        self.assertEqual(watch.budget_reading(BUDGET).seen, watch.OK)

    def test_a_warning_names_the_account_and_the_window(self):
        report = {"accounts": [account("ext.5", windows=[("7d", "allowed_warning", "0.91")]), account("eph", status="rejected")]}
        reading = watch.budget_reading(report)
        self.assertEqual(reading.seen, watch.BAD)
        self.assertEqual(reading.value, "ext.5's 7d window is allowed_warning at 0.91; eph is rejected")

    def test_harness_unanswered_is_unread(self):
        self.assertEqual(watch.budget_read(FakeCli(budget=watch.Unread("no"))).seen, watch.UNREAD)


class Disks(unittest.TestCase):
    def test_free_bytes_are_dfs_avail(self):
        disk = watch.Disk("disk-node-low", "node volume", "/disks/node", "free_bytes_under", 10_000_000_000)
        enough = Stat(7_250_000, 2_500_000, 2_441_407, 4096)
        short = Stat(7_250_000, 2_500_000, 2_441_406, 4096)
        self.assertEqual(watch.disk_reading(disk, enough).seen, watch.OK)
        reading = watch.disk_reading(disk, short)
        self.assertEqual(reading.seen, watch.BAD)
        self.assertIn("bad under 10 GB free", reading.value)

    def test_a_free_percent_is_the_kubelets_nodefs_available(self):
        disk = watch.Disk("disk-root", "root", "/disks/root", "free_percent_under", 5)
        self.assertEqual(watch.disk_reading(disk, Stat(100, 20, 5, 4096)).seen, watch.OK)
        self.assertEqual(watch.disk_reading(disk, Stat(100, 20, 4, 4096)).seen, watch.BAD)

    def test_a_used_percent_is_the_kubelets_image_gc_usage(self):
        disk = watch.Disk("disk-home", "rlm-home", "/disks/home", "used_percent_at", 85)
        self.assertEqual(watch.disk_reading(disk, Stat(100, 20, 15, 4096)).seen, watch.BAD)
        self.assertEqual(watch.disk_reading(disk, Stat(100, 20, 16, 4096)).seen, watch.OK)

    def test_a_row_with_a_clear_line_turns_good_only_past_it_and_bad_only_past_its_line(self):
        disk = watch.Disk.of(NODE_LOW)

        def seen(free, posted):
            return watch.disk_reading(disk, Stat(29_700_000_000, 0, free, 1), posted).seen

        self.assertEqual([seen(10_200_000_000, posted) for posted in (None, watch.OK, watch.BAD)], [watch.OK, watch.OK, watch.BAD])
        self.assertEqual([seen(free, watch.BAD) for free in (10_499_999_999, 10_500_000_000)], [watch.BAD, watch.OK])
        self.assertEqual(seen(9_999_999_999, watch.OK), watch.BAD)
        self.assertIn("bad under 10 GB free, good again at 10.5 GB free", watch.disk_reading(disk, Stat(29_700_000_000, 0, 1, 1)).value)
        used = watch.Disk.of({"check": "disk-home", "label": "h", "path": "/disks/home", "rule": "used_percent_at", "limit": 85, "clear": 80})
        self.assertEqual([watch.disk_reading(used, Stat(100, 0, free, 4096), watch.BAD).seen for free in (19, 21)], [watch.BAD, watch.OK])
        self.assertIn("bad at 85% used, good again under 80% used", watch.disk_reading(used, Stat(100, 0, 21, 4096)).value)

    def test_a_clear_line_on_the_bad_side_of_its_line_is_refused_at_start(self):
        for rule, limit, clear in (("free_bytes_under", 10, 9), ("free_bytes_under", 10, 10), ("free_percent_under", 5, 4), ("used_percent_at", 85, 90)):
            with self.assertRaises(ValueError):
                watch.Disk.of({"check": "d", "label": "d", "path": "/", "rule": rule, "limit": limit, "clear": clear})

    def test_an_unknown_rule_is_refused_at_start(self):
        with self.assertRaises(ValueError):
            watch.Disk.of({"check": "d", "label": "d", "path": "/", "rule": "most", "limit": 1})

    def test_a_missing_mount_is_unread(self):
        def gone(path):
            raise FileNotFoundError(path)

        reading = watch.disk_read(watch.Disk("disk-root", "root", "/nowhere", "free_percent_under", 5), gone)
        self.assertEqual(reading.seen, watch.UNREAD)


class Deployments(unittest.TestCase):
    def test_available_decides(self):
        api = watch.Reading("kube", "k", watch.OK, "")
        path = "/apis/apps/v1/namespaces/pods/deployments/pods"
        for answer, seen in (((200, json.dumps(deployment(True)).encode()), watch.OK), ((200, json.dumps(deployment(False, "MinimumReplicasUnavailable")).encode()), watch.BAD), ((404, b""), watch.BAD)):
            reading = watch.deployment_reading(FakeKube({path: answer}), api, "pods/pods")
            self.assertEqual((reading.check, reading.seen), ("deployment-pods-pods", seen))

    def test_an_api_not_ready_leaves_deployments_unread(self):
        api = watch.api_reading(FakeKube({"/readyz": watch.Unread("refused")}))
        self.assertEqual(api.seen, watch.BAD)
        self.assertEqual(watch.deployment_reading(FakeKube({}), api, "pods/pods").seen, watch.UNREAD)


API_UP = watch.Reading("kube", "Kubernetes API", watch.OK, "")


def warm_read(files, answer, api=API_UP):
    cli = FakeCli(files=files)
    return watch.warm_pod_reading(cli, FakeKube({WARM_PATH: answer}), api, WARM_FILE, "agents"), cli


class WarmPod(unittest.TestCase):
    named = {WARM_FILE: f"{WARM}\n".encode()}

    def test_a_running_run_container_is_ok_and_the_file_is_read_at_the_size_and_snapshot_stat_answered(self):
        reading, cli = warm_read(self.named, (200, warm_pod(RUNNING)))
        self.assertEqual((reading.check, reading.seen, reading.value), ("warm-pod", watch.OK, f"{WARM}'s run container is running"))
        self.assertEqual(cli.reads, [{"Stat": {"at": None, "path": WARM_FILE}}, {"Read": {"at": SNAPSHOT, "len": 23, "offset": 0, "path": WARM_FILE}}])

    def test_a_terminated_run_container_is_bad_with_its_reason_and_exit_code_while_out_runs_on(self):
        reading, _ = warm_read(self.named, (200, warm_pod(OOM)))
        self.assertEqual((reading.seen, reading.value), (watch.BAD, f"{WARM}'s run container ended: OOMKilled, exit 137"))

    def test_a_named_pod_the_api_answers_404_for_is_bad(self):
        reading, _ = warm_read(self.named, (404, NOT_FOUND))
        self.assertEqual((reading.seen, reading.value), (watch.BAD, f"{WARM_FILE} names {WARM}, which namespace agents does not hold (404)"))

    def test_an_empty_or_missing_file_is_bad(self):
        for files in ({WARM_FILE: b""}, {WARM_FILE: b"\n"}, {}):
            reading, _ = warm_read(files, watch.Unread("asked for no pod"))
            self.assertEqual((reading.seen, reading.value), (watch.BAD, f"{WARM_FILE} names no pod"), files)

    def test_a_failed_files_or_api_read_is_unread(self):
        api_down = watch.Reading("kube", "Kubernetes API", watch.BAD, "")
        cases = [
            (watch.Unread("files did not answer"), (200, warm_pod(RUNNING)), API_UP),
            (self.named, watch.Unread("connection refused"), API_UP),
            (self.named, (503, b""), API_UP),
            (self.named, (403, b""), API_UP),
            (self.named, (200, b"<html>"), API_UP),
            (self.named, (200, warm_pod(RUNNING)), api_down),
        ]
        for files, answer, api in cases:
            reading, _ = warm_read(files, answer, api)
            self.assertEqual(reading.seen, watch.UNREAD, (answer, reading.value))


def reading(seen, check="rate"):
    return watch.Reading(check, "block rate", seen, f"value {seen}")


class Holds(unittest.TestCase):
    def test_the_checks_with_a_line_and_the_flaky_ones_wait_turn_after_samples_and_the_rest_one(self):
        _, rate = watch.rate_reading([(0, 100)], 40_000, 140)
        disks = [watch.disk_reading(watch.Disk("d", "d", "/d", rule, 5), Stat(100, 20, 5, 4096)) for rule in watch.RULES]
        public = watch.public_reading(FakeHttp({PUBLIC + "/v1/status": (200, status(1))}), PUBLIC, watch.Reading("node", "node", watch.OK, ""))
        warm, _ = warm_read({WARM_FILE: f"{WARM}\n".encode()}, (200, warm_pod(RUNNING)))
        node, _ = watch.status_reading("node", "node", FakeHttp({NODE + "/v1/status": (200, status(1))}), NODE)
        ccs = watch.ccs_reading(FakeHttp({CCS + "/ccs/status": (200, b"{}")}), CCS)
        self.assertEqual([held.after for held in (rate, *disks, public, warm)], [watch.TURN_AFTER] * 6)
        self.assertEqual([at_once.after for at_once in (node, ccs, watch.budget_reading(BUDGET))], [1, 1, 1])


class Decide(unittest.TestCase):
    def test_only_transitions_post(self):
        state = watch.fresh_state()
        state = watch.decide(state, [reading(watch.OK)], 1)
        self.assertEqual(state["outbox"], [])
        state = watch.decide(state, [reading(watch.BAD)], 2)
        state = watch.decide(state, [reading(watch.BAD)], 3)
        state = watch.decide(state, [reading(watch.UNREAD)], 4)
        state = watch.decide(state, [reading(watch.OK)], 5)
        texts = [item["text"] for item in state["outbox"]]
        self.assertEqual(texts, ["🔴 block rate: value bad", "🟢 block rate is good again: value ok"])
        self.assertEqual([item["id"] for item in state["outbox"]], ["watch-rate-bad-2", "watch-rate-ok-5"])

    def test_a_held_check_turns_bad_on_its_second_bad_sample_in_a_row_good_on_its_second_good_one_and_the_node_on_its_first(self):
        def public(seen):
            return watch.Reading("public", "public /v1/status", seen, f"value {seen}", watch.TURN_AFTER)

        state = watch.fresh_state()
        for at, seen in enumerate((watch.OK, watch.BAD, watch.OK, watch.BAD, watch.UNREAD, watch.BAD)):
            state = watch.decide(state, [public(seen)], at)
        self.assertEqual(state["outbox"], [])
        state = watch.decide(state, [public(watch.BAD)], 6)
        state = watch.decide(state, [public(watch.BAD)], 7)
        self.assertEqual([item["id"] for item in state["outbox"]], ["watch-public-bad-6"])
        for at, seen in enumerate((watch.OK, watch.BAD, watch.OK, watch.UNREAD, watch.OK), start=8):
            state = watch.decide(state, [public(seen)], at)
        self.assertEqual([item["id"] for item in state["outbox"]], ["watch-public-bad-6"])
        state = watch.decide(state, [public(watch.OK)], 13)
        self.assertEqual([item["id"] for item in state["outbox"]], ["watch-public-bad-6", "watch-public-ok-13"])
        state = watch.decide(state, [reading(watch.BAD, "node")], 14)
        self.assertEqual((state["outbox"][-1]["id"], state["streaks"]), ("watch-node-bad-14", {}))

    def test_a_state_saved_before_streaks_loads_with_none_and_decides(self):
        with tempfile.TemporaryDirectory() as temp:
            path = pathlib.Path(temp) / "state.json"
            path.write_text(json.dumps({"checks": {}, "unread": [], "outbox": []}))
            old = watch.Store(path).load()
        state = watch.decide(old, [watch.Reading("public", "public /v1/status", watch.BAD, "v", watch.TURN_AFTER)], 1)
        self.assertEqual((state["outbox"], state["streaks"]), ([], {"public": 1}))

    def test_a_check_bad_from_the_start_posts(self):
        state = watch.decide(watch.fresh_state(), [reading(watch.BAD, "ccs")], 7)
        self.assertEqual(len(state["outbox"]), 1)

    def test_unread_never_starts_a_check_and_is_logged_once_as_it_turns(self):
        logged = []
        watch.say = lambda event, **fields: logged.append(event)
        try:
            state = watch.decide(watch.fresh_state(), [reading(watch.UNREAD)], 7)
            state = watch.decide(state, [reading(watch.UNREAD)], 8)
            state = watch.decide(state, [reading(watch.OK)], 9)
            state = watch.decide(state, [reading(watch.UNREAD)], 10)
        finally:
            watch.say = lambda event, **fields: None
        self.assertEqual(logged, ["check_unread", "check_unread"])
        self.assertEqual((state["outbox"], state["unread"]), ([], ["rate"]))


class Ran:
    def __init__(self, code):
        self.returncode = code
        self.stderr = b""


class FakePager:
    def __init__(self, codes=()):
        self.codes = list(codes)
        self.pages = []

    def __call__(self, argv, input, capture_output):
        self.pages.append((argv, input.decode()))
        return Ran(self.codes.pop(0) if self.codes else 0)


class Blocking(unittest.TestCase):
    def test_a_cli_call_that_does_not_end_within_a_sample_is_unread_and_posts_wait_on_no_block(self):
        seen = []

        def blocks(argv, capture_output, env, timeout):
            seen.append((argv, timeout))
            raise watch.subprocess.TimeoutExpired(argv, timeout)

        with tempfile.TemporaryDirectory() as temp:
            keys = pathlib.Path(temp)
            (keys / "password").write_text("p")
            cli = watch.Cli(["ducktape"], NODE, keys, keys, blocks)
            with self.assertRaises(watch.Unread):
                cli.op("chat", {"PostMessage": {}})
            with self.assertRaises(watch.Unread):
                cli.query("chat", {"MessageById": {"message_id": "x"}}, confirmed=True)
        self.assertEqual([timeout for _, timeout in seen], [watch.SAMPLE_S, watch.SAMPLE_S])
        self.assertNotIn("--land", seen[0][0])
        self.assertIn("--confirmed", seen[1][0])


class Flush(unittest.TestCase):
    def item(self):
        return {"id": "watch-rate-bad-2", "text": "t", "tried": 0}

    def test_a_post_leaves_the_outbox_once_a_block_carries_it_and_pages_nobody(self):
        cli = FakeCli()
        runner = FakePager()
        pager = watch.Pager(["page"], runner)
        left = watch.flush(cli, "alerts", [self.item()], pager)
        self.assertTrue(left[0]["sent"])
        self.assertEqual(watch.flush(cli, "alerts", left, pager), [])
        self.assertEqual(cli.ops[0]["PostMessage"]["channel_id"], "alerts")
        self.assertEqual(len(cli.ops), 1)
        self.assertEqual(runner.pages, [])

    def test_a_chain_that_stalls_pages_at_the_next_sample_and_never_blocks_the_flush(self):
        cli = FakeCli()
        cli.stalled = True
        runner = FakePager()
        pager = watch.Pager(["page", "orthory"], runner)
        left = watch.flush(cli, "alerts", [self.item()], pager)
        self.assertEqual(runner.pages, [])
        left = watch.flush(cli, "alerts", left, pager)
        left = watch.flush(cli, "alerts", left, pager)
        self.assertEqual(runner.pages, [(["page", "orthory"], "t")])
        self.assertEqual(len(cli.ops), 3)
        cli.stalled = False
        left = watch.flush(cli, "alerts", left, pager)
        self.assertEqual(watch.flush(cli, "alerts", left, pager), [])

    def test_a_post_that_landed_unseen_is_read_back_not_resent(self):
        cli = FakeCli()
        cli.landing_unseen = True
        left = watch.flush(cli, "alerts", [self.item()], watch.Pager([]))
        self.assertEqual(left[0]["tried"], 1)
        cli.landing_unseen = False
        self.assertEqual(watch.flush(cli, "alerts", left, watch.Pager([])), [])
        self.assertEqual(len(cli.ops), 1)

    def test_a_post_that_did_not_land_is_sent_again(self):
        cli = FakeCli()
        cli.failing = True
        left = watch.flush(cli, "alerts", [self.item()], watch.Pager([]))
        cli.failing = False
        left = watch.flush(cli, "alerts", left, watch.Pager([]))
        self.assertEqual(watch.flush(cli, "alerts", left, watch.Pager([])), [])
        self.assertEqual(len(cli.ops), 2)

    def logged_through(self, run):
        logged = []
        watch.say = lambda event, **fields: logged.append((event, fields.get("attempts")))
        try:
            run()
        finally:
            watch.say = lambda event, **fields: None
        return logged

    def test_a_chat_that_stays_down_logs_at_attempts_1_10_and_20(self):
        class Down(FakeCli):
            def query(self, program, query, confirmed=False):
                raise watch.Unread("connection refused")

        cli = Down()
        left = [self.item()]

        def run():
            nonlocal left
            for _ in range(25):
                left = watch.flush(cli, "alerts", left, watch.Pager([], FakePager()))

        logged = self.logged_through(run)
        unposted = [attempts for event, attempts in logged if event == "alert_unposted"]
        self.assertEqual(unposted, [1, 10, 20])
        self.assertEqual(left[0]["tried"], 25)
        self.assertEqual(cli.ops, [])

    def test_after_chat_fails_once_the_rest_of_the_flush_is_paged_without_calling_chat(self):
        class Down(FakeCli):
            calls = 0

            def query(self, program, query, confirmed=False):
                Down.calls += 1
                raise watch.Unread("timed out")

        cli = Down()
        runner = FakePager()
        items = [{"id": f"watch-{n}", "text": f"t{n}", "tried": 0} for n in range(5)]
        left = watch.flush(cli, "alerts", items, watch.Pager(["page"], runner))
        self.assertEqual(Down.calls, 1)
        self.assertEqual([text for _, text in runner.pages], ["t0", "t1", "t2", "t3", "t4"])
        self.assertEqual(len(left), 5)

    def test_a_pager_that_keeps_failing_logs_at_pages_1_10_and_20(self):
        cli = FakeCli()
        cli.failing = True
        runner = FakePager(codes=[1] * 25)
        left = [self.item()]

        def run():
            nonlocal left
            for _ in range(25):
                left = watch.flush(cli, "alerts", left, watch.Pager(["page"], runner))

        logged = self.logged_through(run)
        failed = [attempts for event, attempts in logged if event == "page_failed"]
        self.assertEqual(failed, [1, 10, 20])
        self.assertEqual(len(runner.pages), 25)

    def test_an_unposted_alert_is_paged_once_and_again_only_after_a_failed_page(self):
        cli = FakeCli()
        cli.failing = True
        runner = FakePager(codes=[1, 0])
        pager = watch.Pager(["page", "orthory"], runner)
        left = [self.item()]
        for _ in range(3):
            left = watch.flush(cli, "alerts", left, pager)
        self.assertEqual(runner.pages, [(["page", "orthory"], "t"), (["page", "orthory"], "t")])
        self.assertTrue(left[0]["paged"])


class Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


class Whole(unittest.TestCase):
    def test_a_stall_posts_once_and_its_end_posts_once(self):
        config = watch.Config(NODE, PUBLIC, "", "alerts", (), ("duckpond/node",))
        heights = iter([100, 110, 120, 130, 140, 140, 140, 140, 140, 150, 160, 170, 180])
        http = FakeHttp({PUBLIC + "/v1/status": (200, status(1))})
        cli = FakeCli()
        clock = Clock()
        with tempfile.TemporaryDirectory() as temp:
            store = watch.Store(pathlib.Path(temp) / "state.json")
            watcher = watch.Watch(config, http, ready_kube(), cli, store, watch.Pager([]), clock)
            for _ in range(13):
                http.answers[NODE + "/v1/status"] = (200, status(next(heights)))
                watcher.tick()
                clock.now += watch.SAMPLE_S
            texts = [op["PostMessage"]["blocks"][0]["Paragraph"][0]["text"] for op in cli.ops]
            self.assertEqual(len(texts), 2)
            self.assertTrue(texts[0].startswith("🔴 block rate: 0.25 blocks/s"))
            self.assertTrue(texts[1].startswith("🟢 block rate is good again"))
            self.assertEqual(json.loads(store.path.read_text())["checks"]["rate"]["seen"], watch.OK)

    def test_one_failed_public_sample_is_silent_and_two_in_a_row_post_across_a_restart(self):
        config = watch.Config(NODE, PUBLIC, "", "alerts", (), ())
        up, down = (200, status(1)), watch.Unread("timed out")
        http = FakeHttp({NODE + "/v1/status": (200, status(1))})
        cli = FakeCli()
        with tempfile.TemporaryDirectory() as temp:
            path = pathlib.Path(temp) / "state.json"

            def tick(public):
                http.answers[PUBLIC + "/v1/status"] = public
                watch.Watch(config, http, ready_kube(), cli, watch.Store(path), watch.Pager([]), Clock()).tick()

            for public in (up, down, up, down):
                tick(public)
            self.assertEqual(cli.ops, [])
            tick(down)
            tick(up)
            self.assertEqual(len(cli.ops), 1)
            tick(up)
            texts = [op["PostMessage"]["blocks"][0]["Paragraph"][0]["text"] for op in cli.ops]
            self.assertEqual(texts, ["🔴 public /v1/status: no answer: timed out", "🟢 public /v1/status is good again: height 1"])

    def test_a_post_that_landed_before_the_pod_stopped_is_not_posted_again(self):
        config = watch.Config(NODE, PUBLIC, "", "alerts", (), ())
        http = FakeHttp({NODE + "/v1/status": watch.Unread("down"), PUBLIC + "/v1/status": (200, status(1))})
        cli = FakeCli()
        cli.dying = True
        with tempfile.TemporaryDirectory() as temp:
            path = pathlib.Path(temp) / "state.json"
            with self.assertRaises(Killed):
                watch.Watch(config, http, ready_kube(), cli, watch.Store(path), watch.Pager([]), Clock()).tick()
            cli.dying = False
            watch.Watch(config, http, ready_kube(), cli, watch.Store(path), watch.Pager([]), Clock()).tick()
            self.assertEqual(len(cli.ops), 1)
            self.assertEqual(json.loads(path.read_text())["outbox"], [])


    def test_a_dead_run_container_posts_on_its_second_bad_sample_and_good_again_on_its_second_good_one(self):
        config = watch.Config(NODE, PUBLIC, "", "alerts", (), (), warm_pod_file=WARM_FILE, warm_pod_namespace="agents")
        http = FakeHttp({NODE + "/v1/status": (200, status(1)), PUBLIC + "/v1/status": (200, status(1))})
        cli = FakeCli(files={WARM_FILE: f"{WARM}\n".encode()})
        kube = ready_kube()
        posted = []
        with tempfile.TemporaryDirectory() as temp:
            watcher = watch.Watch(config, http, kube, cli, watch.Store(pathlib.Path(temp) / "state.json"), watch.Pager([]), Clock())
            for run in (RUNNING, OOM, OOM, OOM, RUNNING, RUNNING):
                kube.answers[WARM_PATH] = (200, warm_pod(run))
                watcher.tick()
                posted.append(len(cli.ops))
        texts = [op["PostMessage"]["blocks"][0]["Paragraph"][0]["text"] for op in cli.ops]
        self.assertEqual(posted, [0, 0, 1, 1, 1, 2])
        self.assertEqual(texts, [f"🔴 warm pod: {WARM}'s run container ended: OOMKilled, exit 137", f"🟢 warm pod is good again: {WARM}'s run container is running"])

    def test_a_disk_hovering_on_its_line_posts_bad_once_and_good_again_only_after_as_many_good_samples_in_a_row(self):
        disk = watch.Disk("disk-node-low", "node volume", "/disks/node", "free_bytes_under", 10_000_000_000)
        config = watch.Config(NODE, PUBLIC, "", "alerts", (disk,), ())
        http = FakeHttp({NODE + "/v1/status": (200, status(1)), PUBLIC + "/v1/status": (200, status(1))})
        cli = FakeCli()
        over, under = Stat(7_250_000, 2_500_000, 2_441_407, 4096), Stat(7_250_000, 2_500_000, 2_441_406, 4096)
        free = [over]
        posted = []
        with tempfile.TemporaryDirectory() as temp:
            store = watch.Store(pathlib.Path(temp) / "state.json")
            watcher = watch.Watch(config, http, ready_kube(), cli, store, watch.Pager([]), Clock(), lambda path: free[0])
            for sample in (over, under, over, under, under, over, under, over, under, over, over):
                free[0] = sample
                watcher.tick()
                posted.append(len(cli.ops))
        texts = [op["PostMessage"]["blocks"][0]["Paragraph"][0]["text"] for op in cli.ops]
        self.assertEqual(posted, [0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 2])
        shown = "10.0 GB free of 29.7 GB (33% free), bad under 10 GB free"
        self.assertEqual(texts, [f"🔴 node volume: {shown}", f"🟢 node volume is good again: {shown}"])

    def test_the_node_volume_replaying_its_real_hover_posts_bad_once_and_good_again_once_over_its_clear_line(self):
        config = watch.Config(NODE, PUBLIC, "", "alerts", (watch.Disk.of(NODE_LOW),), ())
        http = FakeHttp({PUBLIC + "/v1/status": (200, status(1))})
        cli = FakeCli()
        clock = Clock()
        free = [0]
        samples = [10_300_000_000, *hover_samples(), 10_600_000_000, 10_600_000_000]
        posted = []
        with tempfile.TemporaryDirectory() as temp:
            store = watch.Store(pathlib.Path(temp) / "state.json")
            watcher = watch.Watch(config, http, ready_kube(), cli, store, watch.Pager([]), clock, lambda path: Stat(29_700_000_000, 0, free[0], 1))
            for at, sample in enumerate(samples):
                free[0] = sample
                http.answers[NODE + "/v1/status"] = (200, status(at * 10))
                watcher.tick()
                clock.now += watch.SAMPLE_S
                posted.append(len(cli.ops))
        texts = [op["PostMessage"]["blocks"][0]["Paragraph"][0]["text"] for op in cli.ops]
        self.assertEqual(posted[-3:], [1, 1, 2])
        line = "bad under 10 GB free, good again at 10.5 GB free"
        self.assertEqual(texts, [f"🔴 node volume: 10.0 GB free of 29.7 GB (33% free), {line}", f"🟢 node volume is good again: 10.6 GB free of 29.7 GB (35% free), {line}"])

if __name__ == "__main__":
    unittest.main()
