"""Exercise the signer against a fake network (actions, forge and blobs as the ducktape CLI answers them) and real tool processes; run by `make test`."""
import hashlib
import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("signer", HERE / "signer.py")
signer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(signer)
SAID = []
signer.say = lambda event, **fields: SAID.append((event, fields))
mac_spec = importlib.util.spec_from_file_location("mac", HERE.parent / "mac" / "mac.py")
mac = importlib.util.module_from_spec(mac_spec)
mac_spec.loader.exec_module(mac)

COMMIT = "c0ffee" + "0" * 34
MAC = 41
PODS = 42
STRANGER = 43
KEY = bytes(range(32))
TOOL = """
import json, os, pathlib, sys
inputs, out = map(pathlib.Path, sys.argv[1:])
if os.environ.get("SIGNER_ENV_FAIL"):
    print("security unlock-keychain -p hunter2", file=sys.stderr)
    sys.exit(3)
for path in sorted(inputs.rglob("*")):
    if path.is_file():
        (out / (path.name + ".signed")).write_bytes(b"signed:" + path.read_bytes())
seen = {name: value for name, value in os.environ.items() if name.startswith("SIGNER_") or name in ("DUCKTAPE_USER_KEY", "DUCKTAPE_PASSWORD")}
(out / "env.json").write_text(json.dumps(seen, sort_keys=True))
"""


def ran(out=b"", code=0, err=""):
    return signer.Ran(code, out, err)


def text(value):
    return signer.borsh_text(value)


def framed(data):
    return f"output {len(data)}".encode() + b"\0" + data


def blob_of(data):
    return f"sha256:{hashlib.sha256(framed(data)).hexdigest()}"


def check_row(check, run=1, commit=COMMIT, runs_on=("sign:dev.ducktape.app",), steps=("developer-id",), env=(), state="queued", reporter=None, job="sign", claimant=None):
    body = text(job) + signer.borsh_vec([text(label) for label in runs_on]) + text("") + signer.borsh_vec([text(step) for step in steps])
    body += signer.borsh_vec([text(name) + text(value) for name, value in env]) + signer.borsh_vec([])
    row = signer.u64(check) + signer.u64(run) + text("app") + text(commit) + text("release.toml") + signer.u32(1) + body
    row += signer.u8(signer.CHECK_STATES.index(state)) + signer.borsh_option(claimant, signer.u64) + signer.borsh_option(reporter, signer.u64)
    return row + signer.u64(1) + b"\x00\x00" + text("") + b"\x00"


class Fake:
    def __init__(self):
        self.queue = []
        self.rows = {}
        self.needs = {}
        self.outputs = {}
        self.blobs = {}
        self.on = {"main": {COMMIT}}
        self.submits = []
        self.puts = []
        self.failing = []
        self.taken = set()
        self.unnamed = set()
        self.lost = set()
        self.down = False
        self.accounts = {KEY: 60}

    def key(self):
        return KEY

    def query(self, program, request):
        if self.down:
            return ran(code=1, err="ducktape: IO error: the node is not reachable")
        if program == signer.IDENTITY:
            reader = signer.Reader(request)
            assert reader.u8() == 0
            return ran(signer.u8(0) + signer.borsh_option(self.accounts.get(reader.bytes()), signer.u64))
        if program == signer.FORGE:
            reader = signer.Reader(request)
            assert reader.u8() == 9
            reader.text()
            assert reader.u8() == 1
            commit = reader.text()
            assert reader.u8() == 0
            branch = reader.bytes().decode().removeprefix("refs/heads/")
            ahead = 0 if commit in self.on.get(branch, set()) else 1
            reply = signer.u8(7) + signer.u64(9) + text(commit) + text("f" * 40) + b"\x00" + signer.u64(ahead) + signer.u64(0) + b"\x02"
            return ran(reply)
        tag, check = request[0], int.from_bytes(request[1:9], "little")
        match tag:
            case 2:
                rows = [check_row(id, **fields) for id, fields in self.queue if id not in self.taken]
                return ran(signer.u8(2) + signer.u64(1) + signer.borsh_vec(rows) + b"\x00")
            case 0:
                fields = self.rows.get(check)
                if fields is None and check in self.taken:
                    fields = {"state": "running", "claimant": 60}
                return ran(signer.u8(0) + (b"\x00" if fields is None else b"\x01" + check_row(check, **fields)))
            case 6:
                needs = [text(job) + signer.borsh_option(met, signer.u64) for job, met in self.needs.get(check, [])]
                return ran(signer.u8(6) + signer.borsh_vec(needs))
            case 7:
                row = self.outputs.get(check)
                if row is None:
                    return ran(signer.u8(7) + b"\x00")
                by, files = row
                listed = [text(path) + signer.blob_id(blob) + signer.u64(size) for path, blob, size in files]
                return ran(signer.u8(7) + b"\x01" + signer.u64(by) + signer.borsh_vec(listed))
        raise AssertionError(f"a query this fake does not answer: {request.hex()}")

    def submit(self, program, payload):
        self.submits.append((program, payload))
        if self.failing and payload[0] != 3:
            return self.failing.pop(0)
        if payload[0] == 3:
            check = int.from_bytes(payload[1:9], "little")
            if check in self.taken:
                return ran(b"preconfirmed rejected wrong_state: check is running, not queued")
            if check in self.unnamed:
                return ran(b"preconfirmed rejected unauthorized: check's job names its runners in its workflow, and this account is not one")
            self.taken.add(check)
            if check in self.lost:
                return ran(code=1, err="ducktape: the connection closed before the receipt")
        return ran(b"preconfirmed applied 0a")

    def put(self, paths, kind):
        files = [(pathlib.Path(path).name, pathlib.Path(path).read_bytes()) for path in paths]
        self.puts.append((kind, files))
        return ran("".join(f"{blob_of(data)}\n" for _, data in files).encode())

    def blob(self, blob):
        return ran(self.blobs[blob])


class Signing(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.temp.name)
        (root / "tool.py").write_text(TOOL)
        tool = [sys.executable, str(root / "tool.py")]
        SAID.clear()
        self.config = {
            "labels": ["sign:dev.ducktape.app"],
            "builders": [MAC, PODS],
            "repos": {"app": {"developer-id": ["main", "dev"], "release-manifest": {"stable": ["main"], "nightly": ["main", "dev"]}}},
            "tools": {"developer-id": tool, "release-manifest": tool, "testflight": tool},
        }
        self.fake = Fake()
        self.signer = signer.Signer(self.fake, self.config, root / "home")
        self.assertTrue(self.signer.identify())
        SAID.clear()
        self.built(9, b"\xcf\xfa\xed\xfeapp")

    def tearDown(self):
        self.temp.cleanup()

    def built(self, check, data, by=MAC, reporter=MAC, commit=COMMIT, run=1, state="passed"):
        blob = blob_of(data)
        self.fake.blobs[blob] = framed(data)
        self.fake.rows[check] = {"commit": commit, "run": run, "state": state, "reporter": reporter, "job": "build"}
        self.fake.outputs[check] = (by, [("dist/ducktape.app", blob, len(data))])
        return blob

    def asks(self, check, **fields):
        self.fake.queue.append((check, fields))
        self.fake.needs.setdefault(check, [("build", 9)])

    def sent(self, tag):
        return [payload for program, payload in self.fake.submits if program == signer.ACTIONS and payload[0] == tag]

    def ended(self, check):
        reports = [payload for payload in self.sent(4) if int.from_bytes(payload[1:9], "little") == check]
        self.assertEqual(len(set(reports)), 1, "one report, however often it was sent")
        reader = signer.Reader(reports[-1])
        reader.take(9)
        return signer.CHECK_STATES[reader.u8()], reader.text()

    def test_a_check_built_by_a_trusted_builder_on_main_is_signed_and_reported_with_its_record(self):
        self.asks(3, runs_on=("linux",))
        self.asks(4)
        self.fake.unnamed.add(4)
        self.asks(5, env=(("MODE", "dist"), ("bad-name", "x")))
        self.signer.pull()
        self.assertEqual([int.from_bytes(claim[1:9], "little") for claim in self.sent(3)], [4, 5], "only the checks its labels cover, past one whose job names other runners")
        self.assertEqual(self.ended(5), ("passed", f"developer-id for app at {COMMIT} (main)"))
        [(kind, files)] = self.fake.puts
        self.assertEqual(kind, "output")
        named = dict(files)
        self.assertEqual(named["ducktape.app.signed"], b"signed:\xcf\xfa\xed\xfeapp")
        env = json.loads(named["env.json"])
        self.assertEqual(env, {"SIGNER_BRANCH": "main", "SIGNER_CHECK": "5", "SIGNER_COMMIT": COMMIT, "SIGNER_ENV_MODE": "dist", "SIGNER_REPO": "app"})
        record = json.loads(named["signed.json"])
        self.assertEqual((record["account"], record["operation"], record["commit"], record["branch"], record["check"]), (60, "developer-id", COMMIT, "main", 5))
        self.assertEqual(record["inputs"][0]["blob"], self.fake.outputs[9][1][0][1])
        self.assertEqual(record["inputs"][0]["sha256"], hashlib.sha256(b"\xcf\xfa\xed\xfeapp").hexdigest())
        self.assertIn({"path": "ducktape.app.signed", "sha256": hashlib.sha256(named["ducktape.app.signed"]).hexdigest()}, record["outputs"])
        [outputs] = self.sent(7)
        listed = [(name, blob_of(data), len(data)) for name, data in files]
        self.assertEqual(outputs, signer.actions_outputs(5, listed))
        self.assertEqual(self.signer.owed(), [])
        self.assertFalse((pathlib.Path(self.temp.name) / "home" / "work" / "5").exists(), "nothing it was handed is kept")

    def test_what_the_network_cannot_vouch_for_errs_the_check_and_runs_no_tool(self):
        cases = [
            ("a commit on no allowed branch", {}, lambda: self.fake.on.update(main=set()), f"commit {COMMIT} is on none of app's main, dev"),
            ("an operation it does not hold", {"steps": ("make sign",)}, lambda: None, "a signing check names one operation, one of developer-id, release-manifest, testflight; this one's steps are ['make sign']"),
            ("no build it needs", {}, lambda: self.fake.needs.update({5: []}), "a signing check needs the check that built what it signs"),
            ("a build a stranger reported", {}, lambda: self.built(9, b"app", reporter=STRANGER), f"`build`'s check 9 was reported by account {STRANGER}, no builder this signer trusts"),
            ("outputs a stranger recorded", {}, lambda: self.built(9, b"app", by=STRANGER), "`build`'s check 9 has no outputs a trusted builder recorded"),
            ("a build of another commit", {}, lambda: self.built(9, b"app", commit="d" * 40), f"`build`'s check 9 is not a passed check of this run at {COMMIT}"),
            ("a build of another run", {}, lambda: self.built(9, b"app", run=2), f"`build`'s check 9 is not a passed check of this run at {COMMIT}"),
            ("a blob with other bytes", {}, lambda: self.fake.blobs.update({self.built(9, b"app"): framed(b"evil")}), "came back with other bytes than its id names"),
            ("a path out of its folder", {}, lambda: self.fake.outputs.update({9: (MAC, [("../../x", self.built(9, b"app"), 3)])}), "`build`'s output '../../x' would land outside its folder"),
            ("a stable manifest from dev", {"steps": ("release-manifest",), "env": (("CHANNEL", "stable"),)}, lambda: self.fake.on.update(main=set(), dev={COMMIT}), f"commit {COMMIT} is on none of app's main"),
            ("a manifest with no channel", {"steps": ("release-manifest",)}, lambda: None, "release-manifest names a channel in CHANNEL, one of nightly, stable, not None"),
            ("a repo it does not sign for", {"steps": ("testflight",)}, lambda: None, "this signer does not testflight for repo app"),
        ]
        for name, fields, arrange, sentence in cases:
            with self.subTest(name):
                self.tearDown()
                self.setUp()
                arrange()
                self.asks(5, **fields)
                self.signer.pull()
                state, summary = self.ended(5)
                self.assertEqual(state, "errored")
                self.assertIn(sentence, summary)
                self.assertEqual((self.fake.puts, self.sent(7)), ([], []), "nothing was signed")

    def test_a_nightly_manifest_from_dev_is_signed(self):
        self.fake.on = {"dev": {COMMIT}}
        self.asks(5, steps=("release-manifest",), env=(("CHANNEL", "nightly"),))
        self.signer.pull()
        self.assertEqual(self.ended(5), ("passed", f"release-manifest for app at {COMMIT} (dev)"))

    def test_a_tool_that_fails_fails_the_check_and_keeps_its_words_on_the_mac(self):
        self.asks(5, env=(("FAIL", "1"),))
        self.signer.pull()
        self.assertEqual(self.ended(5), ("failed", "developer-id exited 3; what it said is in the signer's own log"))
        self.assertEqual(self.fake.puts, [])
        self.assertNotIn(b"hunter2", b"".join(payload for _, payload in self.fake.submits))
        log = pathlib.Path(self.temp.name) / "home" / "logs" / "5.log"
        self.assertIn("hunter2", log.read_text())

    def test_a_signer_that_breaks_or_stops_holding_a_check_errs_it_and_clears_its_work(self):
        home = pathlib.Path(self.temp.name) / "home"
        self.config["tools"]["developer-id"] = [str(home / "no-such-tool")]
        self.asks(5)
        self.asks(6)
        self.signer.pull()
        self.assertEqual([self.ended(check) for check in (5, 6)], [("errored", signer.BROKE)] * 2)
        self.assertEqual(list((home / "work").iterdir()), [])
        self.assertFalse((home / "claimed.json").exists())
        (home / "claimed.json").write_text("7")
        (home / "work" / "7" / "inputs").mkdir(parents=True)
        self.fake.rows[7] = {"state": "running", "claimant": 60}
        self.signer.resume()
        self.assertEqual(self.ended(7), ("errored", signer.STOPPED))
        self.assertEqual((list((home / "work").iterdir()), (home / "claimed.json").exists()), ([], False))
        for state, claimant in (("queued", None), ("running", STRANGER), ("passed", 60)):
            (home / "claimed.json").write_text("8")
            self.fake.rows[8] = {"state": state, "claimant": claimant}
            self.signer.resume()
            self.assertFalse((home / "claimed.json").exists(), f"a {state} check held by {claimant} is not this signer's")
        self.assertEqual([payload for payload in self.sent(4) if payload[1] == 8], [], "a check it never held is not reported")

    def test_a_claim_whose_receipt_was_lost_is_read_back_and_errs_if_it_landed(self):
        home = pathlib.Path(self.temp.name) / "home"
        self.asks(5)
        self.asks(6)
        self.fake.lost = {5}
        self.signer.pull()
        self.assertEqual(json.loads((home / "claimed.json").read_text()), 5, "kept: the claim may have landed")
        self.assertEqual([int.from_bytes(claim[1:9], "little") for claim in self.sent(3)], [5], "and nothing more is claimed")
        self.fake.lost = set()
        self.signer.pull()
        self.assertEqual(self.ended(5), ("errored", signer.STOPPED))
        self.assertEqual(self.ended(6)[0], "passed")
        self.assertFalse((home / "claimed.json").exists())

    def test_a_report_that_does_not_send_is_said_once_and_its_sending_once_it_does(self):
        self.asks(5)
        self.fake.failing = [ran(code=1, err="the node was not reached")] * 4
        for _ in range(4):
            self.signer.pull()
        self.signer.pull()
        said = [(event, fields) for event, fields in SAID if event in ("signer_unsent", "signer_sent")]
        self.assertEqual(said, [("signer_unsent", {"owed": 2, "detail": json.dumps("the node was not reached")}), ("signer_sent", {"attempts": 5})])

    def test_a_report_that_was_not_sent_is_sent_before_any_other_check_is_claimed(self):
        self.asks(5)
        self.asks(6)
        self.fake.failing = [ran(code=1, err="the node was not reached")]
        self.signer.pull()
        self.assertEqual(len(self.signer.owed()), 2, "its outputs and report are owed")
        self.fake.failing = [ran(code=1, err="the node was not reached")]
        self.signer.pull()
        self.assertEqual([int.from_bytes(claim[1:9], "little") for claim in self.sent(3)], [5], "nothing is claimed while a report is owed")
        self.signer.pull()
        self.assertEqual(self.signer.owed(), [])
        self.assertEqual([self.ended(check)[0] for check in (5, 6)], ["passed", "passed"])
        self.assertEqual([payload[0] for _, payload in self.fake.submits if payload[1] == 5], [3, 7, 7, 7, 4], "its outputs, sent until taken, then its report")

    def test_the_follower_wakes_the_signer_once_a_height_and_follows_again_after_its_pause(self):
        class Stopped(Exception):
            pass

        class Followed:
            why = "ducktape: IO error: the node is not reachable"

            def __init__(self, lines):
                self.lines = lines

            def __iter__(self):
                return iter(self.lines)

        wakes, paused = [], []
        follows = [[b"5\tk1\tv\n", b"5\tk2\tv\n", b"6\tk1\tv\n"], [], [], [b"7\tk1\tv\n"]]
        self.signer.wake.set = lambda: wakes.append(1)

        def pause(seconds):
            paused.append(seconds)

        def stop():
            if not follows:
                raise Stopped()
            return Followed(follows.pop(0))

        self.fake.follow = lambda program: stop()
        self.signer.pause = pause
        with self.assertRaises(Stopped):
            self.signer.follow()
        self.assertEqual(len(wakes), 1 + 2 + 1 + 1 + 1 + 1, "each follow's start, and each height it heard")
        self.assertEqual(paused, [signer.FOLLOW_PAUSE] * 2)
        said = [(event, fields.get("heard", fields.get("attempts"))) for event, fields in SAID]
        self.assertEqual(said, [("signer_unfollowed", True), ("signer_unfollowed", False), ("signer_followed", 3), ("signer_unfollowed", True)])

    def test_pulls_that_cannot_read_actions_are_said_once_and_their_recovery_with_its_attempts(self):
        self.asks(5)
        self.fake.down = True
        for _ in range(3):
            self.signer.pull()
        self.fake.down = False
        self.signer.pull()
        said = [(event, fields) for event, fields in SAID if event in ("signer_unpulled", "signer_pulled")]
        why = json.dumps("actions did not answer Queued: ducktape: IO error: the node is not reachable")
        self.assertEqual(said, [("signer_unpulled", {"detail": why}), ("signer_pulled", {"attempts": 4})])
        self.assertEqual(self.ended(5)[0], "passed")

    def test_the_signer_is_the_account_its_key_acts_as_and_a_key_with_none_serves_nothing(self):
        key = bytes([0xFB, 0xFF]) * 16
        encoded = __import__("base64").urlsafe_b64encode(bytes(52) + key + bytes(48)).rstrip(b"=").decode()
        self.assertTrue("-" in encoded and "_" in encoded, "the keystore's own alphabet, unpadded")
        self.assertEqual(signer.public_key(signer.USER_KEY + encoded + "\n"), key)
        with self.assertRaises(signer.Refused):
            signer.public_key("ducktape-user-key-v1:AAAA")
        SAID.clear()
        self.fake.down, paused = True, []
        self.signer.pause = lambda seconds: (paused.append(seconds), setattr(self.fake, "down", False))
        self.assertTrue(self.signer.identify())
        self.assertEqual((self.signer.account, paused), (60, [signer.FOLLOW_PAUSE]))
        self.assertEqual([event for event, _ in SAID], ["signer_unidentified", "signer_identified"])
        self.fake.accounts = {}
        self.asks(5)
        self.signer.serve()
        self.assertEqual(self.fake.submits, [], "a key that is no account registers and claims nothing")
        self.assertEqual(SAID[-1][0], "signer_unkeyed")

    def test_a_home_with_no_config_ends_the_signer_and_one_that_does_not_read_fails_it(self):
        home = pathlib.Path(self.temp.name) / "empty"
        home.mkdir()
        __import__("os").environ["SIGNER_HOME"] = str(home)
        try:
            signer.main()
            self.assertEqual(SAID[-1][0], "signer_unconfigured")
            for text in ("", '{"labels": ["sign:'):
                (home / "config.json").write_text(text)
                with self.assertRaises(SystemExit) as ended:
                    signer.main()
                self.assertEqual(ended.exception.code, 1, "launchd reads a half-saved config again")
        finally:
            del __import__("os").environ["SIGNER_HOME"]

    def test_the_follow_pause_is_the_macs_relay_down_pause(self):
        self.assertEqual(signer.FOLLOW_PAUSE, mac.RELAY_DOWN_PAUSE)


if __name__ == "__main__":
    unittest.main()
