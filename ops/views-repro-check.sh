#!/usr/bin/env bash
# The fixed HEAD snapshot includes every tracked dependency of a view. Neither
# private worktrees nor the source checkout are modified by the two builds.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
wasm_tools_root=$1
work="$repo/target/views-repro"
rm -rf "$work"
mkdir -p "$work"
git -C "$repo" archive HEAD > "$work/source.tar"
for place in here there; do
  mkdir "$work/$place"
  tar -xf "$work/source.tar" -C "$work/$place"
  if [ "$place" = here ]; then
    view_target="$work/$place/target"
  else
    view_target="$work/outside-target"
  fi
  CARGO_TARGET_DIR="$view_target" make -C "$work/$place" views WASM_TOOLS_ROOT="$wasm_tools_root"
  (cd "$work/$place/target/views" && printf '%s\n' *.wasm | sort) > "$work/$place.names"
done
cmp "$work/here.names" "$work/there.names"
count=0
for component in "$work/here/target/views/"*.wasm; do
  [ -f "$component" ] || { echo 'no view components were built' >&2; exit 1; }
  name=$(basename "$component")
  cmp "$component" "$work/there/target/views/$name"
  for prefix in "$work/here" "$work/there" "$work/outside-target" \
    "$(cd "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)" \
    "$(cd "${RUSTUP_HOME:-$HOME/.rustup}" && pwd -P)" \
    "$(cd "$HOME" && pwd -P)"; do
    [ "$prefix" != / ] || continue
    if grep -aFq -- "$prefix/" "$component"; then
      echo "builder-local prefix in $name: $prefix" >&2
      exit 1
    fi
  done
  count=$((count + 1))
done
echo "$count views are byte-identical from two HEAD snapshots with no builder-home paths"
