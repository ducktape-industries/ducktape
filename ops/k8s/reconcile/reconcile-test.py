"""Exercise reconcile against a simulated forge and node: the refusal of secrets, verdicts on open changes, apply once per main, proof from the chain's record, and the rollback when a base stops answering; run by `make test`."""
import importlib.util
import pathlib
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


def services(text):
    return rc.served(rc.tomllib.loads(text))


class FakeNode:
    def __init__(self, text=LIVE):
        self.text = text
        self.recorded = services(text)
        self.down = set()
        self.refuse = None
        self.sets = []

    def egress_set(self, text, dry):
        if self.refuse:
            raise rc.Refused(self.refuse)
        document = rc.tomllib.loads(text)
        was, now = set(services(self.text)), set(services(text))
        tally = {"served": len(now), "added": len(now - was), "removed": len(was - now), "changed": 0}
        reach = {name: "failed: refused" if name in self.down else "answered 404" for name in document.get("services", {})}
        if not dry:
            self.sets.append(text)
            self.text = text
        return {"tally": tally, "reach": reach}


class FakeForge:
    def __init__(self, main_files):
        self.main = "a" * 40
        self.files = {self.main: main_files}
        self.changes = {}
        self.verdicts = []

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

    def judge(self, n, head, passed, body):
        self.verdicts.append((n, head, passed, body))


class FakeCli:
    def __init__(self, node):
        self.node = node
        self.posts = []

    def egress_set(self, text, dry):
        return self.node.egress_set(text, dry)

    def query(self, program, query):
        if program == "egress":
            return {"Node": {"key": KEY, "services": self.node.recorded, "since": 1}}
        return {"Message": None}

    def op(self, program, op):
        self.posts.append(op["PostMessage"]["blocks"][0]["Paragraph"][0]["text"])
        return {"landed": 1}


PATH = "egress/fabrics-1.toml"


def world(main_text=LIVE, mode="act"):
    node = FakeNode()
    cli = FakeCli(node)
    forge = FakeForge({PATH: main_text})
    temp = tempfile.TemporaryDirectory()
    store = rc.Store(pathlib.Path(temp.name) / "state.json")
    reconcile = rc.Reconcile(mode, "fabrics-1", KEY, "infra", cli, forge, store)
    return node, cli, forge, reconcile, temp


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
        node, cli, forge, reconcile, temp = world(mode="report")
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

    def test_an_unknown_mode_is_refused_at_start(self):
        with self.assertRaises(ValueError):
            rc.Reconcile("acts", "fabrics-1", KEY, "infra", None, None, None)


class Shape(unittest.TestCase):
    def test_a_misshapen_file_fails_its_change_and_reconcile_keeps_running(self):
        node, cli, forge, reconcile, temp = world()
        with temp:
            for n, text in ((4, "services = 1\n"), (5, '[services]\npods = "x"\n'), (6, '[services.pods]\nbase = "http://x"\nsecrets = 1\n')):
                forge.files[str(n) * 40] = {PATH: text}
                forge.changes[n] = str(n) * 40
            reconcile.tick()
            self.assertEqual([passed for _, _, passed, _ in forge.verdicts], [False, False, False])
            self.assertTrue(all("does not parse" in body for _, _, _, body in forge.verdicts))
            self.assertEqual(node.sets, [])

    def test_a_file_that_is_not_utf8_fails_its_change_and_its_main_and_the_state_saves(self):
        node, cli, forge, reconcile, temp = world()
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
        node, cli, forge, reconcile, temp = world()
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
        node, cli, forge, reconcile, temp = world()
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
        node, cli, forge, reconcile, temp = world()
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
            self.assertEqual(node.sets, [])

    def test_a_change_carrying_a_secret_fails_without_asking_the_node(self):
        node, cli, forge, reconcile, temp = world()
        with temp:
            forge.files["c" * 40] = {PATH: SECRETED}
            forge.changes[2] = "c" * 40
            node.refuse = "the node must not be asked"
            reconcile.tick()
            self.assertFalse(forge.verdicts[0][2])
            self.assertIn("anthropic carries secrets", forge.verdicts[0][3])

    def test_a_base_that_does_not_answer_fails_the_change(self):
        node, cli, forge, reconcile, temp = world()
        with temp:
            node.down.add("mac")
            forge.files["d" * 40] = {PATH: WITH_MAC}
            forge.changes[3] = "d" * 40
            reconcile.tick()
            self.assertFalse(forge.verdicts[0][2])


class Main(unittest.TestCase):
    def test_main_as_it_runs_applies_nothing(self):
        node, cli, forge, reconcile, temp = world()
        with temp:
            reconcile.tick()
            self.assertEqual(node.sets, [])
            self.assertEqual(reconcile.state["applied"]["commit"], forge.main)

    def test_a_new_main_applies_once_and_is_proven_by_the_chains_record(self):
        node, cli, forge, reconcile, temp = world()
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
        node, cli, forge, reconcile, temp = world()
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
        node, cli, forge, reconcile, temp = world()
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

    def test_a_main_without_the_file_reports_and_touches_nothing(self):
        node, cli, forge, reconcile, temp = world()
        with temp:
            forge.merge("8" * 40, {})
            reconcile.tick()
            self.assertEqual(node.sets, [])
            self.assertIn("holds no egress/fabrics-1.toml", cli.posts[0])


if __name__ == "__main__":
    unittest.main()
