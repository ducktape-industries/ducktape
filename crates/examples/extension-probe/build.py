#!/usr/bin/env python3
"""Build independently deployable example files; never link them into a node."""
from pathlib import Path
import os
import subprocess

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
TARGET = ROOT / "target" / "extension-probe"
OUTPUT = HERE / "artifacts"
OUTPUT.mkdir(exist_ok=True)
ENV = dict(os.environ, CARGO_TARGET_DIR=str(TARGET), CARGO_INCREMENTAL="0")

def run(*args):
    subprocess.run(args, cwd=ROOT, env=ENV, check=True)

for replacement, suffix in [(False, ""), (True, "-replacement")]:
    features = ["--features", "replacement"] if replacement else []
    for package, artifact in [("module", "extension_policy"), ("view", "extension_view")]:
        run("cargo", "build", "--manifest-path", str(HERE / package / "Cargo.toml"),
            "--target", "wasm32-unknown-unknown", "--release", *features)
        run(str(ROOT / "target/debug/guest-builder"), "componentize",
            str(TARGET / "wasm32-unknown-unknown/release" / f"{artifact}.wasm"),
            "--out", str(OUTPUT / f"{package}{suffix}.component.wasm"))
    run("cargo", "build", "--manifest-path", str(HERE / "service/Cargo.toml"), "--release", *features)
    destination = OUTPUT / f"service{suffix}"
    destination.write_bytes((TARGET / "release/extension-service").read_bytes())
    destination.chmod(0o755)
