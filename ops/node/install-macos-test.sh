#!/usr/bin/env bash
# shellcheck disable=SC2016
# Exercises the macOS installer on Linux with command stubs. The rendered
# plist is the contract here: a custom --home gets private logs, --label gets
# its own LaunchAgent identity, and the default paths remain unchanged.
set -euo pipefail
# The single-quoted lines below are programs written into the command stubs;
# their variables must expand when the stubs run, not while they are created.

OPS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail(){ printf '\033[31m[install-macos-test] %s\033[0m\n' "$*" >&2; exit 1; }
contains(){ grep -F -- "$2" <<<"$1" >/dev/null || fail "$3"; }
absent(){ ! grep -F -- "$2" <<<"$1" >/dev/null || fail "$3"; }

mkdir -p "$TMP/bin" "$TMP/home/owner" "$TMP/home/rehearsal-a" "$TMP/home/rehearsal-b"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'printf "rehearsal-chain\\t%s/rehearsal-chain/node.toml\\n" "$DUCKTAPE_HOME"' \
  > "$TMP/ducktape"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' > "$TMP/ducktape-node-launcher"
printf '%s\n' '#!/usr/bin/env bash' 'printf Darwin' > "$TMP/bin/uname"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  '[ "$1" = -lint ] && [ -s "$2" ]' \
  '! grep -F @LOG_DIR@ "$2" >/dev/null' \
  > "$TMP/bin/plutil"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'out="$(/usr/bin/mktemp /tmp/ducktape-node-plist.XXXXXX)"' \
  'printf "%s\\n" "$out"' \
  > "$TMP/bin/mktemp"
chmod +x "$TMP/ducktape" "$TMP/ducktape-node-launcher" "$TMP/bin/uname" "$TMP/bin/plutil" "$TMP/bin/mktemp"

run_installer(){
  PATH="$TMP/bin:$PATH" HOME="$TMP/home/owner" \
    bash "$OPS/install-macos.sh" --dry-run --workspace rehearsal \
    --binary "$TMP/ducktape" "$@"
}

ISOLATED_A="$(run_installer --home "$TMP/home/rehearsal-a" --label rehearsal.a)"
ISOLATED_B="$(run_installer --home "$TMP/home/rehearsal-b" --label rehearsal.b)"
DEFAULT="$(run_installer)"

contains "$ISOLATED_A" "$TMP/home/owner/Library/LaunchAgents/rehearsal.a.plist" \
  'custom label did not get its own LaunchAgent path'
contains "$ISOLATED_A" "gui/$(id -u)/rehearsal.a" \
  'custom label did not get its own launchd identity'
contains "$ISOLATED_A" "$TMP/home/rehearsal-a/Library/Logs/ducktape/node.err.log" \
  'custom home did not get private stderr logs'
contains "$ISOLATED_A" "$TMP/home/rehearsal-a/Library/Logs/ducktape/node.out.log" \
  'custom home did not get private stdout logs'
absent "$ISOLATED_A" "$TMP/home/owner/Library/Logs/ducktape/node.err.log" \
  'custom home still points stderr at the owner log'

contains "$ISOLATED_B" "$TMP/home/rehearsal-b/Library/Logs/ducktape/node.err.log" \
  'second custom home did not get a distinct stderr log'
absent "$ISOLATED_A" "$TMP/home/rehearsal-b/Library/Logs/ducktape" \
  'first custom home rendered the second home log path'
absent "$ISOLATED_B" "$TMP/home/rehearsal-a/Library/Logs/ducktape" \
  'second custom home rendered the first home log path'

contains "$DEFAULT" "$TMP/home/owner/Library/LaunchAgents/dev.ducktape.node.plist" \
  'default label or LaunchAgent path changed'
contains "$DEFAULT" "$TMP/home/owner/Library/Logs/ducktape/node.err.log" \
  'default stderr log path changed'
contains "$DEFAULT" "$TMP/home/owner/Library/Logs/ducktape/node.out.log" \
  'default stdout log path changed'
absent "$DEFAULT" "$TMP/home/owner/.ducktape/Library/Logs/ducktape" \
  'default install unexpectedly moved logs under the default home'

printf '\033[32m[install-macos-test] ok\033[0m\n'
