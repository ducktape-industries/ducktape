#!/usr/bin/env bash
# Re-found a ducktape network from nothing: tear down what is running, archive
# its workspaces, found a validator, join a resident, install the executors,
# enable the services, mirror a git repo into the forge, and print what an
# operator needs to reach it.
#
# This is a ROUTINE, not a one-time ceremony. A network is re-founded whenever
# a wire moves — no compat, no migration — so this script is written to be run
# again over the same roots and reach the same end state.
#
# IT TAKES ITS TARGET EXPLICITLY AND HAS NO DEFAULT. Every destructive step is
# scoped to the roots named on the command line: it stops only processes whose
# /proc/<pid>/exe lives under one of them, and it ARCHIVES workspaces by moving
# them aside, never by deleting. A `pkill -f` pattern would match an editor, a
# grep, or another network's node, so pids are resolved by executable path.
set -euo pipefail

BIN_SRC=""
ROOT=""
JOINER_ROOT=""
NAME=""
GUEST_SRC=""
MIRROR_REPO=""
ASSUME_YES=0
SKIP_APP=0
WALLET_NAME="operator"
WALLET_PASSWORD="ducktape"

# The founder's and the resident's port sets. They must not collide with each
# other or with anything else on the host: two workspaces on one box share the
# fixed defaults, and the join then fails with a message that blames the invite
# rather than the address already in use.
#
# These defaults are the LIVE network's, so proving this script on a scratch
# pair while that network is up needs `--port-offset` — otherwise the scratch
# founder binds the real one's http port and the rehearsal takes down the thing
# it was rehearsing for.
PORT_OFFSET=0
F_P2P=35620 F_HTTP=32989 F_GATEWAY=33989 F_RPC=34989 F_WG=46700 F_INVITE=46701
J_P2P=35630 J_HTTP=32990 J_GATEWAY=33990 J_RPC=34990 J_WG=46710 J_INVITE=46711

LAUNCHER_EXE="ducktape-node-launcher"

die() { printf '\nrefound-net: %s\n' "$*" >&2; exit 1; }
say() { printf '\n=== %s ===\n' "$*"; }

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    cat <<'USAGE'

usage: ops/refound-net.sh --root DIR [options]

  --root DIR          REQUIRED. the founder's workspace root. no default: the
                      only thing that ever points this at a live network is an
                      operator typing its path.
  --joiner-root DIR   the resident's workspace root (default: <root>-joiner)
  --name NAME         the network name (default: the root's basename)
  --binary PATH       the node binary to found with. default: build one from
                      this checkout. Whatever it is, it is COPIED into the
                      workspace and vouched against this checkout's HEAD.
  --guest DIR         a guest image directory (vmlinux + rootfs.ext4) to install
                      as the workspace's own. runs boot what is here.
  --mirror REPO       a git checkout to import into the network's forge.
  --port-offset N     add N to every port. the defaults are the live network's,
                      so a scratch run alongside it needs an offset.
  --wallet-name NAME  the workspace's active wallet, and the display name of
                      the account founded for it (default: operator). the
                      service daemons refuse to boot without both.
  --wallet-password P its password (default: ducktape). the mnemonic is written
                      to a 0600 file in the workspace, never to stdout.
  --skip-app          do not rebuild the desktop app.
  --yes               proceed past the teardown/archive of an existing root.
USAGE
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --root) ROOT=${2:-}; shift 2;;
        --joiner-root) JOINER_ROOT=${2:-}; shift 2;;
        --name) NAME=${2:-}; shift 2;;
        --binary) BIN_SRC=${2:-}; shift 2;;
        --guest) GUEST_SRC=${2:-}; shift 2;;
        --mirror) MIRROR_REPO=${2:-}; shift 2;;
        --port-offset) PORT_OFFSET=${2:-0}; shift 2;;
        --wallet-name) WALLET_NAME=${2:-}; shift 2;;
        --wallet-password) WALLET_PASSWORD=${2:-}; shift 2;;
        --skip-app) SKIP_APP=1; shift;;
        --yes|-y) ASSUME_YES=1; shift;;
        -h|--help) usage;;
        *) die "unknown flag $1 (try --help)";;
    esac
done

[ -n "$ROOT" ] || usage
case "$ROOT" in /*) :;; *) die "--root must be an absolute path";; esac
[ -n "$JOINER_ROOT" ] || JOINER_ROOT="${ROOT}-joiner"
[ -n "$NAME" ] || NAME=$(basename "$ROOT")
CHECKOUT=$(cd "$(dirname "$0")/.." && pwd)
STAMP=$(date +%Y%m%d-%H%M%S)

if [ "$PORT_OFFSET" -ne 0 ]; then
    for v in F_P2P F_HTTP F_GATEWAY F_RPC F_WG F_INVITE \
             J_P2P J_HTTP J_GATEWAY J_RPC J_WG J_INVITE; do
        eval "$v=\$(( \$$v + PORT_OFFSET ))"
    done
fi

# NOTE: the port check does NOT live here. Freeing these ports is what the
# teardown below does, so checking them before it would refuse every re-run
# over the same root — the exact case this script exists to serve. It runs
# after the teardown instead, where a port still held means something OUTSIDE
# the target roots owns it.

# --------------------------------------------------------------------------
# 1. the binary, and its vouch.
#
# A shared cargo target directory holds whichever worktree built it LAST, so
# the binary sitting there may belong to a sibling checkout whose wire has
# already moved. Founding with one of those produces a network whose frames no
# other build can read, and the failure surfaces much later as a decode error
# nobody connects to the founding. So: copy first, then make the copy prove its
# ancestry before anything is founded with it.
# --------------------------------------------------------------------------
say "binary"
if [ -z "$BIN_SRC" ]; then
    echo "building a node binary from $CHECKOUT"
    ( cd "$CHECKOUT" && cargo build --release -p node-bin >&2 )
    BIN_SRC="$CHECKOUT/target/release/ducktape"
    [ -x "$BIN_SRC" ] || BIN_SRC=$(cd "$CHECKOUT" && cargo metadata --format-version 1 --no-deps \
        | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/release/ducktape
fi
[ -x "$BIN_SRC" ] || die "no node binary at $BIN_SRC"

# The binary and the founding set are ONE artifact and are pinned together.
# `node init` resolves the set beside the binary, so a binary copied on its own
# finds nothing; and a set left in the shared cargo target is rewritten by
# whichever checkout builds next, which silently changes what this network is
# founded with.
STAGE="/tmp/refound-$NAME-$STAMP"
mkdir -p "$STAGE"
STAGED_BIN="$STAGE/ducktape"
cp "$BIN_SRC" "$STAGED_BIN"
VOUCH=$("$STAGED_BIN" --version | awk '{print $2}')
# `<version>+<short sha>`, or `<version>+<short sha>-<diff digest>` when the
# tree the binary was built from had tracked changes. The digest is not a rev,
# so it comes off before the ancestry test — but it stays in the line printed
# below, because "this binary matches no commit" is worth seeing.
COMMIT=${VOUCH##*+}
COMMIT=${COMMIT%%-*}
echo "staged $BIN_SRC -> $STAGED_BIN  (version $VOUCH)"
if ! ( cd "$CHECKOUT" && git merge-base --is-ancestor "$COMMIT" HEAD ) 2>/dev/null; then
    die "the binary stamps $COMMIT, which is NOT an ancestor of this checkout's HEAD.
     that is a sibling worktree's build: founding with it yields frames this
     checkout cannot read. build here, or pass --binary explicitly."
fi
echo "vouched: $COMMIT is an ancestor of HEAD"

MODULES_SRC=${DUCKTAPE_MODULES_DIR:-}
if [ -z "$MODULES_SRC" ]; then
    BIN_DIR=$(dirname "$BIN_SRC")
    ESCAPED=$(printf '%s' "$CHECKOUT" | tr '/' '%')
    for cand in "$BIN_DIR/modules$ESCAPED" "$BIN_DIR/modules"; do
        if [ -d "$cand" ]; then MODULES_SRC=$cand; break; fi
    done
fi
[ -n "$MODULES_SRC" ] && [ -d "$MODULES_SRC" ] \
    || die "no founding set found. build in this checkout, or set \$DUCKTAPE_MODULES_DIR."

# A `*.pending` marker is a view whose staging was interrupted; founding on one
# fails at genesis with that file named. Catch it here rather than three steps
# in, and say what fixes it.
#
# Counted with a nullglob array, NOT `ls glob | wc -l`: under `pipefail` a glob
# that matches nothing makes `ls` exit 2, the pipeline inherits it, and `set -e`
# kills the script on the assignment — so the HEALTHY case is the one that
# aborts the run.
shopt -s nullglob
pending_views=( "$MODULES_SRC"/*.pending )
staged_entries=( "$MODULES_SRC"/* )
shopt -u nullglob
if [ "${#pending_views[@]}" -ne 0 ]; then
    printf 'refound-net: the founding set at %s has %d pending view(s):\n' \
        "$MODULES_SRC" "${#pending_views[@]}" >&2
    printf '  %s\n' "${pending_views[@]}" >&2
    die "run \`make views\` in $CHECKOUT and build again."
fi

cp -r "$MODULES_SRC" "$STAGE/modules"
echo "staged founding set $MODULES_SRC (${#staged_entries[@]} entries)"

# --------------------------------------------------------------------------
# 2. stop whatever is running under the target roots.
#
# By EXECUTABLE PATH, never by name pattern. `$!` of a `setsid nohup` wrapper
# is the wrapper, not the node, so the pid is resolved from /proc.
# --------------------------------------------------------------------------
# Everything holding this root: the node itself, whose exe lives under it, AND
# the launcher SUPERVISING it, whose exe lives in the checkout and which names
# the workspace in its argv. The launcher restarts its child on exit, so
# killing only the node gets a fresh node a second later — the supervisor has
# to go first. Matched on an exact path in argv, read from /proc, never a
# `pkill -f` pattern that would also match an editor or a grep.
pids_under() {
    local root=$1 d exe cwd args base
    for d in /proc/[0-9]*; do
        exe=$(readlink "$d/exe" 2>/dev/null) || continue
        # the node: its executable lives under the workspace.
        case "$exe" in "$root"/*) printf '%s\n' "${d#/proc/}"; continue;; esac
        # a process started with a RELATIVE selector — `node run --config
        # node.toml`, `service run agent --workspace .` — names nothing
        # absolute anywhere, and its binary can live outside the workspace
        # too. The only thing that places it is its working directory.
        # Narrowed to a ducktape executable so an operator's shell, editor or
        # `tail` sitting in the workspace is not swept up with the network.
        base=${exe##*/}
        cwd=$(readlink "$d/cwd" 2>/dev/null) || cwd=
        case "$base" in
            ducktape | ducktape-*)
                case "$cwd" in
                    "$root" | "$root"/*) printf '%s\n' "${d#/proc/}"; continue;;
                esac
                ;;
        esac
        # the launcher and the service daemons: both run a binary from
        # somewhere else and NAME this workspace in argv. Matching the absolute
        # root path in argv catches every one of them without the
        # false-positive risk of a loose `pkill -f` pattern.
        args=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null) || continue
        case "$args" in
            *"$root"/*)
                # do not match this script, or the shell that launched it.
                case "$args" in *refound-net.sh*) continue;; esac
                printf '%s\n' "${d#/proc/}"
                ;;
        esac
    done
}

# one service daemon of this founder, by the kind and config in its argv. The
# pid `$!` hands back belongs to the `setsid` wrapper, not to the daemon it
# execs, so the only honest answer comes from /proc.
service_pid() {
    local kind=$1 d args
    for d in /proc/[0-9]*; do
        args=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null) || continue
        case "$args" in
            *"service run $kind "*"$FOUNDER_CFG"*) printf '%s\n' "${d#/proc/}"; return;;
        esac
    done
}

# ordered so a supervisor is signalled before the child it would restart.
stop_pids() {
    local pids=$1 p
    for p in $pids; do
        case "$(readlink "/proc/$p/exe" 2>/dev/null)" in
            *"/$LAUNCHER_EXE") kill "$p" 2>/dev/null || true;;
        esac
    done
    sleep 1
    for p in $pids; do kill "$p" 2>/dev/null || true; done
}

say "teardown"
RUNNING=$( { pids_under "$ROOT"; pids_under "$JOINER_ROOT"; } | sort -u )
if [ -n "$RUNNING" ]; then
    echo "processes under the target roots:"
    for p in $RUNNING; do echo "  pid $p $(readlink "/proc/$p/exe" 2>/dev/null)"; done
    [ "$ASSUME_YES" = 1 ] || die "refusing to stop a running network without --yes"
    stop_pids "$RUNNING"
    # `if`, not `[ … ] && break`: a trailing `&&` that evaluates false is a
    # non-zero loop body under `set -e`, which exits the script instead of
    # taking the next lap.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        if [ -z "$( { pids_under "$ROOT"; pids_under "$JOINER_ROOT"; } | sort -u )" ]; then
            break
        fi
        sleep 2
    done
    LEFT=$( { pids_under "$ROOT"; pids_under "$JOINER_ROOT"; } | sort -u )
    [ -z "$LEFT" ] || die "processes still alive under the roots: $LEFT"
    echo "stopped."
else
    echo "nothing running under $ROOT or $JOINER_ROOT"
fi

# --------------------------------------------------------------------------
# 3. archive. MOVED ASIDE, NEVER DELETED — a re-found resets content by
# design, and the only copy of what was there is the one this step keeps.
# --------------------------------------------------------------------------
say "archive"
ARCHIVED=""
for d in "$ROOT" "$JOINER_ROOT"; do
    if [ -e "$d" ]; then
        [ "$ASSUME_YES" = 1 ] || die "refusing to archive an existing $d without --yes"
        mv "$d" "$d.archived-$STAMP"
        ARCHIVED="$ARCHIVED $d.archived-$STAMP"
        echo "archived $d -> $d.archived-$STAMP"
    fi
done
[ -n "$ARCHIVED" ] || echo "nothing to archive"

# --------------------------------------------------------------------------
# 4. found the validator.
# --------------------------------------------------------------------------
# Now that this script's own nodes are down, a port still listening belongs to
# something else — another network, or another checkout's scratch pair. That is
# the failure that otherwise surfaces three steps later as a join blaming the
# invite, so it is named here.
for p in "$F_P2P" "$F_HTTP" "$F_GATEWAY" "$F_RPC" "$J_P2P" "$J_HTTP" "$J_GATEWAY" "$J_RPC"; do
    if ss -ltn 2>/dev/null | grep -q ":$p "; then
        die "port $p is listening and is NOT one of this run's nodes. pass
     --port-offset, or find the owner:
       for d in /proc/[0-9]*; do printf '%s %s\\n' \"\$d\" \"\$(readlink \$d/exe)\"; done"
    fi
done

say "found $NAME"
mkdir -p "$ROOT" "$JOINER_ROOT"
DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" node init --name "$NAME" \
    --modules "$STAGE/modules" \
    --listen "127.0.0.1:$F_P2P" --advertised "127.0.0.1:$F_P2P" \
    --http "127.0.0.1:$F_HTTP" --gateway "127.0.0.1:$F_GATEWAY" --rpc "127.0.0.1:$F_RPC" \
    --wireguard-listen "0.0.0.0:$F_WG" --invite-listen "0.0.0.0:$F_INVITE" \
    --primary-coordinator none
CHAIN=$(ls "$ROOT")
[ -n "$CHAIN" ] || die "node init left no workspace under $ROOT"
FOUNDER_WS="$ROOT/$CHAIN"
# Once two workspaces share a chain id, `-n <chain>` is AMBIGUOUS and resolves
# to whichever registration it finds first. Every verb below names its config.
FOUNDER_CFG="$FOUNDER_WS/node.toml"
echo "founded $CHAIN"

# the guest image a run boots is the workspace's own, so it is installed here
# rather than pointed at somewhere shared.
if [ -n "$GUEST_SRC" ]; then
    say "guest image"
    mkdir -p "$FOUNDER_WS/guest"
    cp "$GUEST_SRC/vmlinux" "$GUEST_SRC/rootfs.ext4" "$FOUNDER_WS/guest/"
    echo "installed $(du -sh "$FOUNDER_WS/guest" | cut -f1) of guest image"
fi

# --------------------------------------------------------------------------
# 5. run it under the launcher, so a later core update goes through the
# release plane instead of an operator swapping a file.
# --------------------------------------------------------------------------
say "launch"
# the crate is `node-launcher`; `ducktape-node-launcher` is the binary it
# produces. Naming the binary to `-p` gets "did not match any packages".
LAUNCHER=$(dirname "$BIN_SRC")/ducktape-node-launcher
if [ ! -x "$LAUNCHER" ]; then
    ( cd "$CHECKOUT" && cargo build --release -p node-launcher >&2 )
fi
[ -x "$LAUNCHER" ] || die "no launcher at $LAUNCHER — build \`-p node-launcher\` first"
# The workspace keeps its OWN copy of the founding set, and the launcher's
# child is pointed at it.
#
# The launcher runs `<workspace>/updates/releases/<sha>/ducktape`, and a node
# resolves its set beside its own binary — so under the launcher there is no
# set to find, and the failure is not a genesis error but a REACHABILITY one:
# `netstack_guest_unreadable` kills the reachability plane, so wireguard and
# the invite listener never bind, and a joiner that cannot redeem just dials
# p2p forever and is answered `PeerRejected`. Nothing in that chain names the
# missing modules directory. Pointing the child at the workspace's own copy
# also survives a release flip, which moves the binary to a new directory.
install_set() {
    local ws=$1
    [ -d "$ws/modules" ] || cp -r "$STAGE/modules" "$ws/modules"
}

install_set "$FOUNDER_WS"
"$LAUNCHER" install --workspace "$FOUNDER_WS" --config "$FOUNDER_CFG" --from "$STAGED_BIN"
DUCKTAPE_MODULES_DIR="$FOUNDER_WS/modules" setsid nohup \
    "$LAUNCHER" run --workspace "$FOUNDER_WS" --config "$FOUNDER_CFG" \
    > "$FOUNDER_WS/launcher.log" 2>&1 < /dev/null &
disown

wait_http() {
    local port=$1 what=$2 n=0
    until curl -fsS "http://127.0.0.1:$port/v1/status" >/dev/null 2>&1; do
        n=$((n+1)); [ "$n" -gt 90 ] && die "$what never answered on :$port"
        sleep 2
    done
}
wait_http "$F_HTTP" "the founder"
echo "founder serving on :$F_HTTP"

# --------------------------------------------------------------------------
# 6. join the resident on its own port set.
# --------------------------------------------------------------------------
say "join the resident"
DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" node invite --config "$FOUNDER_CFG" 2>/dev/null \
    | grep -o '🦆[A-Za-z0-9_+/=-]*' > "/tmp/refound-$NAME-$STAMP.invite"
[ -s "/tmp/refound-$NAME-$STAMP.invite" ] || die "node invite printed no invite blob"
DUCKTAPE_HOME="$JOINER_ROOT" "$STAGED_BIN" node join \
    --listen "127.0.0.1:$J_P2P" --advertised "127.0.0.1:$J_P2P" \
    --http "127.0.0.1:$J_HTTP" --gateway "127.0.0.1:$J_GATEWAY" --rpc "127.0.0.1:$J_RPC" \
    --wireguard-listen "0.0.0.0:$J_WG" --invite-listen "0.0.0.0:$J_INVITE" \
    --primary-coordinator none < "/tmp/refound-$NAME-$STAMP.invite"
JOINER_WS="$JOINER_ROOT/$CHAIN"
JOINER_CFG="$JOINER_WS/node.toml"
[ -f "$JOINER_CFG" ] || die "node join left no workspace at $JOINER_WS"

if [ -n "$GUEST_SRC" ]; then
    mkdir -p "$JOINER_WS/guest"
    cp "$GUEST_SRC/vmlinux" "$GUEST_SRC/rootfs.ext4" "$JOINER_WS/guest/"
fi

install_set "$JOINER_WS"
"$LAUNCHER" install --workspace "$JOINER_WS" --config "$JOINER_CFG" --from "$STAGED_BIN"
DUCKTAPE_MODULES_DIR="$JOINER_WS/modules" setsid nohup \
    "$LAUNCHER" run --workspace "$JOINER_WS" --config "$JOINER_CFG" \
    > "$JOINER_WS/launcher.log" 2>&1 < /dev/null &
disown
wait_http "$J_HTTP" "the resident"

# following, not merely answering: a node serves its http surface before it has
# any chain state at all.
n=0
until [ "$(curl -fsS "http://127.0.0.1:$J_HTTP/v1/status" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["height"])')" -gt 0 ]; do
    n=$((n+1)); [ "$n" -gt 90 ] && die "the resident never followed the head"
    sleep 2
done
echo "resident following on :$J_HTTP"

# --------------------------------------------------------------------------
# 7. executors BEFORE the compute plane.
#
# `service enable compute` snapshots the host's capabilities into the grant. If
# the agent CLIs are not installed yet the grant is taken with capabilities=[],
# the node announces no provider tag, and every saga accept is refused
# (`accept_not_capability_provider`) until the service is disabled and enabled
# again. Install first and the ordering problem does not exist.
# --------------------------------------------------------------------------
say "executors"
# naming the CLI is already the answer to the checklist `--yes` would skip, and
# the two are mutually exclusive: `install claude --yes` is refused outright.
DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" agent install claude \
    --node "http://127.0.0.1:$F_HTTP" || die "agent install failed"

# --------------------------------------------------------------------------
# 7b. the workspace's active wallet.
#
# Every keyless verb signs with it, and a service daemon refuses to boot
# without one ("no active wallet in this workspace"). A freshly founded
# workspace has none.
#
# `wallet new` PRINTS A MNEMONIC. It is written to a 0600 file in the
# workspace and never to this script's stdout, which is a log an operator
# pastes around.
# --------------------------------------------------------------------------
say "wallet"
if DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" wallet list --config "$FOUNDER_CFG" 2>/dev/null \
    | grep -q "$WALLET_NAME"; then
    echo "wallet $WALLET_NAME already exists"
else
    SECRETS="$FOUNDER_WS/wallet-$WALLET_NAME.secret"
    ( umask 077; : > "$SECRETS" )
    if printf '%s\n' "$WALLET_PASSWORD" \
        | DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" wallet new "$WALLET_NAME" \
          --config "$FOUNDER_CFG" > "$SECRETS" 2>&1; then
        chmod 600 "$SECRETS"
        echo "minted wallet $WALLET_NAME — mnemonic in $SECRETS (0600), not echoed here"
        DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" wallet use "$WALLET_NAME" --config "$FOUNDER_CFG" \
            || echo "could not set $WALLET_NAME active (continuing)"
    else
        echo "wallet new failed — see $SECRETS"
    fi
fi

# A wallet is a KEY; an account is the on-chain identity that key belongs to,
# and founding one is a submitted, user-signed transaction — so it needs the
# node already serving, which is why this is here and not beside `node init`.
# A daemon does not stop at "no wallet": with a key that is on no account it
# enables, announces, and THEN exits `FATAL: the active wallet key is on no
# account`, which reads like a grant that worked.
# `--node`, not `-n`: the two workspaces share a chain id from here on.
if ! DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" account show \
    --node "http://127.0.0.1:$F_HTTP" > /dev/null 2>&1; then
    printf '%s\n' "$WALLET_PASSWORD" \
        | DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" account create --name "$WALLET_NAME" \
          --node "http://127.0.0.1:$F_HTTP" \
        || die "account create failed — the service daemons will not boot without one"
fi

say "services"
# `service enable` alone consents to a daemon that is ALREADY SIGNALLING — with
# nothing running it refuses, "there is nothing to consent to". `service run
# --enable` is the one verb that starts the daemon and grants it, and it runs in
# the foreground, so each one is backgrounded here the way a unit file would
# supervise it.
#
# This runs AFTER `agent install` on purpose: the grant snapshots the host's
# capabilities, so a compute service enabled before the executors exist is
# granted `capabilities=[]`, announces no provider tag, and refuses every saga
# accept with `accept_not_capability_provider` until it is disabled and enabled
# again.
# compute and agent OPEN THE SANDBOX at boot and exit if the microVM kernel is
# not there, so without a guest image they can only fail. Say that here instead
# of starting two daemons that die and reporting it as a missing grant.
SERVICES="airlock"
if [ -n "$GUEST_SRC" ]; then
    SERVICES="compute agent airlock"
else
    echo "no --guest: starting airlock only (compute and agent need the microVM kernel)"
fi

# ONE AT A TIME, each grant confirmed before the next daemon starts.
#
# Not for the lost-grant race any more: `commit_enable` holds an exclusive lock
# across the whole read-modify-write of `<workspace>/services.toml`, so three
# daemons granting themselves at once all keep their records. Sequence is kept
# for the reason below, which no lock addresses.
#
# The grant line is also NOT proof the daemon lives: it enables, prints
# `announced at height N`, and can still exit on the next line. So each one
# waits for its grant, then waits out the exit window and must still be there.
for svc in $SERVICES; do
    log="$FOUNDER_WS/service-$svc.log"
    DUCKTAPE_MODULES_DIR="$FOUNDER_WS/modules" setsid nohup \
        "$STAGED_BIN" service run "$svc" --config "$FOUNDER_CFG" --enable \
        > "$log" 2>&1 < /dev/null &
    disown
    n=0
    until grep -q "announced at height" "$log" 2>/dev/null; do
        n=$((n+1))
        [ "$n" -le 30 ] || die "service $svc: no grant after 60s — see $log"
        sleep 2
    done
    sleep 2
    if grep -q "boot_fatal" "$log" 2>/dev/null; then
        die "service $svc enabled and then died: $(grep -m1 -o 'FATAL:.*' "$log")"
    fi
    [ -n "$(service_pid "$svc")" ] \
        || die "service $svc: enabled but no daemon is running — see $log"
done

# and the grants as the NODE reads them back, which is the only surface that
# would have caught the clobber above.
for svc in $SERVICES; do
    DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" service status --config "$FOUNDER_CFG" 2>/dev/null \
        | grep -q "✓ $svc  enabled" \
        || die "service $svc is running but the node does not read it as enabled — see $FOUNDER_WS/service-$svc.log"
done
DUCKTAPE_HOME="$ROOT" "$STAGED_BIN" service status --config "$FOUNDER_CFG" 2>&1 | head -20 || true

# --------------------------------------------------------------------------
# 8. mirror a repo into the network's own forge, so a run clones from the
# network rather than from the host's disk.
# --------------------------------------------------------------------------
if [ -n "$MIRROR_REPO" ]; then
    say "forge mirror"
    python3 "$CHECKOUT/ops/forge-import.py" push \
        --node-url "http://127.0.0.1:$F_HTTP" \
        --token-file "$FOUNDER_WS/admin.token" \
        --repo "$(basename "$MIRROR_REPO")" --branch dev --tip HEAD \
        || echo "forge import: failed (continuing)"
fi

# --------------------------------------------------------------------------
# 9. the desktop app checks its contract against /v1/status on every connect,
# so a re-found that moves the contract needs the app rebuilt beside it.
# --------------------------------------------------------------------------
if [ "$SKIP_APP" = 0 ]; then
    say "app"
    ( cd "$CHECKOUT" && cargo build --release -p ducktape-app >&2 ) \
        || echo "app rebuild failed (continuing)"
fi

# --------------------------------------------------------------------------
# 10. what the operator needs.
# --------------------------------------------------------------------------
say "up"
CONTRACT=$(curl -fsS "http://127.0.0.1:$F_HTTP/v1/status" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("contract","?"))')
cat <<REPORT
  network     $CHAIN
  contract    $CONTRACT
  founder     http 127.0.0.1:$F_HTTP   rpc :$F_RPC   config $FOUNDER_CFG
  resident    http 127.0.0.1:$J_HTTP   rpc :$J_RPC   config $JOINER_CFG
  binary      $VOUCH
  set         $MODULES_SRC
  archived   ${ARCHIVED:- (nothing)}

  both nodes are supervised by ducktape-node-launcher, so a core update flips
  through the release plane. Use --config, never -n: two workspaces now share
  this chain id and -n resolves to whichever it finds first.
REPORT
