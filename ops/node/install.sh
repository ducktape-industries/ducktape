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
#   ops/node/install.sh --workspace <name> --join-file <file> [--genesis <file>]
#   ops/node/install.sh --dry-run --workspace <name> --init
#   ops/node/install.sh --archive <node archive> --workspace <name> --join-file <file>
#   ops/node/install.sh --user --workspace <name>
#
# `--user` needs no root: it runs a workspace under ~/.ducktape that is
# already under the launcher (`ducktape-node-launcher install` wrote its
# `updates/state.json`) as this user's own systemd unit,
# ducktape-node-user@<chain id>, with the `ducktape-node-launcher` on PATH
# copied into the workspace. It founds, joins and builds nothing.
#
# `--archive <file>` installs the program from a node release archive
# (`ops/release/archive.sh --kind node`: `ducktape`, `ducktape-node-launcher`,
# `modules/` and `release.json` at its root) instead of building this
# checkout, and needs `zstd`. Without it, `make install-node` builds one.
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
USER_MODE=0
WORKSPACE=""
MODE=""       # "init" or "join"
INVITE_FILE=""
GENESIS=""
ARCHIVE=""
INIT_ARGS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --user) USER_MODE=1; shift ;;
    --workspace) WORKSPACE="${2:?--workspace needs a value}"; shift 2 ;;
    --init) MODE="init"; shift ;;
    --join-file) MODE="join"; INVITE_FILE="${2:?--join-file needs a file}"; shift 2 ;;
    --genesis) GENESIS="${2:?--genesis needs a file}"; shift 2 ;;
    --archive) ARCHIVE="${2:?--archive needs a node release archive}"; shift 2 ;;
    --) shift; INIT_ARGS=("$@"); break ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "$WORKSPACE" ] || die "--workspace <name> is required"
[ "$USER_MODE" = 1 ] || [ -n "$MODE" ] || die "one of --init or --join-file <file> is required"

if [ "$DRY_RUN" = 0 ] && [ "$MODE" = join ]; then
  [ -f "$INVITE_FILE" ] || die "invite file is not a regular file: $INVITE_FILE"
  [ -r "$INVITE_FILE" ] || die "invite file is not readable: $INVITE_FILE"
  INVITE_MODE=$(stat -c '%a' -- "$INVITE_FILE") || die "cannot read invite file mode: $INVITE_FILE"
  [ "$INVITE_MODE" = 600 ] || die "invite file must be mode 600, got $INVITE_MODE: $INVITE_FILE"
fi

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
  [ -z "$ARCHIVE" ] || [ -f "$ARCHIVE" ] || die "no such archive: $ARCHIVE"
  [ -z "$ARCHIVE" ] || command -v zstd >/dev/null 2>&1 || die "--archive needs zstd to unpack $ARCHIVE"
fi

if [ "$USER_MODE" = 1 ]; then
  [ -z "$MODE$ARCHIVE$GENESIS" ] || die "refusing: --user founds, joins and unpacks nothing — it runs a workspace already under the launcher"
  USER_HOME="$HOME/.ducktape"
  UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  matches=()
  for dir in "$USER_HOME/$WORKSPACE"*/; do
    [ -d "$dir" ] && matches+=("$(basename "$dir")")
  done
  [ "${#matches[@]}" -eq 1 ] || die "--workspace $WORKSPACE matches ${#matches[@]} workspaces under $USER_HOME; pass more of the chain id"
  CHAIN_ID="${matches[0]}"
  WS_DIR="$USER_HOME/$CHAIN_ID"
  # the unit runs the launcher, which refuses a workspace it was never
  # installed into: enabling it would only restart that refusal forever.
  [ -f "$WS_DIR/updates/state.json" ] || die "refusing: $WS_DIR is not under the launcher (no updates/state.json) — seed it first: ducktape-node-launcher install --workspace '$WS_DIR' --config '$WS_DIR/node.toml' --from <ducktape>"
  LAUNCHER="$(command -v ducktape-node-launcher)" || die "refusing: no ducktape-node-launcher on PATH to copy into $WS_DIR"
  UNIT="ducktape-node-user@$(systemd-escape "$CHAIN_ID")"

  log "1/3 the launcher into $WS_DIR, the unit into $UNIT_DIR"
  run install -m 0755 "$LAUNCHER" "$WS_DIR/ducktape-node-launcher"
  run install -D -m 0644 "$SCRIPT_DIR/ducktape-node-user@.service" "$UNIT_DIR/ducktape-node-user@.service"
  run systemctl --user daemon-reload

  log "2/3 enable and start $UNIT"
  run systemctl --user enable --now "$UNIT"

  log "3/3 linger, so the node outlives this login and starts at boot"
  ME="${USER:-$(id -un)}"
  run loginctl enable-linger "$ME" \
    || log "linger needs an admin here, and without it the node stops at logout and waits for a login after a reboot: sudo loginctl enable-linger $ME"

  log "done — log: $WS_DIR/launcher.log; systemctl --user status|restart|stop '$UNIT'"
  exit 0
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

if [ -n "$ARCHIVE" ]; then
  log "1/7 unpacking ducktape, its launcher and its founding set from $ARCHIVE"
  SRC="$(mktemp -d)"
  trap 'rm -rf "$SRC"' EXIT
  run bash -c "zstd -dc '$ARCHIVE' | tar -C '$SRC' -xf -"
  MODULES_SRC="$SRC/modules"
else
  log "1/7 building ducktape, its launcher and its founding set (make install-node)"
  run bash -c "cd '$REPO_ROOT' && make install-node"
  SRC="$CARGO_BIN"
fi
sudo_run install -d -m 0755 "$PROGRAM_DIR"
sudo_run install -m 0755 "$SRC/ducktape" "$SRC/ducktape-node-launcher" "$PROGRAM_DIR/"
# an archive's identity rides beside the binary, where `launcher install`
# reads the sequence it pins; a build from source has none, and one left by
# an earlier archive would name another release.
sudo_run rm -f "$PROGRAM_DIR/release.json"
[ ! -f "$SRC/release.json" ] || sudo_run install -m 0644 "$SRC/release.json" "$PROGRAM_DIR/"
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
# reads at boot). It is more than the wasm: every <id>.component.wasm, every
# <id>.index.wasm and netstack.component.wasm, the `.staged-by` stamp naming
# the build that staged the set, every <id>.lanes file and every <id>.assets
# directory. The whole directory goes across, dotfiles included — a node that
# founds from a set missing the stamp refuses to boot, because a set no build
# claims is a set this binary cannot show it was built with.
log "3/7 founding set"
sudo_run install -d -m 0755 "$MODULES_DIR"
if [ "$DRY_RUN" = 1 ]; then
  run bash -c "sudo cp -R '$MODULES_SRC'/. '$MODULES_DIR/'"
else
  shopt -s nullglob
  wasm_files=("$MODULES_SRC"/*.wasm)
  shopt -u nullglob
  [ "${#wasm_files[@]}" -gt 0 ] || die "no .wasm files in $MODULES_SRC (make install-node or the archive should have carried them)"
  sudo cp -R "$MODULES_SRC"/. "$MODULES_DIR/"
fi
sudo_run chmod -R a+rX "$MODULES_DIR"

log "4/7 systemd units + log rotation"
sudo_run cp "$SCRIPT_DIR/ducktape-node@.service" "$SCRIPT_DIR/ducktape-service@.service" /etc/systemd/system/
sudo_run install -m 0644 "$SCRIPT_DIR/ducktape-node.logrotate" /etc/logrotate.d/ducktape-node
sudo_run systemctl daemon-reload

log "5/7 founding or joining the network as the service user"
DT=(sudo -u ducktape env "DUCKTAPE_HOME=$DUCK_HOME" /usr/local/bin/ducktape)
join_from_file(){
  if [ "$DRY_RUN" = 1 ]; then
    printf '+'
    printf ' %q' "${DT[@]}" node join
    if [ -n "$GENESIS" ]; then
      printf ' --genesis %q' "$GENESIS"
    fi
    printf ' < %q\n' "$INVITE_FILE"
    return
  fi
  if [ -n "$GENESIS" ]; then
    run "${DT[@]}" node join --genesis "$GENESIS" < "$INVITE_FILE"
  else
    run "${DT[@]}" node join < "$INVITE_FILE"
  fi
}
case "$MODE" in
  init) run "${DT[@]}" node init --name "$WORKSPACE" --modules "$MODULES_DIR" "${INIT_ARGS[@]}" ;;
  join) join_from_file ;;
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
