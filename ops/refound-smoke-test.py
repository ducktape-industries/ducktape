"""Exercise the smoke's waits against a stubbed node.

The point of every assertion here is that the wait can FAIL. A check that
cannot go red is worse than no check: it reports the chain intact while a
mention reaches nobody, which is exactly the state this smoke exists to catch.
"""
import importlib.util
import pathlib

SMOKE = pathlib.Path(__file__).with_name("refound-smoke.py").resolve()
spec = importlib.util.spec_from_file_location("refound_smoke", SMOKE)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

# The real deadlines are minutes; the behaviour under test is what happens when
# one expires, so make expiring cheap.
smoke.RUN_DEADLINE = 1
smoke.REPLY_DEADLINE = 1
smoke.POLL = 0.05

SETTLED = {"settled": {"at": {"height": 9, "time": 9}, "outcome": "result_accepted"}}


class StubNet:
    """Answers the three calls the waits make, from canned state."""

    def __init__(self, runs=(), messages=()):
        self.runs = list(runs)
        self.messages = list(messages)
        self.submitted = []

    def view(self, index, query):
        assert index == "runs", index
        return {"runs": self.runs}

    def query(self, target, query, optional=False):
        assert target == "chat", target
        return {"messages": self.messages}

    def submit(self, target, payload):
        self.submitted.append((target, payload))
        if "post_message" in payload:
            self.messages.append({"seq": len(self.messages) + 1, "head": {}})
        return ""


def run_record(channel, seq, state):
    return {"run_id": f"attributed/{seq}/smoke", "agent_id": "smoke",
            "dispatched": {"height": 1, "time": 1},
            "origin": {"channel_id": channel, "kind": "chat_message", "seq": seq},
            "state": state}


def program_message(seq, text):
    return {"seq": seq, "head": {"content_origin": {"Program": 2},
                                 "blocks": [{"paragraph": [{"text": text}]}]}}


def refuses(call):
    try:
        call()
    except SystemExit as exit:
        return exit.code
    raise AssertionError("expected the wait to refuse, it returned")


# A run this smoke did not cause is not evidence for it. Same agent id, same
# channel, settled green — and the wait must still time out, because the seq
# says it belongs to an earlier mention.
stale = StubNet(runs=[run_record("smoke", 1, SETTLED)])
assert refuses(lambda: smoke.await_run(stale, "smoke", "smoke", 7)) == 1
print("PASS: a settled run at another seq does not satisfy this mention")

# Pending forever is the shape of "no provider announced the capability": the
# run exists, it is dispatched, and nothing ever takes it.
pending = StubNet(runs=[run_record("smoke", 7, "dispatched")])
assert refuses(lambda: smoke.await_run(pending, "smoke", "smoke", 7)) == 1
print("PASS: a run that never settles expires the deadline")

live = StubNet(runs=[run_record("smoke", 1, SETTLED), run_record("smoke", 7, SETTLED)])
got = smoke.await_run(live, "smoke", "smoke", 7)
assert got["origin"]["seq"] == 7, got
print("PASS: the run for this mention's seq is the one returned")

# The agent's OWN earlier reply is program-authored and sits in the same
# channel. Only a message after the mention can be an answer to it.
echo = StubNet(messages=[program_message(1, "hello from the last smoke")])
assert refuses(lambda: smoke.await_reply(echo, "smoke", 7)) == 1
print("PASS: a program message at or before the mention is not the reply")

replied = StubNet(messages=[program_message(1, "old"), program_message(8, "new")])
assert smoke.await_reply(replied, "smoke", 7) == "new"
print("PASS: the first program message after the mention is the reply")

# Message ids are network-global: a fixed one refuses the second smoke a
# network ever runs, so each mention stamps its own.
posted = StubNet(messages=[{"seq": 1, "head": {}}])
seq = smoke.mention(posted, 2, "smoke", "smoke")
assert seq == 2, seq
ids = [p["post_message"]["message_id"] for _, p in posted.submitted if "post_message" in p]
assert ids and ids[0].startswith("smoke-") and ids[0] != "smoke-", ids
assert not any("create_channel" in p for _, p in posted.submitted), posted.submitted
print("PASS: the mention lands with a per-run message id in an existing channel")
