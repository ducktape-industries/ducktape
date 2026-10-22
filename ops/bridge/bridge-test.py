#!/usr/bin/env python3
# bridge.py against a stand-in ducktape that answers from a fixture and records every op, so each case runs offline and ends on an event: the stand-in's stream ending, or the bridge stopping.
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ACCOUNT = 30

STAND_IN = r'''#!/usr/bin/env python3
import json, os, signal, sys
with open(os.environ["FIXTURE"]) as source:
    fixture = json.load(source)
args = sys.argv[1:]
if args[0] == "--node":
    args = args[2:]
verb, program = args[0], args[1]
if verb == "changes":
    with open(os.environ["FOLLOWER"], "w") as out:
        out.write(str(os.getpid()))
    sys.stdout.write("".join(line + "\n" for line in fixture["lines"]))
    sys.stdout.flush()
    if fixture.get("hold"):
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, 2)
        signal.pause()
    sys.exit(0)
asked = json.loads(args[2])
if verb == "op":
    with open(os.environ["OPS"], "a") as log:
        log.write(json.dumps({"program": program, "op": asked, "password": "DUCKTAPE_PASSWORD" in os.environ}) + "\n")
    answer = fixture.get("answer")
    if answer:
        sys.stderr.write(answer + "\n")
        sys.exit(1)
    print(json.dumps({"landed": 9, "ok": True}))
    sys.exit(0)
changes = {str(change["seq"]): change for change in fixture["changes"]}


def said(name, value):
    print(json.dumps({name: value}))
if "changes_to" in asked:
    after = asked["changes_to"]["page"].get("after")
    pages = fixture["pages"]
    at = 0 if after is None else int(after) + 1
    items = [changes[str(seq)] for seq in pages[at]]
    following = str(at) if at + 1 < len(pages) else None
    said("changes", {"height": 9, "items": items, "next": following})
elif "change" in asked:
    said("change", changes.get(str(asked["change"]["seq"])))
elif "message_by_id" in asked:
    said("message", fixture["messages"].get(asked["message_by_id"]["message_id"]))
else:
    sys.stderr.write(f"ducktape: there is no variant named {next(iter(asked))}\n")
    sys.exit(1)
'''


def read(path):
    with open(path) as source:
        return source.read()


def key(account, seq):
    return b"to/".hex() + account.to_bytes(8, "big").hex() + seq.to_bytes(8, "big").hex()


def line(account, seq, value=""):
    return f"12\t{key(account, seq)}\t{value}"


def change(seq, message, kind="added", recipient=ACCOUNT):
    return {
        "seq": seq,
        "source": {"program": "chat", "object": {"kind": "message", "id": message}},
        "recipient": recipient,
        "kind": kind,
    }


def message(message_id, text, deleted=False):
    return {"channel_id": "general", "seq": 4, "message_id": message_id, "text": text, "deleted": deleted}


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


class Bridge(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        root = self.dir.name
        self.stand_in = os.path.join(root, "ducktape")
        with open(self.stand_in, "w") as out:
            out.write(STAND_IN)
        os.chmod(self.stand_in, 0o755)
        self.fixture = os.path.join(root, "fixture.json")
        self.ops = os.path.join(root, "ops.log")
        self.seen = os.path.join(root, "seen.log")
        self.passwords = os.path.join(root, "passwords.log")
        self.follower = os.path.join(root, "follower.pid")
        self.state = os.path.join(root, "state")

    def tearDown(self):
        self.dir.cleanup()

    def run_bridge(self, fixture, handler=None):
        with open(self.fixture, "w") as out:
            json.dump(fixture, out)
        answer = '{"add_reaction": {"channel_id": "general", "seq": 4, "emoji": "👀"}}'
        told = f"cat >> \"$SEEN\"; echo >> \"$SEEN\"; echo \"pw=${{DUCKTAPE_PASSWORD:-}}\" >> \"$PASSWORDS\"; echo '{answer}'"
        env = dict(
            os.environ,
            DUCKTAPE=self.stand_in,
            DUCKTAPE_PASSWORD="the-wallet-password",
            FIXTURE=self.fixture,
            OPS=self.ops,
            SEEN=self.seen,
            PASSWORDS=self.passwords,
            FOLLOWER=self.follower,
            ACCOUNT=str(ACCOUNT),
            STATE=self.state,
            HANDLER=handler or told,
        )
        return subprocess.run(
            [sys.executable, os.path.join(HERE, "bridge.py")], env=env, capture_output=True, text=True
        )

    def seen_texts(self):
        if not os.path.exists(self.seen):
            return []
        return [json.loads(row)["message"]["text"] for row in read(self.seen).splitlines() if row.strip()]

    def posted(self):
        if not os.path.exists(self.ops):
            return []
        return [json.loads(row) for row in read(self.ops).splitlines()]

    def world(self):
        return {
            "changes": [
                change(1, "m1"),
                change(2, "m1", kind="withdrawn"),
                change(3, "m3", recipient=31),
                change(5, "m5"),
                change(6, "gone"),
            ],
            "pages": [[1], [2]],
            "messages": {"m1": message("m1", "first"), "m5": message("m5", "fifth"), "gone": message("gone", "", deleted=True)},
            "lines": [
                line(ACCOUNT, 2),
                line(31, 3),
                line(ACCOUNT, 4, "-"),
                "12\t" + b"change/".hex() + (5).to_bytes(8, "big").hex() + "\t00",
                line(ACCOUNT, 5),
                line(ACCOUNT, 6),
            ],
        }

    def test_it_catches_up_then_follows_and_answers_only_what_is_its_own(self):
        done = self.run_bridge(self.world())
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("closed attribution's changes", done.stderr)
        self.assertEqual(self.seen_texts(), ["first", "fifth"])
        self.assertEqual([row["program"] for row in self.posted()], ["chat", "chat"])
        self.assertEqual(self.posted()[0]["op"]["add_reaction"]["emoji"], "👀")
        self.assertEqual(read(self.state), "6")

    def test_the_handler_never_sees_the_wallet_password_and_ducktape_does(self):
        self.run_bridge(self.world())
        self.assertEqual(read(self.passwords).splitlines(), ["pw=", "pw="])
        self.assertEqual([row["password"] for row in self.posted()], [True, True])

    def test_a_restart_hands_nothing_it_handled_before(self):
        with open(self.state, "w") as out:
            out.write("6")
        fixture = self.world()
        fixture["pages"] = [[1, 2], [5, 6]]
        self.run_bridge(fixture)
        self.assertEqual(self.seen_texts(), [])
        self.assertEqual(self.posted(), [])

    def test_the_followed_change_it_handled_last_is_not_handed_again(self):
        with open(self.state, "w") as out:
            out.write("5")
        fixture = self.world()
        fixture["lines"] = [line(ACCOUNT, 5)]
        self.run_bridge(fixture)
        self.assertEqual(self.seen_texts(), [])
        self.assertEqual(read(self.state), "5")

    def test_an_op_the_network_rejects_is_reported_and_its_change_counts_as_handled(self):
        fixture = self.world()
        fixture["answer"] = "ducktape: landed 9 rejected unauthorized: the room is members-only"
        done = self.run_bridge(fixture)
        self.assertIn("change 1: ducktape: landed 9 rejected unauthorized: the room is members-only", done.stderr)
        self.assertIn("closed attribution's changes", done.stderr)
        self.assertEqual(self.seen_texts(), ["first", "fifth"])
        self.assertEqual(read(self.state), "6")

    def test_a_rejection_after_another_line_of_stderr_is_still_a_rejection(self):
        fixture = self.world()
        fixture["answer"] = "a warning first\nducktape: landed 9 rejected wrong_state: the room is archived"
        done = self.run_bridge(fixture)
        self.assertIn("change 1: ducktape: landed 9 rejected wrong_state: the room is archived", done.stderr)
        self.assertEqual(read(self.state), "6")

    def test_a_node_it_cannot_reach_stops_the_bridge_before_its_change_counts(self):
        fixture = self.world()
        fixture["answer"] = "ducktape: https://node.example did not answer: connection refused"
        done = self.run_bridge(fixture)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("ducktape op chat failed on change 1: ducktape: https://node.example did not answer", done.stderr)
        self.assertFalse(os.path.exists(self.state))

    def test_a_failed_handler_stops_the_bridge_and_its_follower_before_its_change_counts(self):
        fixture = self.world()
        fixture["pages"] = [[]]
        fixture["lines"] = [line(ACCOUNT, 1)]
        fixture["hold"] = True
        done = self.run_bridge(fixture, handler="cat > /dev/null; echo no >&2; exit 3")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("the handler failed on change 1: no", done.stderr)
        self.assertFalse(os.path.exists(self.state))
        self.assertEqual(self.posted(), [])
        follower = int(read(self.follower))
        still = alive(follower)
        if still:
            os.kill(follower, 9)
        self.assertFalse(still, "ducktape changes outlived the bridge")


if __name__ == "__main__":
    unittest.main()
