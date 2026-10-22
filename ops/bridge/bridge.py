#!/usr/bin/env python3
# A chat bridge for one account: it hears each message chat attributes to the account, hands it to a fresh handler, and posts what the handler answers.
# Usage: ACCOUNT=N HANDLER='<command>' STATE=<file> [NODE=<url>] [DUCKTAPE=ducktape] bridge.py; the workspace and its password come from DUCKTAPE_HOME and DUCKTAPE_PASSWORD.
import json
import os
import re
import subprocess
import sys

TO = b"to/".hex()
REJECTED = re.compile(r"^ducktape: landed \d+ rejected ")


def ducktape(*args):
    node = os.environ.get("NODE")
    return [os.environ.get("DUCKTAPE", "ducktape"), *(["--node", node] if node else []), *args]


def run(*args):
    done = subprocess.run(ducktape(*args), capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit(f"bridge: ducktape {' '.join(args[:2])} failed: {done.stderr.strip()}")
    return json.loads(done.stdout)


def query(program, asked):
    return run("query", program, json.dumps(asked))


def send(seq, op):
    done = subprocess.run(ducktape("op", "chat", op, "--land"), capture_output=True, text=True)
    if done.returncode == 0:
        return
    told = done.stderr.strip()
    last = told.splitlines()[-1] if told else ""
    if not REJECTED.match(last):
        raise SystemExit(f"bridge: ducktape op chat failed on change {seq}: {told}")
    print(f"bridge: change {seq}: {last}", file=sys.stderr)


def seq_of(line, account):
    _, key, value = line.rstrip("\n").split("\t")
    prefix = TO + account.to_bytes(8, "big").hex()
    ours = key.startswith(prefix) and len(key) == len(prefix) + 16 and value != "-"
    return int(key[len(prefix):], 16) if ours else None


def handler_env():
    return {name: value for name, value in os.environ.items() if name != "DUCKTAPE_PASSWORD"}


class Bridge:
    def __init__(self, account, handler, state):
        self.account = account
        self.handler = handler
        self.state = state
        self.last = 0
        if os.path.exists(state):
            with open(state) as kept:
                self.last = int(kept.read())

    def handled(self, seq):
        self.last = seq
        with open(self.state + ".new", "w") as out:
            out.write(str(seq))
        os.replace(self.state + ".new", self.state)

    def hand(self, change):
        source = change["source"]
        heard = source["program"] == "chat" and source["object"]["kind"] == "message" and change["kind"] == "added"
        message = query("chat", {"message_by_id": {"message_id": source["object"]["id"]}})["message"] if heard else None
        if message and not message["deleted"]:
            told = json.dumps({"change": change, "message": message})
            done = subprocess.run(
                ["sh", "-c", self.handler], input=told, capture_output=True, text=True, env=handler_env()
            )
            if done.returncode != 0:
                raise SystemExit(f"bridge: the handler failed on change {change['seq']}: {done.stderr.strip()}")
            for line in filter(str.strip, done.stdout.splitlines()):
                send(change["seq"], line)
        self.handled(change["seq"])

    def catch_up(self):
        page = {}
        while True:
            answer = query("attribution", {"changes_to": {"recipient": self.account, "page": page}})["changes"]
            for change in answer["items"]:
                if change["seq"] > self.last:
                    self.hand(change)
            if answer["next"] is None:
                return
            page = {"after": answer["next"]}

    def follow(self, lines):
        for line in lines:
            seq = seq_of(line, self.account)
            if seq is not None and seq > self.last:
                self.hand(query("attribution", {"change": {"seq": seq}})["change"])


def main():
    bridge = Bridge(int(os.environ["ACCOUNT"]), os.environ["HANDLER"], os.environ["STATE"])
    follower = subprocess.Popen(ducktape("changes", "attribution"), stdout=subprocess.PIPE, text=True)
    try:
        bridge.catch_up()
        bridge.follow(follower.stdout)
    finally:
        follower.terminate()
        code = follower.wait()
    sys.exit(f"bridge: the node closed attribution's changes (ducktape exit {code})")


if __name__ == "__main__":
    main()
