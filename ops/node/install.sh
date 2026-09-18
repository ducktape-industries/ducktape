#!/usr/bin/env bash
# The executable form of docs/deploy/node-service.md's Install/Enable
# sections. Idempotent: every step is safe to re-run (useradd -m is not used,
# `install -d`/`cp` are unconditional, `systemctl enable --now` on an already
# enabled+running unit is a no-op). Linux/systemd only — the units and paths
# this script writes (`/etc/systemd/system`, `/var/lib/ducktape`) have no
# other-platform equivalent. On macOS the node is a per-user LaunchAgent
# instead: install-macos.sh beside this file.
#
# Usage:
#   ops/node/install.sh --workspace <name> --init [-- <node init args...>]
#   ops/node/install.sh --workspace <name> --join <invite> [--genesis <file>]
#   ops/node/install.sh --dry-run --workspace <name> --init
#
# `--genesis <file>` is the founder's `<workspace>/genesis`: a member (an
# identity the founder `admit`ted before genesis) boots from its own copy, so
# its join needs the file; a resident fetches it off the mesh at first boot.
#
# `--workspace <name>` is what `ducktape node run -n` takes: the chain id or a
# unique prefix of it. The units name the workspace DIRECTORY, so once the
# network is founded or joined this resolves it to the one full chain id.
#
# The node runs under ducktape-node-launcher, which follows the network's
# node releases: this seeds the first release from the installed binary and
# its founding set (`launcher install`), and the launcher pins the network's
# release key on its first read.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

log(){ printf '\033[36m[install]\033[0m %s\n' "$*"; }
die(){ printf '\033[31m[install] %s\033[0m\n' "$*" >&2; exit 1; }

DRY_RUN=0
WORKSPACE=""
MODE=""       # "init" or "join"
INVITE=""
GENESIS=""
INIT_ARGS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --workspace) WORKSPACE="${2:?--workspace needs a value}"; shift 2 ;;
    --init) MODE="init"; shift ;;
    --join) MODE="join"; INVITE="${2:?--join needs an invite blob}"; shift 2 ;;
    --genesis) GENESIS="${2:?--genesis needs a file}"; shift 2 ;;
    --) shift; INIT_ARGS=("$@"); break ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "$WORKSPACE" ] || die "--workspace <name> is required"
[ -n "$MODE" ] || die "one of --init or --join <invite> is required"

# run() either prints the command (--dry-run) or executes it. sudo_run()
# is the same but only the lines that touch root-owned paths need it.
run(){
  if [ "$DRY_RUN" = 1 ]; then
    printf '+ %s\n' "$*"
  else
    "$@"
  fi
}
sudo_run(){ run sudo "$@"; }

if [ "$DRY_RUN" = 0 ]; then
  [ "$(uname -s)" = "Linux" ] || die "refusing: this installs a systemd service, not available on $(uname -s)"
  command -v systemctl >/dev/null 2>&1 || die "refusing: systemctl not found (no systemd on this host)"
fi

DUCK_HOME=/var/lib/ducktape
# the program: `ducktape`, `ducktape-node-launcher` and the founding set
# beside them — the shape of an unpacked node release, which is what
# `launcher install --from` seeds the first release from. Never under the
# home: the home holds one directory per network and nothing else.
PROGRAM_DIR=/usr/local/lib/ducktape
MODULES_DIR="$PROGRAM_DIR/modules"
# the founding set `make install-node` stages beside the built binary
# (what `workspace_config::modules_dir()` resolves for that binary).
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
MODULES_SRC="${DUCKTAPE_MODULES_DIR:-$CARGO_BIN/modules}"

log "1/7 building and installing ducktape, its launcher and its founding set (make install-node)"
run bash -c "cd '$REPO_ROOT' && make install-node"
sudo_run install -d -m 0755 "$PROGRAM_DIR"
sudo_run install -m 0755 "$CARGO_BIN/ducktape" "$CARGO_BIN/ducktape-node-launcher" "$PROGRAM_DIR/"
# on PATH by link, so the operator's `ducktape` resolves its founding set
# beside the real file.
sudo_run ln -sfn "$PROGRAM_DIR/ducktape" /usr/local/bin/ducktape
sudo_run ln -sfn "$PROGRAM_DIR/ducktape-node-launcher" /usr/local/bin/ducktape-node-launcher

log "2/7 dedicated user + state dir"
if [ "$DRY_RUN" = 1 ] || ! id ducktape >/dev/null 2>&1; then
  sudo_run useradd --system --home-dir "$DUCK_HOME" --shell /usr/sbin/nologin ducktape
fi
sudo_run usermod -aG kvm ducktape
sudo_run install -d -o ducktape -g ducktape -m 0700 "$DUCK_HOME"

# the founding set the service user founds from (`node init --modules`) and
# the first release carries beside its binary (the netstack guest a node
# reads at boot): every <id>.component.wasm, every <id>.index.wasm,
# netstack.component.wasm.
log "3/7 founding set"
sudo_run install -d -m 0755 "$MODULES_DIR"
if [ "$DRY_RUN" = 1 ]; then
  run bash -c "sudo cp '$MODULES_SRC'/*.wasm '$MODULES_DIR/'"
else
  shopt -s nullglob
  wasm_files=("$MODULES_SRC"/*.wasm)
  shopt -u nullglob
  [ "${#wasm_files[@]}" -gt 0 ] || die "no .wasm files in $MODULES_SRC (make install-node should have staged them)"
  sudo cp "${wasm_files[@]}" "$MODULES_DIR/"
fi
sudo_run chmod -R a+rX "$MODULES_DIR"

log "4/7 systemd units + log rotation"
sudo_run cp "$SCRIPT_DIR/ducktape-node@.service" "$SCRIPT_DIR/ducktape-service@.service" /etc/systemd/system/
sudo_run install -m 0644 "$SCRIPT_DIR/ducktape-node.logrotate" /etc/logrotate.d/ducktape-node
sudo_run systemctl daemon-reload

log "5/7 founding or joining the network as the service user"
DT=(sudo -u ducktape env "DUCKTAPE_HOME=$DUCK_HOME" /usr/local/bin/ducktape)
case "$MODE" in
  init) run "${DT[@]}" node init --name "$WORKSPACE" --modules "$MODULES_DIR" "${INIT_ARGS[@]}" ;;
  join)
    if [ -n "$GENESIS" ]; then
      run "${DT[@]}" node join "$INVITE" --genesis "$GENESIS"
    else
      run "${DT[@]}" node join "$INVITE"
    fi ;;
esac

# the one registered chain id `--workspace` is a prefix of, as `-n` resolves
# it — the units name the directory, and a prefix names none.
chain_id_of(){
  local id matches=()
  while IFS=$'\t' read -r id _; do
    case "$id" in "$WORKSPACE"*) matches+=("$id") ;; esac
  done < <("${DT[@]}" node list)
  [ "${#matches[@]}" -eq 1 ] || die "--workspace $WORKSPACE matches ${#matches[@]} registered workspaces ('ducktape node list'); pass more of the chain id"
  printf '%s' "${matches[0]}"
}
if [ "$DRY_RUN" = 1 ]; then
  CHAIN_ID="<chain id of $WORKSPACE>"
  UNIT="ducktape-node@\$(systemd-escape '$CHAIN_ID')"
else
  CHAIN_ID="$(chain_id_of)"
  UNIT="ducktape-node@$(systemd-escape "$CHAIN_ID")"
fi
WS_DIR="$DUCK_HOME/$CHAIN_ID"

# the FIRST release only: once the launcher's state is on disk it owns
# `current`, and a re-seed would put back a release the network moved past.
seeded(){ [ "$DRY_RUN" = 0 ] && sudo test -f "$WS_DIR/updates/state.json"; }
if seeded; then
  log "6/7 $WS_DIR is already under the launcher; it owns current/ from here"
else
  log "6/7 seeding the first release under the launcher"
  run sudo -u ducktape env "DUCKTAPE_HOME=$DUCK_HOME" /usr/local/bin/ducktape-node-launcher install \
    --workspace "$WS_DIR" --config "$WS_DIR/node.toml" --from "$PROGRAM_DIR/ducktape"
fi
# the workspace ducktape-service@<kind> runs its daemon over.
sudo_run install -d -m 0755 /etc/ducktape
sudo_run sh -c "printf 'DUCKTAPE_WORKSPACE=\"%s\"\n' '$WS_DIR' > /etc/ducktape/workspace.env"

log "7/7 enable and start"
sudo_run systemctl enable --now "$UNIT"

log "done — 'ducktape node status' (as the ducktape user) once it serves; see docs/deploy/node-service.md"
