#!/usr/bin/env bash
# Run from the checkout root. Match guest-builder's canonical source prefixes;
# encoded flags preserve paths containing spaces and replace ambient Rust flags.
set -euo pipefail
repo=$(pwd -P)
cargo_home=$(cd "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)
# A toolchain installed without rustup (the agent guest's /opt/rust) has no
# rustup home; its sysroot is the prefix rust's own paths live under.
rustup_home=$(cd "${RUSTUP_HOME:-$HOME/.rustup}" 2>/dev/null && pwd -P || rustc --print sysroot)
view_target=${CARGO_TARGET_DIR:-$repo/target}
mkdir -p "$view_target"
view_target=$(cd "$view_target" && pwd -P)
printf -v CARGO_ENCODED_RUSTFLAGS '%s\x1f%s\x1f%s\x1f%s' \
  "--remap-path-prefix=$cargo_home=/cargo" \
  "--remap-path-prefix=$rustup_home=/rustup" \
  "--remap-path-prefix=$repo=/ducktape" \
  "--remap-path-prefix=$view_target=/view-builder"
export CARGO_ENCODED_RUSTFLAGS
export CARGO_TARGET_DIR="$view_target"
unset RUSTFLAGS
# wit-bindgen emits the component type into each Rust cdylib. No UI compiler,
# browser import adapters, or embedded guest bytes are involved.
packages=()
while (($#)); do
  case "$1" in
    -p) packages+=("$2"); shift 2 ;;
    *) echo "expected -p <view-package>, got $1" >&2; exit 1 ;;
  esac
done
(("${#packages[@]}")) || { echo "no view packages selected" >&2; exit 1; }
arguments=()
for package in "${packages[@]}"; do arguments+=(-p "$package"); done
"${CARGO:-cargo}" build --locked --manifest-path crates/views/Cargo.toml --release \
  --target wasm32-unknown-unknown "${arguments[@]}"
mkdir -p "$repo/target/views"
for package in "${packages[@]}"; do
  library=${package//-/_}
  wasm-tools component new "$view_target/wasm32-unknown-unknown/release/$library.wasm" \
    -o "$repo/target/views/$library.wasm"
done
