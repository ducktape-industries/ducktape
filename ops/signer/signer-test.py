"""Exercise the signer against a fake network (actions, forge and blobs as the ducktape CLI answers them) and real tool processes; run by `make test`."""
import hashlib
import importlib.util
import json
import pathlib
import shutil
import subprocess
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
CAPACITY = "query or operation exceeds its configured work/byte bound"
TOOL = """
import json, os, pathlib, sys
inputs, out = map(pathlib.Path, sys.argv[1:])
if os.environ.get("SIGNER_ENV_FAIL"):
    print("security unlock-keychain -p hunter2", file=sys.stderr)
    sys.exit(3)
if os.environ.get("SIGNER_ENV_PUBLISH_JSON"):
    (out / "publish.json").write_text(os.environ["SIGNER_ENV_PUBLISH_JSON"])
elif os.environ.get("SIGNER_ENV_PUBLISH"):
    (out / "inputs-seen.json").write_text(pathlib.Path(os.environ["SIGNER_INPUTS"]).read_text())
    source = os.environ.get("SIGNER_ENV_FROM", "inputs/build/dist/ducktape.app")
    wanted = {"files": [{"path": "/shared/releases/app/x/ducktape.app", "from": source}], "tag": {"name": os.environ["SIGNER_ENV_PUBLISH"], "message": '{"record": 1}', "time": 1792000000}}
    (out / "publish.json").write_text(json.dumps(wanted))
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
        self.refs = {}
        self.packs = {}
        self.commits = []
        self.files_answers = []
        self.ref_refused = False
        self.pushed = None
        self.uncountable = set()
        self.misanswered = False
        self.forge_asked = []

    def key(self):
        return KEY

    def query(self, program, request):
        if self.down:
            return ran(code=1, err="ducktape: IO error: the node is not reachable")
        if program == signer.FORGE:
            self.forge_asked.append(request[0])
        if program == signer.IDENTITY:
            reader = signer.Reader(request)
            assert reader.u8() == 0
            return ran(signer.u8(0) + signer.borsh_option(self.accounts.get(reader.bytes()), signer.u64))
        if program == signer.FILES:
            assert request == signer.files_head()
            return ran(signer.u8(0) + b"\x01" + signer.blob_id("sha256:" + "cd" * 32))
        if program == signer.FORGE and request[0] == 26:
            reader = signer.Reader(request)
            reader.u8()
            assert reader.text() == "app"
            assert reader.u8() == 1
            commit = reader.text()
            compared = []
            for kind, name in reader.vec(lambda: (reader.u8(), reader.bytes())):
                assert kind == 0
                branch = name.decode().removeprefix("refs/heads/")
                behind = 0 if commit in self.on.get(branch, set()) else 1
                counted = signer.u8(0) + text("f" * 40) + text(commit) + b"\x00" + signer.u64(0) + signer.u64(behind) + b"\x02"
                tally = signer.u8(1) + text(CAPACITY) if branch in self.uncountable else counted
                compared.append(signer.u8(0) + signer.borsh_bytes(name) + tally)
            if self.misanswered:
                compared.reverse()
            return ran(signer.u8(23) + signer.u64(9) + text(commit) + signer.borsh_vec(compared))
        if program == signer.FORGE and request[0] == 16:
            reader = signer.Reader(request)
            reader.u8()
            assert reader.text() == "app"
            prefix = reader.text()
            items = [text(name) + text(oid) for name, oid in sorted(self.refs.items()) if name.startswith(prefix)]
            return ran(signer.u8(14) + signer.u64(9) + signer.u64(9) + signer.borsh_vec(items) + b"\x00")
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
        if program == signer.FILES:
            self.commits.append(payload)
            return self.files_answers.pop(0) if self.files_answers else ran(b"preconfirmed applied 0a")
        if program == signer.FORGE:
            reader = signer.Reader(payload)
            assert reader.u8() == 4 and reader.text() == "app"
            request = reader.bytes()
            pack = reader.option(reader.blob)
            line = request[4 : int(request[:4], 16)].split(b"\0")[0].decode()
            _, oid, ref = line.split(" ")
            assert request.endswith(b"0000")
            if not self.ref_refused:
                self.refs[ref] = oid
            self.pushed = (oid, ref, self.packs[pack])
            return ran(b"preconfirmed applied 0a")
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
        self.packs.update({blob_of(data): data for _, data in files})
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
            "publish": {"app": {"release-manifest": {"files": "/shared/releases/app/", "tags": "release/"}}},
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
        self.signer.pull()
        self.assertEqual([int.from_bytes(claim[1:9], "little") for claim in self.sent(3)], [4, 5], "a check whose job names other runners is asked once")
        self.assertEqual([event for event, _ in SAID].count("signer_unnamed"), 1)
        self.fake.queue = [(id, fields) for id, fields in self.fake.queue if id != 4]
        self.signer.pull()
        self.assertEqual(self.signer.unnamed, set(), "forgotten once actions no longer queues it")
        self.assertEqual(self.ended(5), ("passed", f"developer-id for app at {COMMIT} (main)"))
        [(kind, files)] = self.fake.puts
        self.assertEqual(kind, "output")
        named = dict(files)
        self.assertEqual(named["ducktape.app.signed"], b"signed:\xcf\xfa\xed\xfeapp")
        env = json.loads(named["env.json"])
        self.assertTrue(env.pop("SIGNER_INPUTS").endswith("/work/5/inputs.json"))
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

    def test_a_check_this_signer_signed_itself_is_an_input_it_trusts(self):
        self.built(9, b"signed bundle", by=60, reporter=60)
        self.asks(5, steps=("release-manifest",), env=(("CHANNEL", "stable"),))
        self.signer.pull()
        self.assertEqual(self.ended(5)[0], "passed", "its own account's outputs, learnt from its key")

    def test_a_tool_asks_for_files_and_a_tag_and_the_signer_makes_them_with_its_own_key(self):
        home = pathlib.Path(self.temp.name) / "home"
        self.asks(5, steps=("release-manifest",), env=(("CHANNEL", "stable"), ("PUBLISH", "release/stable/1792000000")))
        self.signer.pull()
        self.assertEqual(self.ended(5)[0], "passed")
        [commit] = self.fake.commits
        self.assertIn(b"/shared/releases/app/x/ducktape.app", commit)
        chunks = [files for kind, files in self.fake.puts if kind == signer.CHUNK]
        self.assertEqual(chunks, [[("chunk-0", b"\xcf\xfa\xed\xfeapp")]])
        oid, ref, pack = self.fake.pushed
        self.assertEqual((ref, self.fake.refs), ("refs/tags/release/stable/1792000000", {ref: oid}))
        tagger = "signer 60 <60@duckpond>"
        body = f"object {COMMIT}\ntype commit\ntag release/stable/1792000000\ntagger {tagger} 1792000000 +0000\n\n{{\"record\": 1}}\n".encode()
        self.assertEqual(oid, hashlib.sha1(f"tag {len(body)}\0".encode() + body).hexdigest())
        git = shutil.which("git")
        if git:
            repo = home / "git"
            subprocess.run([git, "init", "-q", "--bare", str(repo)], check=True)
            subprocess.run([git, "-C", str(repo), "index-pack", "--stdin"], input=pack, check=True, capture_output=True)
            shown = subprocess.run([git, "-C", str(repo), "cat-file", "tag", oid], check=True, capture_output=True).stdout
            self.assertEqual(shown, body, "git reads the pack as this one tag")
        named = dict(next(files for kind, files in self.fake.puts if kind == signer.OUTPUT))
        self.assertNotIn("publish.json", named, "an instruction, not an output")
        seen = json.loads(named["inputs-seen.json"])
        self.assertEqual([(item["job"], item["check"], item["blob"]) for item in seen], [("build", 9, self.fake.outputs[9][1][0][1])])
        record = json.loads(named["signed.json"])
        published = {"path": "/shared/releases/app/x/ducktape.app", "sha256": hashlib.sha256(b"\xcf\xfa\xed\xfeapp").hexdigest()}
        self.assertEqual(record["published"], {"files": [published], "tag": {"name": "release/stable/1792000000", "oid": oid}})
        self.fake.puts.clear()
        self.fake.pushed = None
        self.asks(6, steps=("release-manifest",), env=(("CHANNEL", "stable"), ("PUBLISH", "release/stable/1792000000")))
        self.signer.pull()
        self.assertEqual((self.ended(6)[0], self.fake.pushed), ("passed", None), "the same record again is already published")
        self.fake.refs[ref] = "e" * 40
        written = len(self.fake.commits)
        self.asks(7, steps=("release-manifest",), env=(("CHANNEL", "stable"), ("PUBLISH", "release/stable/1792000000")))
        self.signer.pull()
        self.assertEqual(self.ended(7), ("errored", f"app already has {ref} at {'e' * 40}, another record than this one ({oid})"))
        self.assertEqual(len(self.fake.commits), written, "a refused record writes no files")

    def test_what_a_tool_may_publish_is_bounded_by_the_signers_config_and_a_refusal_writes_nothing(self):
        place = lambda path: {"files": [{"path": path, "from": "inputs/build/dist/ducktape.app"}]}
        tag = lambda name, message="m", time=1: {"tag": {"name": name, "message": message, "time": time}}
        cases = [
            ("a path with no write list", place("/shared/anyone/notes.md"), "outside /shared/releases/app/, where this signer publishes release-manifest's files"),
            ("another's home", place("/home/eddy/notes.md"), "outside /shared/releases/app/"),
            ("a path climbing out", place("/shared/releases/app/../../x"), "which is not an absolute files path"),
            ("a relative path", place("shared/releases/app/x"), "which is not an absolute files path"),
            ("a tag outside its prefix", tag("v9.9.9"), "outside release/, where this signer tags for release-manifest"),
            ("a tag with ..", tag("release/../x"), "which is no ref name forge takes"),
            ("a hidden component", tag("release/.hidden"), "which is no ref name forge takes"),
            ("a lock name", tag("release/x.lock"), "which is no ref name forge takes"),
            ("a time that is true", tag("release/1", time=True), "its time as a whole unix time"),
            ("a message that is not text", tag("release/1", message=7), "its name and message as text"),
            ("not an object", [1], "is an object of files and tag"),
            ("an operation that publishes nothing", tag("release/1"), "this signer publishes nothing for developer-id in app"),
        ]
        for name, wanted, sentence in cases:
            with self.subTest(name):
                self.tearDown()
                self.setUp()
                steps = ("developer-id",) if name.startswith("an operation") else ("release-manifest",)
                self.asks(5, steps=steps, env=(("CHANNEL", "stable"), ("PUBLISH_JSON", json.dumps(wanted))))
                self.signer.pull()
                state, summary = self.ended(5)
                self.assertEqual(state, "errored")
                self.assertIn(sentence, summary)
                self.assertEqual((self.fake.commits, self.fake.refs, self.fake.pushed), ([], {}, None), "nothing was written")

    def test_a_push_forge_applied_without_moving_the_tag_errs_the_check(self):
        self.fake.ref_refused = True
        self.asks(5, steps=("release-manifest",), env=(("CHANNEL", "stable"), ("PUBLISH", "release/stable/1792000000")))
        self.signer.pull()
        oid = self.fake.pushed[0]
        self.assertEqual(self.ended(5), ("errored", f"forge took the push, but refs/tags/release/stable/1792000000 does not name {oid}"))

    def test_a_files_commit_whose_head_moved_is_made_again_on_the_new_head(self):
        self.fake.files_answers = [ran(b"preconfirmed rejected stale: the head moved")]
        self.asks(5, steps=("release-manifest",), env=(("CHANNEL", "stable"), ("PUBLISH", "release/stable/1792000000")))
        self.signer.pull()
        self.assertEqual((self.ended(5)[0], len(self.fake.commits)), ("passed", 2))

    def test_a_tool_that_asks_to_publish_a_file_from_outside_its_work_errs_the_check(self):
        self.asks(5, steps=("release-manifest",), env=(("CHANNEL", "stable"), ("PUBLISH", "release/stable/1"), ("FROM", "../../pending.json")))
        self.signer.pull()
        self.assertTrue((pathlib.Path(self.temp.name) / "home" / "pending.json").is_file(), "it names a file that is there")
        self.assertEqual(self.ended(5), ("errored", "the tool asked to write '../../pending.json', which is no file of this check's work"))
        self.assertEqual((self.fake.commits, self.fake.refs), ([], {}))

    def test_a_nightly_manifest_from_dev_is_signed(self):
        self.fake.on = {"dev": {COMMIT}}
        self.asks(5, steps=("release-manifest",), env=(("CHANNEL", "nightly"),))
        self.signer.pull()
        self.assertEqual(self.ended(5), ("passed", f"release-manifest for app at {COMMIT} (dev)"))

    def test_a_branch_forge_cannot_count_leaves_another_allowed_branch_to_prove_the_commit_in_one_read(self):
        self.fake.on = {"main": set(), "dev": {COMMIT}}
        self.fake.uncountable = {"main"}
        self.asks(5)
        self.signer.pull()
        self.assertEqual(self.ended(5), ("passed", f"developer-id for app at {COMMIT} (dev)"))
        self.assertEqual(self.fake.forge_asked, [26], "every allowed branch in one Compares")

    def test_a_commit_no_allowed_branch_proves_errs_naming_each_branch_forge_could_not_count(self):
        self.fake.on = {"main": {COMMIT}, "dev": set()}
        self.fake.uncountable = {"main"}
        self.asks(5)
        self.signer.pull()
        self.assertEqual(self.ended(5), ("errored", f"commit {COMMIT} is on none of app's main, dev; forge could not count main: {CAPACITY}"))
        self.assertEqual((self.fake.puts, self.sent(7)), ([], []), "nothing was signed")

    def test_an_answer_for_other_branches_than_asked_proves_nothing(self):
        self.fake.on = {"main": {COMMIT}, "dev": set()}
        self.fake.misanswered = True
        self.asks(5)
        self.signer.pull()
        self.assertEqual(self.ended(5), ("errored", "forge answered Compares for other branches than main, dev"))
        self.assertEqual((self.fake.puts, self.sent(7)), ([], []), "nothing was signed")

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
