#!/usr/bin/env python3
"""Seed an agent on a freshly founded network and prove a mention still runs it.

`ops/refound-net.sh` ends here. Everything before it checks a part — the node
serves, the services read back enabled, the set staged. Only this crosses all
of them at once: a chat mention becomes an attributed run only if the
attribution, the model registration, the capability announcement, the sandbox
and the executor credential are ALL intact, which is exactly the chain a
re-found silently breaks.

The seed is `ops/demo-seed.sh`'s, minus the demo content: `agent provision` ->
`runs register_model` on the `claude` capability -> one channel -> one
mention. Then it waits for the run to settle and for the agent's reply to land
in the channel, and exits non-zero if either never does.

The wallet password is read from stdin, never argv — it is a password.

    printf '%s\\n' "$PASSWORD" | ops/refound-smoke.py \\
        --node http://127.0.0.1:8844 --workspace ~/.ducktape/net \\
        --binary ~/.ducktape/net/ducktape --key ~/.ducktape/net/keys/operator.key
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

# A settled run is the slow step: the host boots a microVM, the CLI runs, the
# reply is submitted and finalized. Two minutes is a comfortable box on this
# hardware; a run that has not settled by then is wedged, not slow.
RUN_DEADLINE = 240
REPLY_DEADLINE = 60
POLL = 2


class Net:
    def __init__(self, url, workspace, binary, key, password):
        self.url = url.rstrip("/")
        self.workspace = workspace
        self.bin = binary
        self.key = key
        self.password = password
        with open(f"{workspace}/admin.token") as f:
            self.admin = f.read().strip()

    def _post(self, path, body, headers):
        req = urllib.request.Request(f"{self.url}{path}", data=body, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return r.status, r.read().decode()
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode()

    def query(self, target, query, optional=False):
        body = json.dumps({"target": target, "query": query}).encode()
        code, text = self._post("/v1/query", body, {"content-type": "application/json"})
        if code != 200:
            if optional:
                return None
            die(f"query {target} failed [{code}]: {text}")
        return json.loads(text)

    def view(self, index, query):
        body = json.dumps(query).encode()
        code, text = self._post(
            f"/v1/index/{index}/view", body, {"content-type": "application/json"}
        )
        if code != 200:
            die(f"{index} view failed [{code}]: {text}")
        return json.loads(text)

    def submit(self, target, payload):
        """the frameless operator lane — stamps the node's own validator key."""
        body = json.dumps({"target": target, "payload": payload}).encode()
        code, text = self._post(
            "/v1/submit",
            body,
            {"content-type": "application/json", "x-ducktape-admin-token": self.admin},
        )
        if code != 200:
            die(f"submit {target} failed [{code}]: {text}")
        return text

    def submit_user(self, target, payload):
        """user-signed: a model belongs to the wallet, not to the node identity."""
        nanos = time.time_ns()
        request = f"{target} {nanos} {json.dumps(payload).encode().hex()}"
        signed = subprocess.run(
            [self.bin, "user", "sign-frame", "--key", self.key],
            input=f"{self.password}\n{request}\n",
            capture_output=True,
            text=True,
        )
        if signed.returncode != 0:
            die(f"sign-frame failed: {signed.stderr.strip()}")
        frame = bytes.fromhex(signed.stdout.strip())
        code, text = self._post(
            "/v1/submit/frame", frame, {"content-type": "application/octet-stream"}
        )
        if code != 200:
            die(f"submit_user {target} failed [{code}]: {text}")
        return text

    def run_bin(self, *args):
        env = dict(os.environ)
        # the home is the workspace's PARENT — the home holds workspaces.
        env["DUCKTAPE_HOME"] = os.path.dirname(self.workspace.rstrip("/"))
        out = subprocess.run(
            [self.bin, *args], capture_output=True, text=True, env=env
        )
        if out.returncode != 0:
            die(f"{' '.join(args)} failed: {out.stderr.strip()}")
        return out.stdout


def die(message):
    print(f"\nrefound-smoke: {message}", file=sys.stderr)
    raise SystemExit(1)


def controller_account(net, pubkey_hex):
    """the account this wallet key belongs to — `account create` founded it."""
    got = net.query("identity", {"of_key": {"key": list(bytes.fromhex(pubkey_hex))}})
    account = (got or {}).get("account")
    if not account:
        die("the active wallet key is on no account — `account create` did not run")
    return account["number"]


def model_account(net, controller, display_name):
    got = net.query(
        "identity", {"controlled": {"by": controller, "from": 0, "limit": 256}}
    )
    matches = [
        a
        for a in got.get("accounts", [])
        if a.get("name") == display_name
        and (a.get("control", {}).get("program") or {}).get("executor") == "agent"
    ]
    if len(matches) != 1:
        die(f"expected exactly one {display_name} program account, got {len(matches)}")
    return matches[0]["number"]


def seed(net, agent_id, display):
    pub = net.run_bin("user", "key", "status", "--key", net.key).split()[-1]
    controller = controller_account(net, pub)

    program = json.loads(net.run_bin("agent", "model-program", agent_id))
    net.submit_user(
        "agent",
        {"provision": {"request_id": agent_id, "name": display, "program": program}},
    )
    account = model_account(net, controller, display)
    net.submit_user(
        "runs",
        {
            "configure_model": {
                "operation": {
                    "register_model": {
                        "account": account,
                        "agent_id": agent_id,
                        "display_name": display,
                        "capability": "claude",
                        "skills": [],
                    }
                }
            }
        },
    )
    print(f"  seeded {display} as account {account} on capability claude")
    return account


def messages(net, channel):
    """the channel's messages, or None when there is no such channel."""
    got = net.query(
        "chat",
        {"messages_range": {"channel_id": channel, "from_seq": 0, "limit": 200}},
        optional=True,
    )
    return None if got is None else got.get("messages", [])


def mention(net, account, agent_id, channel):
    """Post the mention and return ITS seq.

    Every wait below keys on that seq. A run and a reply left by an earlier
    smoke on the same network are indistinguishable from this one's by
    agent id alone, and a check that passes on last week's run is not a check.
    """
    if messages(net, channel) is None:
        net.submit(
            "chat",
            {"create_channel": {"channel_id": channel, "name": "Smoke",
                                "post_policy": "open"}},
        )
    before = max((m.get("seq", 0) for m in messages(net, channel)), default=0)
    # A mention span holds ONLY the `@name` token — the rest of the sentence
    # rides in its own plain span, which is the composer's shape.
    net.submit(
        "chat",
        {
            "post_message": {
                "channel_id": channel,
                # message ids are network-global, not channel-scoped, so a
                # fixed one refuses the second smoke this network ever runs.
                "message_id": f"{agent_id}-{time.time_ns()}",
                "blocks": [
                    {
                        "paragraph": [
                            {"text": f"@{agent_id}",
                             "marks": [{"mention": {"account": account}}]},
                            {"text": " say hello, then stop.", "marks": []},
                        ]
                    }
                ],
                "thread": None,
            }
        },
    )
    seq = max((m.get("seq", 0) for m in messages(net, channel)), default=0)
    if seq <= before:
        die(f"the mention was accepted but no message landed in #{channel}")
    print(f"  mentioned @{agent_id} in #{channel} at seq {seq}")
    return seq


def await_run(net, agent_id, channel, seq):
    """A mention that reaches no provider does NOT fail — it sits pending for
    hours. So the deadline here IS the assertion; there is nothing to catch."""
    deadline = time.monotonic() + RUN_DEADLINE
    seen = None
    while time.monotonic() < deadline:
        runs = net.view("runs", {"recent": {"agent_id": agent_id, "limit": 10}})
        for run in runs.get("runs", []):
            origin = run.get("origin") or {}
            if origin.get("channel_id") != channel or origin.get("seq") != seq:
                continue
            seen = run
            state = run.get("state")
            # `dispatched` is a unit variant, so it arrives as a bare string.
            if isinstance(state, dict) and "settled" in state:
                return run
        time.sleep(POLL)
    if seen is None:
        die(f"no run was requested for @{agent_id} from #{channel} seq {seq} within "
            f"{RUN_DEADLINE}s — the mention never became a run (attribution or "
            f"model registration)")
    die(f"the run {seen.get('run_id')} never settled within {RUN_DEADLINE}s — it is "
        f"{json.dumps(seen.get('state'))}; no provider announced the capability, "
        f"or the sandbox never started")


def await_reply(net, channel, seq):
    deadline = time.monotonic() + REPLY_DEADLINE
    while time.monotonic() < deadline:
        for message in messages(net, channel) or []:
            head = message.get("head", {})
            origin = head.get("content_origin")
            is_program = isinstance(origin, dict) and "Program" in origin
            if not is_program or message.get("seq", 0) <= seq:
                continue
            spans = [
                s.get("text", "")
                for b in head.get("blocks", [])
                for s in (b.get("paragraph") or [])
            ]
            return "".join(spans)
        time.sleep(POLL)
    die(f"the run settled but no reply reached #{channel} within {REPLY_DEADLINE}s")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--node", required=True, help="the node's http base url")
    ap.add_argument("--workspace", required=True, help="the founder's workspace dir")
    ap.add_argument("--binary", required=True, help="the ducktape binary to sign with")
    ap.add_argument("--key", required=True, help="the wallet key that owns the model")
    ap.add_argument("--agent-id", default="smoke")
    ap.add_argument("--name", default="Smoke")
    ap.add_argument("--channel", default="smoke")
    args = ap.parse_args()

    password = sys.stdin.readline().rstrip("\n")
    if not password:
        die("no wallet password on stdin")

    net = Net(args.node, args.workspace, args.binary, args.key, password)
    account = seed(net, args.agent_id, args.name)
    seq = mention(net, account, args.agent_id, args.channel)

    run = await_run(net, args.agent_id, args.channel, seq)
    settled = run["state"]["settled"]
    outcome = settled.get("outcome")
    print(f"  run {run['run_id']}  dispatched h{run['dispatched']['height']}"
          f"  settled h{settled['at']['height']}  outcome {outcome}")
    if outcome != "result_accepted":
        die(f"the run settled {outcome}: {settled.get('reason')}")

    reply = await_reply(net, args.channel, seq)
    print(f"  reply  {reply}")


if __name__ == "__main__":
    main()
