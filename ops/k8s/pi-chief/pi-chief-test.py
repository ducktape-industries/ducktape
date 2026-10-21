"""Exercise pi-chief against replies recorded from duckpond and a simulated chain, Deployment and CLI: each trigger, each guard, the hold and its acknowledgement, and the dry runs; run by `make test`."""
import copy
import importlib.util
import io
import json
import pathlib
import ssl
import tempfile
import unittest
import urllib.error
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("pi_chief", HERE / "pi-chief.py")
pc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pc)
pc.say = lambda event, **fields: None

STATUS = "080000006475636b706f6e64c036714aa1010000e8030000000000004000000000000000dc13020000000000dfc0f2967d951a2b07c824d9ab7806acc4819901eddb9cc59c9682cb42bd2596ea3b3309239a3defceeb6060eefeb6a8b5e446f8c8fdcf7a6357c417943a9d984f080000000000002000000075bf265d1e35103677d009f4fbba0401996c437f56e0481b3a9f9c1e53afe37d09000000725bcad16a1005eef8efe8db1d84ec5d58d00a6b4fd38d6446e5a3d845f71fb5090000000600000061636365737306000000616363657373060000006177616974730600000061776169747305000000626c6f627305000000626c6f627306000000656772657373060000006567726573730400000068616c6f0400000068616c6f080000006964656e74697479080000006964656e746974790800000070726f6772616d730800000070726f6772616d73090000007363686564756c6572090000007363686564756c65720a00000076616c696461746f72730600000076616c736574"
SCHEDULE_TX = "011ccd01000000000000000000c4ae9751a101000090ffcf9e49ae8ecb9c15feb111848f895a5d598cdefd01786d8efa99fa5dc40220000000408bd99d74e77c47a24d63fab653c8f72ee058c6f5d34c0c4e70290349652f691acd010000000000954e43d28ee23de3eb17b56603cbfae177e6f28f6e4c8f756585836a3cb5ca220800000070726f6772616d733700000001370000000073cd01000000000000040000006368617400c9478db204d103f7f395f93c793bfda4f2a20f8e09c1b64d214f31b055e15d6f00000000010800000070726f6772616d7300000000000000000000000000"
OF_KEY_18 = "00011200000000000000"
ACCOUNT_18 = "0301120000000000000004000000656464790000df477d4ca101000000030000000020000000408bd99d74e77c47a24d63fab653c8f72ee058c6f5d34c0c4e70290349652f69010c00000067697420287a6b2d64657629df477d4ca10100000020000000af39d48610761ec7f6b7a3511939c86965e9d38d29c26057769b425a4567dbfc010500000050686f6e65fe4dad4aa10100000020000000c2c5e9d227652af4fd5282b56aae736901d3dc13c15b00214533c62d9e676aeb00da13974aa1010000"
ACCOUNT_24 = "030118000000000000000b000000656464792d636c617564650000695d4e4fa101000001120000000000000000000100000000200000003f6ad57603b06635aa81d8d6e5d407af501f3027c06f6d289a367d1526419e3e010b0000006475636b2d636c61756465695d4e4fa101000000"
SCHEDULED = "01e0130200000000000200000084e80100000000000007000000616374696f6e73001a99434fc234b7b1fc675d4961eba9efbefb71214e960711df75da26cda1302b0000000001110000000000000084e8010000000000000c000000616374696f6e732d706f6473008014e87d2005bf6bc3e1098c428a6aa6081ceff0c63e3dc52dbf96598adad2130c0000000100000004000000706f647301110000000000000000"
ENTRY_HARNESS = "02e01302000000000001070000006861726e6573730053b651a06c566fe044730c3f0882ffa2cecb0c3555a2a1aa6968b7f5ca9e552600000000"
REACTORS_759 = "0b01000000f70200000000000004000000f09f918d01000000001100000000000000"
MESSAGE_764 = "0201290000006368616e6e656c2d313864663931323766316133656566342d31373932333130373733393439353834fc02000000000000290000006d6573736167652d313864666464663165346136346635662d31373932333935313538303831393532000f00000000000000c00c020000000000f0f01453a101000001000000000100000022010000eab7b8eb9faceba9b420ec9db4eba087eab28c20ed9598ec9e902e206f70657261746f72ed959ced858c20eba790ed95b4ec849c20666162726963732d31ec979020ecb2b4ec9db8ec9db4eb9e9120ebb384eb8f84eba19c20eb8f8cec9584eab080eb8a94207069206167656e7420ecb9b4ed94bcebb3b8ec9d8420ed9598eb829820eba78ceb93a4ec96b4eb8baceb9dbceab3a020ed95b42028636f70792066726f6d20697473206f776e2073657474696e677320616e6420736b696c6c73292e2070692d6368696566eba19c2e20ec9698eb8a9420ecb2b4ec9db8ec9db4eb9e9120ec8381eab48020ec9786eab3a020eca780eab88820ec98a4ed8dbceba088ec9db4ed84b020ecb298eb9fbc20eb8f8cec9584eab080eb8a94eab1b0ec95bc0000000022010000eab7b8eb9faceba9b420ec9db4eba087eab28c20ed9598ec9e902e206f70657261746f72ed959ced858c20eba790ed95b4ec849c20666162726963732d31ec979020ecb2b4ec9db8ec9db4eb9e9120ebb384eb8f84eba19c20eb8f8cec9584eab080eb8a94207069206167656e7420ecb9b4ed94bcebb3b8ec9d8420ed9598eb829820eba78ceb93a4ec96b4eb8baceb9dbceab3a020ed95b42028636f70792066726f6d20697473206f776e2073657474696e677320616e6420736b696c6c73292e2070692d6368696566eba19c2e20ec9698eb8a9420ecb2b4ec9db8ec9db4eb9e9120ec8381eab48020ec9786eab3a020eca780eab88820ec98a4ed8dbceba088ec9db4ed84b020ecb298eb9fbc20eb8f8cec9584eab080eb8a94eab1b0ec95bc000000000000000001eb020000000000000000000000000000000000000000000000"
CHAT_CODE = "sha256:c9478db204d103f7f395f93c793bfda4f2a20f8e09c1b64d214f31b055e15d6f"
OPEN_DM_15 = "010f000000000000000800000070692d6368696566"
POST_ONE_LINE = "0408000000646d2d31352d34300a00000070692d63686965662d78010000000001000000080000006c696e65206f6e650000000000"

X = "localhost:5000/ducktape:aaaa@sha256:" + "a" * 64
Z = "localhost:5000/ducktape:bbbb@sha256:" + "b" * 64
W = "localhost:5000/ducktape:cccc@sha256:" + "c" * 64
A = "sha256:" + "1" * 64
B = "sha256:" + "2" * 64
PROGRAMS_CODE = "sha256:" + "9" * 64
OTHER_CODE = "sha256:" + "8" * 64
ROLES = {"programs": "programs", "identity": "identity"}
UID = "deployment-uid"
SIGNER = bytes.fromhex("408bd99d74e77c47a24d63fab653c8f72ee058c6f5d34c0c4e70290349652f69")
DEADLINE = 600
EPOCH = 64
SOURCE_COMMIT = "5" * 40
SOURCE_NOTICE = 75
LIB_OID = "6" * 40
SOURCE_FOLDER = ("programs", SOURCE_COMMIT, b"crates/system/programs/src")
ANCHOR = 256


def b(hexed):
    return bytes.fromhex(hexed)


def applied(program):
    return pc.Outcome(program, True, "")


def refused(program, reason):
    return pc.Outcome(program, False, reason)


def tx(target, *outcomes, position=0, signer=b"k", size=10):
    return pc.Tx(position, signer, target, size, None, tuple(outcomes))


def deployment(image, revision=2, complete=True, exceeded=False, deadline=DEADLINE, version="7"):
    conditions = [{"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded"}] if exceeded else [{"type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable"}]
    ready = 1 if complete else 0
    return {
        "metadata": {"name": "node", "uid": UID, "generation": 5, "resourceVersion": version, "annotations": {"deployment.kubernetes.io/revision": str(revision)}, "labels": {"app": "node"}},
        "spec": {
            "replicas": 1,
            "progressDeadlineSeconds": deadline,
            "template": {"metadata": {"labels": {"app": "node"}}, "spec": {"containers": [{"name": "node", "image": image, "resources": {"requests": {"cpu": "8"}}, "ports": [{"containerPort": 8844, "hostPort": 8844}]}]}},
        },
        "status": {"observedGeneration": 5, "replicas": 1, "updatedReplicas": ready, "availableReplicas": ready, "conditions": conditions},
    }


def replicaset(image, revision, uid=UID):
    return {
        "metadata": {"name": f"node-{revision}", "annotations": {"deployment.kubernetes.io/revision": str(revision)}, "ownerReferences": [{"kind": "Deployment", "name": "node", "uid": uid}]},
        "spec": {"template": {"spec": {"containers": [{"name": "node", "image": image}]}}},
    }


class Chain:
    def __init__(self, programs):
        self.blocks = []
        self.programs = dict(programs)
        self.time = 1_000_000
        self.answers = True
        self.payloads = {}
        self.contract = 10

    def make(self, *txs):
        height = len(self.blocks) + 100
        self.time += 1000
        self.blocks.append(pc.Block(height, self.time, tuple(txs)))
        return height

    def tip(self):
        return self.blocks[-1].height


class FakeNode:
    def __init__(self, chain):
        self.chain = chain

    def check(self):
        if not self.chain.answers:
            raise pc.Unreached("connection refused")

    def status(self):
        self.check()
        return pc.Status("duckpond", 1000, EPOCH, self.chain.tip(), ROLES, self.chain.contract)

    def table(self):
        self.check()
        return dict(self.chain.programs)

    def blocks(self, before, limit):
        self.check()
        below = [block for block in reversed(self.chain.blocks) if block.height < before]
        return below[:limit], self.chain.blocks[0].height

    def tx(self, height, position, most):
        self.check()
        block = next(block for block in self.chain.blocks if block.height == height)
        held = block.txs[position]
        return pc.Tx(held.position, held.signer, held.target, held.size, self.chain.payloads.get((height, position)), held.outcomes)

    def call(self, route):
        self.check()
        return b(STATUS)


POOLED = b"pooled " + b"ab" * 32 + b"\n"


class FakeCli:
    def __init__(self, chain):
        self.chain = chain
        self.queries = []
        self.submits = []
        self.pending = []
        self.entries = {}
        self.accounts = {}
        self.reactions = {}
        self.posted = set()
        self.answer = "applied"
        self.queries_answer = True
        self.sources = {}
        self.source_code = {}
        self.tree = {SOURCE_FOLDER: [(b"describe.rs", "8" * 40), (b"examples.rs", "9" * 40), (b"lib.rs", LIB_OID)]}
        self.files = {LIB_OID: f"// programs\npub const NOTICE: u64 = {SOURCE_NOTICE};\n".encode()}
        self.tree_asks = []

    def query(self, program, request):
        if not self.chain.answers or not self.queries_answer:
            raise pc.Unreached("connection refused")
        self.queries.append((program, request))
        match (program, request[0]):
            case ("programs", 1):
                return pc.u8(1) + pc.u64(self.chain.tip()) + pc.borsh_vec([pc.u64(height) + pc.u8(0) + pc.borsh_text(name) + pc.blob_id(code) + pc.borsh_bytes(b"p") + pc.u8(0) for height, name, code in self.pending]) + pc.u8(0)
            case ("programs", 2):
                name = pc.Reader(request[1:]).text()
                code = self.chain.programs.get(name)
                return pc.u8(2) + pc.u64(self.chain.tip()) + pc.borsh_option(code, lambda held: pc.borsh_text(name) + pc.blob_id(held) + pc.borsh_bytes(self.entries.get(name, b"params")))
            case ("programs", 7):
                rows = []
                for name, code in sorted(self.chain.programs.items()):
                    source = self.sources.get(name, ("programs", SOURCE_COMMIT))
                    held = pc.u8(1) if source is None else pc.u8(0) + pc.borsh_text(source[0]) + pc.borsh_text(source[1])
                    rows.append(pc.borsh_text(name) + pc.blob_id(self.source_code.get(name, code)) + held)
                return pc.u8(7) + pc.u64(self.chain.tip()) + pc.borsh_vec(rows)
            case ("forge", 6):
                reader = pc.Reader(request[1:])
                asked = (reader.text(), reader.text(), reader.bytes())
                after = reader.option(reader.bytes)
                self.tree_asks.append(asked)
                entries = self.tree.get(asked, [])
                at = int(after or b"0")
                more = str(at + 1).encode() if at + 1 < len(entries) else None
                listed = pc.borsh_vec([pc.borsh_bytes(name) + pc.borsh_text(oid) + pc.u8(0) for name, oid in entries[at : at + 1]])
                return pc.u8(4) + pc.u64(self.chain.tip()) + pc.borsh_text("7" * 40) + pc.u64(self.chain.tip()) + listed + pc.borsh_option(more, pc.borsh_bytes)
            case ("forge", 7):
                reader = pc.Reader(request[1:])
                reader.text()
                oid = reader.text()
                data = self.files[oid]
                return pc.u8(5) + pc.u64(self.chain.tip()) + pc.borsh_text(oid) + pc.u64(len(data)) + pc.u8(0) + pc.u64(0) + pc.u64(len(data)) + pc.borsh_bytes(data)
            case ("identity", 0):
                key = pc.Reader(request[1:]).bytes()
                return pc.u8(0) + pc.borsh_option(self.accounts.get(key, (None, None))[0], pc.u64)
            case ("identity", 5):
                number = int.from_bytes(request[1:9], "little")
                manager = next((held[1] for held in self.accounts.values() if held[0] == number), None)
                control = pc.u8(0) + pc.borsh_vec([]) if manager is None else pc.u8(1) + pc.u64(manager)
                return pc.u8(3) + pc.u8(1) + pc.u64(number) + pc.borsh_text("n") + pc.u8(0) + pc.u8(0) + pc.u64(0) + control
            case ("chat", 2):
                message_id = pc.Reader(request[1:]).text()
                seq = 40 + sorted(self.posted).index(message_id) if message_id in self.posted else None
                return pc.u8(2) + pc.borsh_option(seq, lambda held: pc.borsh_text("dm") + pc.u64(held))
            case ("chat", 11):
                reader = pc.Reader(request[1:])
                channel = reader.text()
                seqs = reader.vec(reader.u64)
                rows = [pc.u64(seq) + pc.borsh_text("👍") + pc.borsh_vec([pc.u8(0) + pc.u64(by) for by in self.reactions.get((channel, seq), [])]) for seq in seqs]
                return pc.u8(11) + pc.borsh_vec(rows)

    def submit(self, program, payload, node=None):
        if node is not None:
            return self.sign_through(node, program, payload)
        if not self.chain.answers:
            return pc.Ran(1, b"", "error sending request: connection refused")
        self.submits.append((program, payload))
        if program == "chat" and payload[0] == 4:
            reader = pc.Reader(payload[1:])
            reader.text()
            self.posted.add(reader.text())
        block = self.chain.tip() + 1
        match self.answer:
            case "applied":
                return pc.Ran(0, POOLED + f"landed {block} applied \n".encode(), "")
            case "unlanded":
                return pc.Ran(0, POOLED + b"unlanded\n", "")
            case "unknown":
                return pc.Ran(1, POOLED, "ducktape: error sending request: connection reset")
            case reason:
                return pc.Ran(0, POOLED + f"landed {block} rejected {reason}: no\n".encode(), "")

    def sign_through(self, node, program, payload):
        with urllib.request.urlopen(node + "/v1/status") as answer:
            status = answer.read()
        frame = b"signed:" + pc.borsh_text(program) + pc.borsh_bytes(payload) + status[:8]
        request = urllib.request.Request(node + "/v1/submit", data=pc.borsh_bytes(frame) + pc.u8(0), method="POST")
        with urllib.request.urlopen(request) as answer:
            said = answer.read()
        return pc.Ran(0, b"pooled 00\n", "") if said == pc.u8(0) else pc.Ran(1, b"", "bad answer")


class FakeKube:
    def __init__(self, live, replicasets):
        self.container = "node"
        self.namespace = "duckpond"
        self.name = "node"
        self.live = live
        self.sets = replicasets
        self.patches = []
        self.extra = None
        self.answers = True

    def deployment(self):
        if not self.answers:
            raise pc.Failed("the server is currently unable to handle the request")
        return copy.deepcopy(self.live)

    def replicasets(self):
        return copy.deepcopy(self.sets)

    def patch(self, body, dry_run):
        self.patches.append((body, dry_run))
        assert body["metadata"]["resourceVersion"] == self.live["metadata"]["resourceVersion"]
        patched = copy.deepcopy(self.live)
        for wanted in body["spec"]["template"]["spec"]["containers"]:
            held = next(container for container in patched["spec"]["template"]["spec"]["containers"] if container["name"] == wanted["name"])
            held["image"] = wanted["image"]
        if self.extra is not None:
            patched["spec"]["template"]["spec"]["containers"][0]["resources"] = self.extra
        if not dry_run:
            self.live = patched
        return patched


class MemoryStore:
    def __init__(self):
        self.state = pc.fresh_state()

    def load(self):
        return copy.deepcopy(self.state)

    def save(self, state):
        self.state = copy.deepcopy(state)


class Clock:
    def __init__(self, chain):
        self.chain = chain

    def __call__(self):
        return self.chain.time / 1000


def config(mode="act"):
    return pc.Config(mode=mode, account=40, report_to=(15, 17), hands_off=18, anchor_window=ANCHOR)


class World:
    def __init__(self, mode="act", image=X):
        self.chain = Chain({"programs": PROGRAMS_CODE, "identity": OTHER_CODE, "harness": A})
        self.node = FakeNode(self.chain)
        self.cli = FakeCli(self.chain)
        self.kube = FakeKube(deployment(image, revision=1), [replicaset(image, 1)])
        self.store = MemoryStore()
        self.clock = Clock(self.chain)
        self.chief = pc.Chief(config(mode), self.node, self.cli, self.kube, self.store, clock=self.clock, pause=lambda seconds: None)
        self.chain.make()

    def tick(self, *txs, blocks=1):
        for _ in range(blocks):
            self.chain.make(*txs)
        self.chief.tick()

    def roll(self, image, complete=True, exceeded=False):
        revision = int(self.kube.live["metadata"]["annotations"]["deployment.kubernetes.io/revision"]) + 1
        self.kube.live = deployment(image, revision=revision, complete=complete, exceeded=exceeded, version=str(revision * 10))
        self.kube.sets.append(replicaset(image, revision))

    def stall(self, seconds):
        self.chain.time += seconds * 1000
        self.chief.tick()

    def reports(self):
        return [item for item in self.chief.state["reported"]]

    def lines(self, key):
        found = [item["lines"] for item in self.chief.state["outbox"] if item["key"] == key]
        return found[0] if found else self.posted_lines(key)

    def posted_lines(self, key):
        for program, payload in self.cli.submits:
            if program == "chat" and payload[0] == 4 and f"pi-chief-{key}-15".encode() in payload:
                return [payload.decode(errors="replace")]
        return []

    def switch_harness(self, code, scheduled_by, signer=SIGNER):
        self.switch("harness", code, scheduled_by, signer)

    def switch(self, program, code, scheduled_by, signer=SIGNER):
        self.chain.payloads[(self.chain.tip() + 1, 0)] = pc.u8(0) + pc.u64(self.chain.tip() + 3) + pc.u8(0) + pc.borsh_text(program) + pc.blob_id(code) + pc.borsh_bytes(b"params")
        self.cli.accounts[signer] = scheduled_by
        self.tick(tx("programs", applied("programs"), signer=signer))
        self.tick()
        self.chain.programs[program] = code
        self.tick()


class Recorded(unittest.TestCase):
    def test_status_names_the_chain_its_block_time_epoch_and_roles(self):
        status = pc.status_of(b(STATUS))
        self.assertEqual((status.network, status.block_time_ms, status.epoch_length, status.contract), ("duckpond", 1000, 64, 9))
        self.assertEqual(status.roles["validators"], "valset")
        self.assertNotIn("harness", status.roles.values())

    def test_a_schedule_tx_names_the_set_its_signer_and_through_identity_its_account(self):
        located = pc.located_of(b(SCHEDULE_TX))
        scheduled = pc.schedule_of(located.payload)
        self.assertEqual((scheduled.change.verb, scheduled.change.program, scheduled.change.code), ("set", "chat", CHAT_CODE))
        self.assertEqual(located.signer, SIGNER)
        self.assertEqual(pc.number_of(b(OF_KEY_18)), 18)
        self.assertIsNone(pc.manager_of(b(ACCOUNT_18)))
        self.assertEqual(pc.manager_of(b(ACCOUNT_24)), 18)

    def test_programs_replies_decode(self):
        pending = pc.scheduled_of(b(SCHEDULED))
        self.assertEqual({item.change.program for item in pending}, {"actions", "actions-pods"})
        self.assertEqual({item.by for item in pending}, {17})
        program, code, params = pc.entry_of(b(ENTRY_HARNESS))
        self.assertEqual(program, "harness")
        self.assertTrue(code.startswith("sha256:53b651a0"))

    def test_chat_replies_decode(self):
        self.assertEqual(pc.reactors_of(b(REACTORS_759)), {17})
        self.assertEqual(pc.seq_of(b(MESSAGE_764)), 764)

    def test_chat_ops_match_the_chat_crate(self):
        self.assertEqual(pc.dm_open(15, "pi-chief").hex(), OPEN_DM_15)
        self.assertEqual(pc.post_message("dm-15-40", "pi-chief-x", ["line one"]).hex(), POST_ONE_LINE)
        self.assertEqual(pc.dm_channel(40, 15), "dm-15-40")

    def test_a_schedule_set_reads_back_as_the_same_set(self):
        op = pc.schedule_set(500, "harness", A, b"params")
        scheduled = pc.schedule_of(op)
        self.assertEqual((scheduled.height, scheduled.change), (500, pc.Change("set", "harness", A)))
        self.assertTrue(op.endswith(pc.borsh_bytes(b"params")))

    def test_forge_and_programs_queries_carry_their_tags(self):
        self.assertEqual(pc.sources_query(), b"\x07")
        self.assertEqual(pc.tree_query("programs", "ab", b"src", None)[:1], b"\x06")
        self.assertEqual(pc.blob_query("programs", "ab")[:1], b"\x07")


class NodeTriggers(unittest.TestCase):
    def test_first_start_takes_what_runs_as_proven_and_names_no_change(self):
        world = World()
        world.tick()
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [f"stall-{world.chain.tip()}"])
        self.assertEqual(world.kube.patches, [])

    def test_an_image_change_then_a_stall_rolls_only_the_image_back(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.tick()
        world.stall(DEADLINE)
        key = f"node-{pc.short(Z)}"
        self.assertEqual(world.reports(), [key])
        self.assertEqual([dry for _, dry in world.kube.patches], [True, False])
        self.assertEqual(pc.container_image(world.kube.live["spec"]["template"], "node"), X)
        self.assertEqual(world.kube.live["spec"]["template"]["spec"]["containers"][0]["resources"], {"requests": {"cpu": "8"}})
        self.assertEqual(world.chief.state["held"]["key"], key)

    def test_a_rollback_to_an_image_that_ran_another_contract_refuses(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.chain.contract = 11
        world.tick()
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [f"node-{pc.short(Z)}"])
        self.assertEqual(world.kube.patches, [])
        self.assertIn("contract_differs", " ".join(world.lines(f"node-{pc.short(Z)}")))
        self.assertIsNone(world.chief.state["held"])

    def test_coming_back_after_a_contract_roll_with_its_old_record_refuses(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.chain.contract = 11
        world.chain.make()
        world.chief = pc.Chief(config(), world.node, world.cli, world.kube, world.store, clock=world.clock, pause=lambda seconds: None)
        world.tick()
        world.stall(DEADLINE)
        self.assertEqual(world.kube.patches, [])
        self.assertIn("contract_differs", " ".join(world.lines(f"node-{pc.short(Z)}")))

    def test_dry_run_node_judges_the_recorded_contracts_and_never_asks_the_node(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.chain.contract = 11
        world.tick()
        world.chain.answers = False
        rollbacks = pc.Rollbacks(config(), world.node, world.cli, world.kube)
        verdict = pc.node_dry_run_recorded(rollbacks, world.chief.state["node"])
        self.assertEqual(verdict.reason, "contract_differs")
        self.assertEqual(world.kube.patches, [])

    def test_dry_run_node_allows_what_act_mode_allows_when_the_running_contract_was_never_seen(self):
        world = World()
        world.tick()
        world.roll(Z, complete=False)
        world.chain.contract = 11
        world.tick()
        world.chain.answers = False
        self.assertIsNone(world.chief.state["node"]["contract"])
        rollbacks = pc.Rollbacks(config(), world.node, world.cli, world.kube)
        verdict = pc.node_dry_run_recorded(rollbacks, world.chief.state["node"])
        self.assertIsInstance(verdict, pc.Would)
        self.assertEqual(verdict.evidence["to"], X)

    def test_a_record_kept_before_contracts_learns_the_running_one(self):
        world = World()
        world.tick()
        del world.chief.state["node"]["contract"]
        world.tick()
        self.assertEqual(world.chief.state["node"]["contract"], 10)

    def test_a_stall_shorter_than_the_deadline_is_nothing(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.tick()
        world.stall(DEADLINE - 1)
        self.assertEqual(world.reports(), [])

    def test_the_deadline_is_read_from_the_deployment_each_tick(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.kube.live["spec"]["progressDeadlineSeconds"] = 900
        world.tick()
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [])
        world.stall(300)
        self.assertEqual(world.reports(), [f"node-{pc.short(Z)}"])

    def test_progress_deadline_exceeded_rolls_back_while_the_chain_still_moves(self):
        world = World()
        world.tick()
        world.roll(Z, complete=False, exceeded=True)
        world.tick()
        self.assertEqual(world.reports(), [f"node-{pc.short(Z)}"])
        self.assertEqual(pc.container_image(world.kube.live["spec"]["template"], "node"), X)

    def test_an_image_proven_for_an_epoch_is_not_rolled_back(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.tick()
        world.tick(blocks=EPOCH)
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [f"stall-{world.chain.tip()}"])
        self.assertEqual(world.kube.patches, [])

    def test_an_image_change_made_during_a_stall_is_a_fix_attempt_not_watched(self):
        world = World()
        world.tick()
        world.stall(DEADLINE)
        world.roll(Z)
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [f"stall-{world.chain.tip()}"])
        self.assertEqual(world.kube.patches, [])

    def test_its_own_rollback_is_never_watched_as_a_change(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.tick()
        world.stall(DEADLINE)
        world.chief.state["held"] = None
        world.kube.sets.append(replicaset(X, 3))
        world.stall(DEADLINE)
        self.assertEqual(world.chief.state["node"]["own"], True)
        self.assertEqual(len([dry for _, dry in world.kube.patches if not dry]), 1)

    def test_replicasets_that_disagree_with_the_proven_image_refuse(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.roll(W)
        world.kube.sets = [replicaset(X, 1), replicaset(Z, 2), replicaset(W, 3)]
        world.tick()
        world.stall(DEADLINE)
        self.assertEqual(world.kube.patches, [])
        self.assertIn("replicasets_disagree", " ".join(world.lines(f"node-{pc.short(W)}")))

    def test_a_dry_run_that_changes_more_than_the_image_refuses(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.tick()
        world.kube.extra = {"requests": {"cpu": "1"}}
        world.stall(DEADLINE)
        self.assertEqual([dry for _, dry in world.kube.patches], [True])
        self.assertIn("patch_changes_more", " ".join(world.lines(f"node-{pc.short(Z)}")))

    def test_report_mode_runs_the_dry_run_and_changes_nothing(self):
        world = World(mode="report")
        world.tick()
        world.roll(Z)
        world.tick()
        world.stall(DEADLINE)
        self.assertEqual([dry for _, dry in world.kube.patches], [True])
        self.assertEqual(pc.container_image(world.kube.live["spec"]["template"], "node"), Z)
        self.assertIsNone(world.chief.state["held"])
        self.assertIn("would", " ".join(world.lines(f"node-{pc.short(Z)}")))


class ProgramTriggers(unittest.TestCase):
    def failing_harness(self, mode="act", scheduled_by=(25, 15), before=None):
        world = World(mode=mode)
        if before is not None:
            before(world)
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, scheduled_by)
        world.tick(tx("harness", refused("harness", "trap")))
        return world

    def scheduled_set(self, world):
        sets = [pc.schedule_of(payload) for program, payload in world.cli.submits if program == "programs"]
        return sets[0] if sets else None

    def test_a_program_that_traps_after_its_change_is_scheduled_back_with_its_params(self):
        world = self.failing_harness()
        scheduled = self.scheduled_set(world)
        self.assertEqual(scheduled.change, pc.Change("set", "harness", A))
        self.assertEqual(scheduled.height, world.chain.tip() + ANCHOR + SOURCE_NOTICE)
        self.assertEqual(set(world.cli.tree_asks), {SOURCE_FOLDER})
        program, payload = next(held for held in world.cli.submits if held[0] == "programs")
        self.assertTrue(payload.endswith(pc.borsh_bytes(b"params")))
        self.assertEqual(world.chief.state["held"]["key"], f"program-harness-{pc.short(B)}")
        self.assertEqual(world.chief.state["own_programs"], {"harness": A})

    def test_protocol_counts_and_a_program_refusal_does_not(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.tick(tx("harness", refused("harness", "unauthorized")))
        self.assertIsNone(self.scheduled_set(world))
        world.tick(tx("harness", refused("harness", "protocol")))
        self.assertIsNotNone(self.scheduled_set(world))

    def test_a_nested_receipt_counts_for_its_own_program(self):
        world = World()
        world.tick()
        world.tick(tx("egress", applied("egress"), applied("harness")))
        world.switch_harness(B, (25, 15))
        world.tick(tx("egress", applied("egress"), refused("harness", "trap")))
        self.assertIsNotNone(self.scheduled_set(world))

    def test_one_applied_receipt_under_the_new_code_proves_it(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.tick(tx("harness", applied("harness")))
        world.tick(tx("harness", refused("harness", "trap")))
        self.assertIsNone(self.scheduled_set(world))
        self.assertEqual(world.reports(), [])

    def test_old_code_never_seen_applying_is_reported_not_rolled_back(self):
        world = World()
        world.tick()
        world.switch_harness(B, (25, 15))
        world.tick(tx("harness", refused("harness", "trap")))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("never_applied", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_change_eddy_scheduled_is_reported_only(self):
        world = self.failing_harness(scheduled_by=(18, None))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("hands_off", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_change_an_agent_eddy_manages_scheduled_is_reported_only(self):
        world = self.failing_harness(scheduled_by=(24, 18))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("hands_off", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_change_with_no_schedule_in_the_blocks_is_reported_only(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.chain.programs["harness"] = B
        world.tick()
        world.tick(tx("harness", refused("harness", "trap")))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("scheduler_unknown", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_role_program_is_reported_only(self):
        world = World()
        world.tick()
        world.tick(tx("identity", applied("identity")))
        world.chain.programs["identity"] = B
        world.tick()
        world.tick(tx("identity", refused("identity", "trap")))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("role", " ".join(world.lines(f"program-identity-{pc.short(B)}")))

    def test_programs_naming_another_code_for_itself_reports(self):
        world = self.failing_harness(before=lambda world: world.cli.source_code.update({"programs": OTHER_CODE}))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("notice_source", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_an_unstamped_programs_code_reports(self):
        world = self.failing_harness(before=lambda world: world.cli.sources.update({"programs": None}))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("notice_source", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_programs_source_with_no_notice_reports(self):
        world = self.failing_harness(before=lambda world: world.cli.files.update({LIB_OID: b"pub const OTHER: u64 = 1;\n"}))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("notice_source", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_programs_source_with_no_lib_reports(self):
        world = self.failing_harness(before=lambda world: world.cli.tree.update({SOURCE_FOLDER: [(b"describe.rs", "8" * 40)]}))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("notice_source", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_change_already_waiting_for_the_program_reports(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.cli.pending = [(world.chain.tip() + 50, "harness", A)]
        world.tick(tx("harness", refused("harness", "trap")))
        self.assertIsNone(self.scheduled_set(world))
        self.assertIn("pending", " ".join(world.lines(f"program-harness-{pc.short(B)}")))

    def test_a_change_that_already_landed_does_not_count_as_waiting(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.cli.pending = [(world.chain.tip() - 5, "harness", B)]
        world.tick(tx("harness", refused("harness", "trap")))
        self.assertIsNotNone(self.scheduled_set(world))

    def test_a_program_that_moved_on_reports(self):
        world = World()
        world.tick()
        world.chain.programs["harness"] = OTHER_CODE
        verdict = world.chief.rollbacks.program_op("harness", B, A)
        self.assertEqual(verdict.reason, "code_moved")

    def test_with_the_chain_not_advancing_it_waits_and_reports_nothing(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.cli.queries_answer = False
        world.tick(tx("harness", refused("harness", "trap")))
        world.cli.queries_answer = True
        world.chief.tick()
        self.assertIsNone(self.scheduled_set(world))
        self.assertEqual(world.reports(), [])
        world.tick()
        self.assertIsNotNone(self.scheduled_set(world))

    def test_an_unknown_outcome_holds_and_is_never_resent(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.cli.answer = "unknown"
        world.tick(tx("harness", refused("harness", "trap")))
        world.cli.answer = "applied"
        world.tick(tx("harness", refused("harness", "trap")))
        self.assertEqual(len([held for held in world.cli.submits if held[0] == "programs"]), 1)
        self.assertEqual(world.chief.state["held"]["key"], f"program-harness-{pc.short(B)}")

    def test_an_unlanded_rollback_is_reported_once_and_not_held(self):
        world = World()
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.cli.answer = "unlanded"
        world.tick(tx("harness", refused("harness", "trap")))
        world.tick(tx("harness", refused("harness", "trap")))
        text = " ".join(world.lines(f"program-harness-{pc.short(B)}"))
        self.assertIn("the Schedule never landed, so nothing changed: no block carried the frame before its anchor aged out (unlanded)", text)
        self.assertIsNone(world.chief.state["held"])
        self.assertEqual(len([held for held in world.cli.submits if held[0] == "programs"]), 1)

    def test_its_own_rollback_landing_is_not_a_change(self):
        world = self.failing_harness()
        world.chief.state["held"] = None
        world.chain.programs["harness"] = A
        world.tick()
        world.tick(tx("harness", refused("harness", "trap")))
        self.assertTrue(world.chief.state["programs"]["harness"]["own"])
        self.assertEqual(len([held for held in world.cli.submits if held[0] == "programs"]), 1)

    def test_report_mode_signs_through_the_capture_and_sends_nothing(self):
        world = self.failing_harness(mode="report")
        self.assertIsNone(self.scheduled_set(world))
        text = " ".join(world.lines(f"program-harness-{pc.short(B)}"))
        self.assertIn('"sent": false', text)
        self.assertIn('"carries_op": true', text)
        self.assertIsNone(world.chief.state["held"])


class RevertReports(unittest.TestCase):
    def failing(self, program, mode):
        world = World(mode=mode)
        world.chain.programs.setdefault(program, A)
        world.tick()
        world.tick(tx(program, applied(program)))
        world.switch(program, B, (25, 15))
        tip = world.chain.tip()
        world.tick(tx(program, refused(program, "trap")))
        return world, tip + 1, " ".join(world.lines(f"program-{program}-{pc.short(B)}"))

    def test_an_acted_revert_names_the_block_it_lands_at_and_how_to_cancel_it(self):
        world, tip, text = self.failing("counter", "act")
        height = tip + ANCHOR + SOURCE_NOTICE
        self.assertEqual(self.scheduled(world).height, height)
        self.assertIn(f"It lands at block {height} (about 6 min); Cancel before {height} if this revert is wrong.", text)

    def test_an_acted_revert_names_the_block_that_carried_its_schedule(self):
        world, tip, text = self.failing("counter", "act")
        self.assertIn(f'"scheduled_in": {tip + 1}', text)

    def test_a_dry_run_revert_names_the_block_it_would_land_at(self):
        world, tip, text = self.failing("counter", "report")
        height = tip + ANCHOR + SOURCE_NOTICE
        self.assertIn(f"It would land at block {height} (about 6 min); Cancel before {height} if this revert is wrong.", text)

    def test_the_minutes_come_from_the_block_time(self):
        line = pc.landing_line({"height": 1316, "tip": 1000, "block_time_ms": 2000}, "lands")
        self.assertEqual(line, "It lands at block 1316 (about 11 min); Cancel before 1316 if this revert is wrong.")

    def test_a_harness_revert_asks_chief_to_sweep_threads(self):
        _, _, text = self.failing("harness", "act")
        self.assertIn("harness reverted: chief, sweep threads", text)

    def test_a_harness_dry_run_says_what_would_follow_and_asks_for_no_sweep(self):
        _, _, text = self.failing("harness", "report")
        self.assertIn("harness would be reverted, so chief would sweep threads.", text)
        self.assertNotIn("harness reverted: chief, sweep threads", text)

    def test_a_revert_of_another_program_asks_for_no_sweep(self):
        _, _, text = self.failing("counter", "act")
        self.assertNotIn("sweep threads", text)

    def test_a_refused_revert_names_no_block(self):
        world = World(mode="act")
        world.tick()
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (24, 18))
        world.tick(tx("harness", refused("harness", "trap")))
        text = " ".join(world.lines(f"program-harness-{pc.short(B)}"))
        self.assertIn("hands_off", text)
        self.assertNotIn("Cancel before", text)
        self.assertNotIn("sweep threads", text)

    def scheduled(self, world):
        return next(pc.schedule_of(payload) for program, payload in world.cli.submits if program == "programs")


class Halts(unittest.TestCase):
    def test_a_stall_at_a_switch_height_is_reported_and_neither_rollback_runs(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.tick(tx("harness", applied("harness")))
        world.switch_harness(B, (25, 15))
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [f"halt-{world.chain.tip()}"])
        self.assertEqual(world.kube.patches, [])
        self.assertIsNone(TestSupport.programs_set(world))

    def test_a_stall_with_a_change_due_at_the_next_block_is_reported(self):
        world = World()
        world.tick()
        world.cli.pending = [(world.chain.tip() + 1, "harness", B)]
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [f"halt-{world.chain.tip()}"])
        self.assertIn("Neither rollback helps", " ".join(world.lines(f"halt-{world.chain.tip()}")))


class TestSupport:
    @staticmethod
    def programs_set(world):
        sets = [payload for program, payload in world.cli.submits if program == "programs"]
        return sets[0] if sets else None


class HoldAndReports(unittest.TestCase):
    def acted(self):
        world = World()
        world.tick()
        world.roll(Z)
        world.tick()
        world.stall(DEADLINE)
        return world

    def test_reports_go_to_both_dms_once_and_open_them_first(self):
        world = self.acted()
        chat = [payload for program, payload in world.cli.submits if program == "chat"]
        self.assertEqual([payload[0] for payload in chat], [1, 4, 1, 4])
        self.assertEqual(world.chief.state["outbox"], [])
        world.stall(1)
        world.stall(1)
        key = f"pi-chief-node-{pc.short(Z)}-".encode()
        posts = [payload for program, payload in world.cli.submits if program == "chat" and key in payload]
        self.assertEqual(len(posts), 2)

    def test_while_held_a_second_incident_is_reported_and_not_acted_on(self):
        world = self.acted()
        world.chain.make()
        world.roll(W)
        world.tick()
        world.stall(DEADLINE)
        self.assertEqual(len([dry for _, dry in world.kube.patches if not dry]), 1)
        self.assertIn("held", " ".join(world.posted_lines(f"node-{pc.short(W)}")))

    def test_a_reaction_by_15_or_17_acknowledges_and_nobody_else_does(self):
        world = self.acted()
        posts = world.chief.state["held"]["posts"]
        self.assertEqual({post["channel"] for post in posts}, {"dm-15-40", "dm-17-40"})
        world.chief.check_ack()
        seq = world.chief.state["held"]["posts"][0]["seq"]
        world.cli.reactions[("dm-15-40", seq)] = [22]
        world.chief.check_ack()
        self.assertIsNotNone(world.chief.state["held"])
        world.cli.reactions[("dm-15-40", seq)] = [22, 15]
        world.chief.check_ack()
        self.assertIsNone(world.chief.state["held"])

    def test_with_the_chain_down_a_stall_is_judged_from_the_last_block_it_saw(self):
        world = World()
        world.tick()
        world.chain.answers = False
        world.stall(DEADLINE)
        self.assertEqual(world.reports(), [f"stall-{world.chain.tip()}"])
        self.assertEqual(len(world.chief.state["outbox"]), 1)
        world.chain.answers = True
        world.stall(0)
        self.assertEqual(world.chief.state["outbox"], [])

    def test_a_report_made_while_the_chain_is_down_posts_when_it_answers(self):
        world = World()
        world.tick()
        world.roll(Z, complete=False, exceeded=True)
        world.chain.answers = False
        world.stall(1)
        self.assertEqual(len(world.chief.state["outbox"]), 1)
        world.chain.answers = True
        world.tick()
        self.assertEqual(world.chief.state["outbox"], [])

    def test_already_exists_counts_as_posted(self):
        world = self.acted()
        world.chief.state["outbox"].append({"key": "again", "lines": ["x"], "to": [15], "posted": []})
        world.cli.answer = "already_exists"
        world.chief.flush()
        self.assertEqual([item["key"] for item in world.chief.state["outbox"]], [])

    def test_state_survives_a_restart(self):
        world = self.acted()
        again = pc.Chief(config(), world.node, world.cli, world.kube, world.store, clock=world.clock, pause=lambda seconds: None)
        self.assertEqual(again.state["held"]["key"], f"node-{pc.short(Z)}")


class Shapes(unittest.TestCase):
    def test_the_previous_image_is_the_newest_older_replicaset_with_another_image(self):
        live = deployment(Z, revision=4)
        sets = [replicaset(X, 1), replicaset(W, 2), replicaset(Z, 3), replicaset(Z, 4), replicaset(X, 9, uid="other")]
        self.assertEqual(pc.image_before(live, sets, "node"), W)

    def test_changed_paths_names_only_what_moved(self):
        live = deployment(Z)
        moved = copy.deepcopy(live)
        moved["spec"]["template"]["spec"]["containers"][0]["image"] = X
        self.assertEqual(pc.changed_paths(live, moved), ["spec.template.spec.containers.0.image"])
        self.assertEqual(pc.image_path(live, "node"), "spec.template.spec.containers.0.image")

    def test_the_patch_names_only_the_container_image_and_the_version(self):
        body = pc.image_patch("node", X, "77")
        self.assertEqual(body, {"metadata": {"resourceVersion": "77"}, "spec": {"template": {"spec": {"containers": [{"name": "node", "image": X}]}}}})

    def test_a_rollout_reads_complete_and_the_deadline(self):
        rollout = pc.rollout_of(deployment(Z, complete=False, exceeded=True, deadline=321), "node")
        self.assertEqual((rollout.image, rollout.complete, rollout.deadline_exceeded, rollout.deadline_s), (Z, False, True, 321))


class Receipts(unittest.TestCase):
    def test_a_receipt_is_the_block_that_carried_the_frame(self):
        self.assertEqual(pc.receipt(pc.Ran(0, POOLED + b"landed 41 applied 00ff\nevent 0a\n", "")), ("applied", "00ff", "", 41))
        self.assertEqual(pc.receipt(pc.Ran(0, POOLED + b"landed 42 rejected stale: moved\n", "")), ("rejected", "stale", "moved", 42))
        self.assertEqual(pc.receipt(pc.Ran(0, POOLED + b"unlanded\n", "")), ("unlanded", "", "no block carried the frame before its anchor aged out", None))
        self.assertEqual(pc.receipt(pc.Ran(1, POOLED, "ducktape: the node closed its change stream")), ("unknown", "", "ducktape: the node closed its change stream", None))
        self.assertEqual(pc.receipt(pc.Ran(0, b"preconfirmed applied 00ff\n", "")), ("unknown", "", "preconfirmed applied 00ff", None))

    def test_a_submit_waits_for_its_block_and_a_capture_signs_only(self):
        asked = []
        cli = pc.Cli(["ducktape"], "http://node", pathlib.Path("/w"), runner=lambda argv, stdin=None, env=None: asked.append(argv) or pc.Ran(0, b"", ""))
        cli.submit("chat", b"op")
        cli.submit("programs", b"op", node="http://capture")
        (chat, capture) = asked
        self.assertEqual((chat[chat.index("--node") + 1], chat[-4], chat[-3], chat[-1]), ("http://node", "submit", "chat", "--land"))
        self.assertEqual((capture[capture.index("--node") + 1], capture[-3], capture[-2]), ("http://capture", "submit", "programs"))


class Opened:
    def __init__(self, body):
        self.body = body

    def __enter__(self):
        return io.BytesIO(self.body)

    def __exit__(self, *exc):
        return False


class KubeRequests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.mount = pathlib.Path(self.temp.name)
        (self.mount / "token").write_text("token-1\n")
        (self.mount / "ca.crt").write_bytes(pathlib.Path(ssl.get_default_verify_paths().openssl_cafile).read_bytes())
        self.requests = []
        self.answer = b'{"kind": "Deployment"}'

    def tearDown(self):
        self.temp.cleanup()

    def opener(self, request, context):
        self.requests.append(request)
        if isinstance(self.answer, Exception):
            raise self.answer
        return Opened(self.answer)

    def kube(self, host="10.43.0.1"):
        environ = {"KUBERNETES_SERVICE_HOST": host, "KUBERNETES_SERVICE_PORT": "443"}
        return pc.Kube("duckpond", "node", "node", mount=self.mount, environ=environ, opener=self.opener)

    def test_the_deployment_is_read_from_the_api_with_the_service_account_token(self):
        self.assertEqual(self.kube().deployment(), {"kind": "Deployment"})
        request = self.requests[0]
        self.assertEqual((request.get_method(), request.full_url), ("GET", "https://10.43.0.1:443/apis/apps/v1/namespaces/duckpond/deployments/node"))
        self.assertEqual((request.get_header("Authorization"), request.data), ("Bearer token-1", None))

    def test_replicasets_are_listed_in_the_namespace(self):
        self.answer = b'{"items": [{"kind": "ReplicaSet"}]}'
        self.assertEqual(self.kube(host="fd00::1").replicasets(), [{"kind": "ReplicaSet"}])
        self.assertEqual(self.requests[0].full_url, "https://[fd00::1]:443/apis/apps/v1/namespaces/duckpond/replicasets")

    def test_a_dry_run_patch_is_a_strategic_merge_under_its_field_manager_with_dry_run_all(self):
        body = pc.image_patch("node", X, "77")
        self.kube().patch(body, dry_run=True)
        request = self.requests[0]
        self.assertEqual((request.get_method(), request.full_url), ("PATCH", "https://10.43.0.1:443/apis/apps/v1/namespaces/duckpond/deployments/node?fieldManager=pi-chief&dryRun=All"))
        self.assertEqual(request.get_header("Content-type"), "application/strategic-merge-patch+json")
        self.assertEqual(json.loads(request.data), body)

    def test_a_real_patch_carries_no_dry_run(self):
        self.kube().patch(pc.image_patch("node", X, "77"), dry_run=False)
        self.assertEqual(self.requests[0].full_url, "https://10.43.0.1:443/apis/apps/v1/namespaces/duckpond/deployments/node?fieldManager=pi-chief")

    def test_a_refusal_fails_with_the_apis_reason_and_message(self):
        status = b'{"kind": "Status", "reason": "Forbidden", "message": "deployments.apps \\"ccs\\" is forbidden"}'
        self.answer = urllib.error.HTTPError("https://api", 403, "Forbidden", {}, io.BytesIO(status))
        with self.assertRaises(pc.Failed) as failed:
            self.kube().deployment()
        self.assertEqual(str(failed.exception), 'the Kubernetes API answered 403 Forbidden: deployments.apps "ccs" is forbidden')

    def test_an_unreached_api_fails(self):
        self.answer = urllib.error.URLError("connection refused")
        with self.assertRaises(pc.Failed) as failed:
            self.kube().deployment()
        self.assertEqual(str(failed.exception), "the Kubernetes API was not reached: connection refused")


if __name__ == "__main__":
    unittest.main()
