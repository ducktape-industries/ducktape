#!/usr/bin/env bash
# The macOS half of ops/node/install.sh: render ops/node/dev.ducktape.node.plist
# for one workspace and hand it to launchd as a per-user LaunchAgent. macOS
# only — on Linux the node is a system unit and install.sh is the script.
#
# A plist cannot expand `~`, `$HOME`, or a workspace selector, so the shipped
# file is a template and this script is what turns it into a loadable agent.
# Idempotent: every run re-renders the plist, boots the old agent out and the
# new one in, so it is also how you change the workspace, the log filter or the
# binary path.
#
# Usage:
#   ops/node/install-macos.sh --workspace <selector>
#   ops/node/install-macos.sh --workspace <selector> --rust-log 'info,ducktape::join=debug'
#   ops/node/install-macos.sh --dry-run --workspace <selector>   # print the plist
#   ops/node/install-macos.sh --uninstall
#
# <selector> is what `ducktape node run -n` takes: a registered chain id or any
# unique prefix of it (`ducktape node list`). Founding or joining the network is
# NOT this script's job — run `ducktape node init` / `ducktape node join` as
# yourself first, then install the agent over the workspace they wrote.
#
# The agent runs ducktape-node-launcher (beside the `ducktape` binary), which
# follows the network's node releases. A workspace the launcher has never
# owned gets its first release seeded from the binary here (`launcher
# install`); after that the launcher owns `current` and a re-run leaves it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$SCRIPT_DIR/dev.ducktape.node.plist"

log(){ printf '\033[36m[install-macos]\033[0m %s\n' "$*"; }
die(){ printf '\033[31m[install-macos] %s\033[0m\n' "$*" >&2; exit 1; }

DRY_RUN=0
UNINSTALL=0
WORKSPACE=""
LABEL="dev.ducktape.node"
RUST_LOG_FILTER="${RUST_LOG:-info}"
DUCK_HOME="${DUCKTAPE_HOME:-$HOME/.ducktape}"
DUCKTAPE_BIN="${DUCKTAPE_BIN:-}"
LOG_DIR="$HOME/Library/Logs/ducktape"

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --workspace) WORKSPACE="${2:?--workspace needs a selector}"; shift 2 ;;
    # a second network on the same Mac needs a second label, because the label
    # is the agent's identity in the user's launchd domain.
    --label) LABEL="${2:?--label needs a value}"; shift 2 ;;
    --rust-log) RUST_LOG_FILTER="${2:?--rust-log needs a filter}"; shift 2 ;;
    --home)
      DUCK_HOME="${2:?--home needs a directory}"
      LOG_DIR="$DUCK_HOME/Library/Logs/ducktape"
      shift 2
      ;;
    --binary) DUCKTAPE_BIN="${2:?--binary needs a path}"; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || die "refusing: this installs a LaunchAgent; on Linux run install.sh"

AGENT_DIR="$HOME/Library/LaunchAgents"
PLIST="$AGENT_DIR/$LABEL.plist"
DOMAIN="gui/$(id -u)"

run(){
  if [ "$DRY_RUN" = 1 ]; then
    printf '+ %s\n' "$*"
  else
    "$@"
  fi
}

# launchctl bootout answers non-zero when the agent is not loaded, which is the
# ordinary state on a first install and on a re-run after a reboot.
bootout_if_loaded(){
  if [ "$DRY_RUN" = 1 ]; then
    printf '+ launchctl bootout %s/%s\n' "$DOMAIN" "$LABEL"
  else
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  fi
}

if [ "$UNINSTALL" = 1 ]; then
  log "removing $LABEL"
  bootout_if_loaded
  run rm -f "$PLIST"
  log "done — the workspace under $DUCK_HOME is untouched"
  exit 0
fi

[ -n "$WORKSPACE" ] || die "--workspace <selector> is required"

if [ -z "$DUCKTAPE_BIN" ]; then
  DUCKTAPE_BIN="$(command -v ducktape || true)"
fi
[ -n "$DUCKTAPE_BIN" ] || die "no 'ducktape' on PATH — run 'make install-node', or pass --binary <path>"
# launchd resolves nothing: ProgramArguments[0] must be an absolute path that
# exists at load time, not a name and not a symlink into a PATH entry.
DUCKTAPE_BIN="$(cd "$(dirname "$DUCKTAPE_BIN")" && pwd)/$(basename "$DUCKTAPE_BIN")"
[ -x "$DUCKTAPE_BIN" ] || die "$DUCKTAPE_BIN is not executable"
# the launcher ships beside `ducktape` in every shape that ships it: a node
# archive's root, a cargo target directory, `make install-node`'s ~/.cargo/bin.
LAUNCHER_BIN="$(dirname "$DUCKTAPE_BIN")/ducktape-node-launcher"
[ -x "$LAUNCHER_BIN" ] || die "no ducktape-node-launcher beside $DUCKTAPE_BIN — run 'make install-node'"

# The launcher takes the workspace DIRECTORY: the one registered workspace the
# selector is a prefix of, as `-n` resolves it.
workspace_dir_of(){
  local id config matches=()
  while IFS=$'\t' read -r id config; do
    case "$id" in "$WORKSPACE"*) matches+=("$(dirname "$config")") ;; esac
  done < <(DUCKTAPE_HOME="$DUCK_HOME" "$DUCKTAPE_BIN" node list)
  [ "${#matches[@]}" -eq 1 ] || die "--workspace $WORKSPACE matches ${#matches[@]} registered workspaces under $DUCK_HOME ('ducktape node list'); found or join first, or pass more of the chain id"
  printf '%s' "${matches[0]}"
}
WORKSPACE_DIR="$(workspace_dir_of)"

# Every rendered value lands inside an XML text node, so markup in one would
# produce a plist launchd cannot parse, and `|` is the sed delimiter below. A
# chain id's `#` is fine in both.
for value in "$LABEL" "$WORKSPACE_DIR" "$DUCK_HOME" "$LAUNCHER_BIN" "$RUST_LOG_FILTER" "$LOG_DIR"; do
  case "$value" in
    *'<'*|*'>'*|*'&'*|*'"'*) die "refusing: '$value' carries XML markup" ;;
    *'|'*) die "refusing: the '|' in '$value' would break the substitution" ;;
  esac
done

# launchd hands a job a minimal PATH, so the Homebrew prefixes the sandbox's
# tool lookup searches (crates/services/sandbox/src/host_tools.rs) are named
# here the way ops/macos-preflight.sh names them.
AGENT_PATH="/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin"

render(){
  sed -e "s|@LABEL@|$LABEL|g" \
      -e "s|@LAUNCHER_BIN@|$LAUNCHER_BIN|g" \
      -e "s|@WORKSPACE_DIR@|$WORKSPACE_DIR|g" \
      -e "s|@DUCKTAPE_HOME@|$DUCK_HOME|g" \
      -e "s|@RUST_LOG@|$RUST_LOG_FILTER|g" \
      -e "s|@PATH@|$AGENT_PATH|g" \
      -e "s|@LOG_DIR@|$LOG_DIR|g" \
      "$TEMPLATE"
}

# The FIRST release only: once the launcher's state is on disk it owns
# `current`, and a re-seed would put back a release the network moved past.
seed_once(){
  if [ -f "$WORKSPACE_DIR/updates/state.json" ]; then
    log "$WORKSPACE_DIR is already under the launcher; it owns current/ from here"
    return
  fi
  run "$LAUNCHER_BIN" install --workspace "$WORKSPACE_DIR" \
    --config "$WORKSPACE_DIR/node.toml" --from "$DUCKTAPE_BIN"
}

if [ "$DRY_RUN" = 1 ]; then
  log "would write $PLIST:"
  render
  echo
  # the same validity check the real install runs, on a copy launchd never
  # sees: --dry-run is what tells you the rendering is loadable.
  scratch="$(mktemp -t ducktape-node-plist)"
  trap 'rm -f "$scratch"' EXIT
  render > "$scratch"
  plutil -lint "$scratch" >/dev/null || die "the rendered plist is not a valid property list"
  log "plutil -lint: OK"
  seed_once
  printf '+ mkdir -p %s %s\n' "$AGENT_DIR" "$LOG_DIR"
  bootout_if_loaded
  printf '+ launchctl bootstrap %s %s\n' "$DOMAIN" "$PLIST"
  exit 0
fi

log "1/4 seeding the first release under the launcher"
seed_once

log "2/4 log directory + agent directory"
mkdir -p "$AGENT_DIR" "$LOG_DIR"

log "3/4 rendering $PLIST"
render > "$PLIST"
plutil -lint "$PLIST" >/dev/null || die "the rendered plist is not a valid property list: $PLIST"

log "4/4 loading the agent into $DOMAIN"
bootout_if_loaded
launchctl bootstrap "$DOMAIN" "$PLIST"

log "done — 'launchctl print $DOMAIN/$LABEL' for its state, 'ducktape node status' once it serves"
log "logs: $LOG_DIR/node.err.log and $DUCK_HOME/<chain-id>/daemon.log"
