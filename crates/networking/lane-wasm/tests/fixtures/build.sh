#!/usr/bin/env bash
# Rebuild the echo guest fixture from `../fixture-guest` (needs the wasm32
# target and wasm-tools). Run from anywhere; writes `echo.media.wasm` here.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
target="${CARGO_TARGET_DIR:-$here/../../../../../target-lane-echo-guest}"
RUSTC_WRAPPER= CARGO_TARGET_DIR="$target" cargo build --release \
  --target wasm32-unknown-unknown --manifest-path "$here/../fixture-guest/Cargo.toml"
wasm-tools component new "$target/wasm32-unknown-unknown/release/lane_echo_guest.wasm" \
  -o "$here/echo.media.wasm"
wasm-tools component wit "$here/echo.media.wasm" >/dev/null
