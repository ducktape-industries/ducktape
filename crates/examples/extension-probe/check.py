#!/usr/bin/env python3
"""Compile native hosts, then build and deploy independent application artifacts."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
TARGET = ROOT / "target"
ENV = dict(os.environ, CARGO_TARGET_DIR=str(TARGET), CARGO_INCREMENTAL="0",
           RUSTC_WRAPPER="", RUST_MIN_STACK="134217728")


def cargo(*arguments):
    process = subprocess.Popen(
        ["timeout", "--kill-after=10s", "3600s", "cargo", *arguments, "--message-format=json"], cwd=ROOT, env=ENV,
        stdout=subprocess.PIPE, text=True,
    )
    artifacts = []
    for line in process.stdout:
        message = json.loads(line)
        if message["reason"] == "compiler-message":
            sys.stderr.write(message["message"].get("rendered") or "")
        if message["reason"] == "compiler-artifact" and message.get("executable"):
            artifacts.append(message)
    if process.wait():
        raise SystemExit("native compilation failed")
    return artifacts


def executable(artifacts, name):
    matches = [Path(item["executable"]) for item in artifacts
               if item["target"]["name"] == name]
    if len(matches) != 1:
        raise SystemExit(f"expected one executable for {name}, got {matches}")
    return matches[0]


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


subprocess.run(["timeout", "--kill-after=5s", "30s", "sudo", "-n", "true"], check=True)
app = executable(cargo("build", "-p", "ducktape-app"), "ducktape-app")
app_test = executable(cargo("test", "-p", "ducktape-app", "--no-run", "--bin",
                            "ducktape-app"), "ducktape-app")
node_artifacts = cargo("test", "-p", "node-bin", "--no-run", "--test", "extension_e2e")
node = executable(node_artifacts, "ducktape")
test = executable(node_artifacts, "extension_e2e")
cargo("build", "-p", "guest-builder")
frozen = {str(path): digest(path) for path in [app, app_test, node]}
subprocess.run(["timeout", "--kill-after=10s", "1800s", sys.executable, str(HERE / "build.py")], cwd=ROOT, env=ENV, check=True)
environment = dict(ENV, DUCK_EXTENSION_APP_BIN=str(app), DUCK_EXTENSION_APP_TEST=str(app_test))
subprocess.run(["timeout", "--kill-after=10s", "3600s", str(test), "independent_service_module_and_view_replace_without_rebuilding_native_hosts",
                "--ignored", "--exact", "--nocapture"], cwd=ROOT, env=environment, check=True)
assert frozen == {path: digest(Path(path)) for path in frozen}, "native binary changed"
print(json.dumps({"native_sha256": frozen}, indent=2))
