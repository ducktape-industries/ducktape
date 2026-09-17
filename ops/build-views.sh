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
# Cargo hashes a path dependency's ABSOLUTE location into that package's
# `-C metadata` whenever the package sits outside the workspace root, and
# `-C metadata` seeds every symbol hash the crate emits. Views reach module
# crates outside `crates/views` (boards, chat-message, duckfs-core, runs,
# agent), so a view's bytes would otherwise depend on where the checkout
# lives — and two hosts could not build one founding set. Compile through one
# constant path so that location is the same string everywhere. The path is a
# single global name, so one build owns it at a time; `ops/views-repro-check.sh`
# names it too, to prove it never reaches a component's bytes.
source_root=/var/tmp/ducktape-view-root
lock="$source_root.lock"
# The holder's pid reclaims the lock after a build dies without its trap.
until mkdir "$lock" 2>/dev/null; do
  holder=$(cat "$lock/pid" 2>/dev/null || true)
  [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null || rm -rf "$lock"
  sleep 1
done
echo $$ > "$lock/pid"
trap 'rm -rf "$lock"' EXIT
[ ! -e "$source_root" ] || [ -L "$source_root" ] || {
  echo "$source_root exists and is not a symlink" >&2
  exit 1
}
ln -sfn "$repo" "$source_root"
printf -v CARGO_ENCODED_RUSTFLAGS '%s\x1f%s\x1f%s\x1f%s' \
  "--remap-path-prefix=$cargo_home=/cargo" \
  "--remap-path-prefix=$rustup_home=/rustup" \
  "--remap-path-prefix=$source_root=/ducktape" \
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
"${CARGO:-cargo}" build --locked --manifest-path "$source_root/crates/views/Cargo.toml" --release \
  --target wasm32-unknown-unknown "${arguments[@]}"
mkdir -p "$repo/target/views"
for package in "${packages[@]}"; do
  library=${package//-/_}
  wasm-tools component new "$view_target/wasm32-unknown-unknown/release/$library.wasm" \
    -o "$repo/target/views/$library.wasm"
done
