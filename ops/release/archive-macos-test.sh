#!/usr/bin/env bash
# Native regression for managed ad-hoc packing. Inputs are read-only; all
# archives and the deliberately damaged copy go beneath a new output directory.
# Usage: archive-macos-test.sh /path/to/Ducktape.app /new/proof-directory
set -euo pipefail

[ "$(uname -s)" = Darwin ] || { echo 'native macOS required' >&2; exit 1; }
bundle="${1:?qualified ad-hoc Ducktape.app required}"
proof="${2:?new output directory required}"
[ ! -e "$proof" ] || { echo 'output directory already exists' >&2; exit 1; }
packer="$(cd "$(dirname "$0")" && pwd)/archive.sh"
codesign --verify --deep --strict "$bundle"
codesign -dv "$bundle" 2>&1 | grep -q '^Signature=adhoc$'
mkdir -p "$proof"
shasum -a 256 "$bundle/Contents/MacOS/ducktape-app" \
  "$bundle/Contents/MacOS/ducktape-launcher" > "$proof/input-before.sha256"

for seq in 1 2; do
  display="0.1.0+05567a22.rehearsal$seq"
  bash "$packer" --kind app --from "$bundle" --sequence "$seq" \
    --display "$display" --out-dir "$proof/seq$seq" > "$proof/seq$seq.log" 2>&1
  archive="$(sed -n 's/^archive: //p' "$proof/seq$seq.log")"
  [ -f "$archive" ]
  identity="$(zstd -dc "$archive" | tar -xOf - release.json)"
  [ "$identity" = "{\"sequence\":$seq,\"display\":\"$display\"}" ]
  mkdir "$proof/extracted$seq"
  zstd -dc "$archive" | tar -xf - -C "$proof/extracted$seq"
  codesign --verify --deep --strict "$proof/extracted$seq/Ducktape.app"
done

mkdir "$proof/damaged"
cp -R "$bundle" "$proof/damaged/Ducktape.app"
printf 'deliberate regression corruption\n' >> "$proof/damaged/Ducktape.app/Contents/MacOS/ducktape-app"
if bash "$packer" --kind app --from "$proof/damaged/Ducktape.app" \
  --out-dir "$proof/refused" > "$proof/refused.log" 2>&1; then
  echo 'damaged signature was accepted' >&2
  exit 1
fi
grep -q 'codesign_refused' "$proof/refused.log"
shasum -a 256 "$bundle/Contents/MacOS/ducktape-app" \
  "$bundle/Contents/MacOS/ducktape-launcher" > "$proof/input-after.sha256"
cmp "$proof/input-before.sha256" "$proof/input-after.sha256"
printf 'PASS: ad-hoc archives, both release identities, extracted signatures, damaged refusal, input preserved\n'
