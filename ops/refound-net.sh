#!/usr/bin/env bash
# Re-found a ducktape network from nothing: tear down what is running, archive
# its workspaces, found a validator, join a resident, install the executors,
# enable the services, mirror a git repo into the forge, prove a mention still
# reaches an agent, and print what an operator needs to reach it.
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
SKIP_SMOKE=0
KEEP_STAGE=0
WALLET_NAME="operator"
# EMPTY ON PURPOSE — there is no default password. A word committed in this
# file would unlock the release-signing key of every network founded by it.
# Left empty, the wallet step below generates one; --wallet-password overrides.
# Either way it lands in a 0600 file beside the mnemonic, so the release lane
# reads it back the same way whoever chose it.
WALLET_PASSWORD=""
WALLET_PASSWORD_FILE=""
WALLET_MNEMONIC_FILE=""
WALLET_MNEMONIC=""

# The founder's and the resident's port sets. They must not collide with each
# other or with anything else on the host: two workspaces on one box share the
# fixed defaults, and the join then fails with a message that blames the invite
# rather than the address already in use.
#
# These defaults are the set a network founded by this script runs on, so
# proving it on a scratch pair beside a live network needs `--port-offset` —
# otherwise the scratch founder binds the real one's http port and the
# rehearsal takes down the thing it was rehearsing for.
#
# The TCP block is 28800–28831: the tens digit is the surface (http 0, gateway
# 1, rpc 2, p2p 3), the ones digit the node (founder 0, resident 1). It sits
# BELOW 32768, the bottom of Linux's ephemeral range: the kernel hands a port
# above that to any outbound connection as its source port, and a node that
# restarts while one holds its listener's port cannot bind it. WireGuard and
# the invite door are UDP and keep their own set.
PORT_OFFSET=0
F_HTTP=28800 F_GATEWAY=28810 F_RPC=28820 F_P2P=28830 F_WG=46700 F_INVITE=46701
J_HTTP=28801 J_GATEWAY=28811 J_RPC=28821 J_P2P=28831 J_WG=46710 J_INVITE=46711
# The http listen is the one port an operator names from outside: it is what
# `--node` resolves against, what the app dials, and what an existing network
# already serves on. Left empty, it follows the block above and the offset.
F_HTTP_SET="" J_HTTP_SET=""

LAUNCHER_EXE="ducktape-node-launcher"

# Once the archive has run, every failure below leaves the operator with no
# network AND no obvious way back. Say where it went, every time.
die() {
    printf '\nrefound-net: %s\n' "$*" >&2
    if [ -n "${ARCHIVED:-}" ]; then
        printf '\n  the previous network was NOT deleted. it is at:\n' >&2
        # ARCHIVED is a space-separated path list, and the split is the point.
        # shellcheck disable=SC2086
        printf '    %s\n' $ARCHIVED >&2
        printf '  to put it back: stop anything under the roots, then move each\n' >&2
        printf '  archive back to the root it came from.\n' >&2
    fi
    exit 1
}
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
  --port-offset N     add N to every port. the defaults are what a network
                      founded by this script runs on, so a scratch run beside
                      one needs an offset. keep the tcp block (28800–28831)
                      plus N below 32768, where the kernel's ephemeral range
                      starts.
  --founder-http PORT the founder's http listen, outright. every other port
                      still follows --port-offset. use it to found on the port
                      a network already serves, instead of founding on the
                      default and editing node.toml afterwards.
  --resident-http PORT
                      the resident's http listen, outright. same rule.
  --wallet-name NAME  the workspace's active wallet, and the display name of
                      the account founded for it (default: operator). the
                      service daemons refuse to boot without both.
  --wallet-password P its password. NO DEFAULT: left out, one is generated.
                      the mnemonic and the password are each written to their
                      own 0600 file in the workspace, never to stdout.
  --wallet-password-file F
                      the password is F's first line. prefer it to the flag
                      above: an argv word is visible to every process.
  --wallet-mnemonic-file F
                      restore the wallet from the mnemonic line in F (the file
                      `wallet new` wrote) instead of minting one: the
                      network keeps the release key its installs already pin.
                      F is read before anything is torn down, so it may live
                      in the workspace this run archives. needs a password
                      (the restored wallet's), by either flag above.
  --skip-app          do not rebuild the desktop app.
  --no-smoke          do not seed an agent and mention it at the end. the smoke
                      is the only step that crosses the WHOLE chain, and it
                      costs one real agent run; this is how you decline it.
  --keep-stage        keep the /tmp staging directory (the staged binary,
                      founding set and init homes). default: it is removed on
                      every exit, success or failure.
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
        --founder-http) F_HTTP_SET=${2:-}; shift 2;;
        --resident-http) J_HTTP_SET=${2:-}; shift 2;;
        --wallet-name) WALLET_NAME=${2:-}; shift 2;;
        --wallet-password) WALLET_PASSWORD=${2:-}; shift 2;;
        --wallet-password-file) WALLET_PASSWORD_FILE=${2:-}; shift 2;;
        --wallet-mnemonic-file) WALLET_MNEMONIC_FILE=${2:-}; shift 2;;
        --skip-app) SKIP_APP=1; shift;;
        --no-smoke) SKIP_SMOKE=1; shift;;
        --keep-stage) KEEP_STAGE=1; shift;;
        --yes|-y) ASSUME_YES=1; shift;;
        -h|--help) usage;;
        *) die "unknown flag $1 (try --help)";;
    esac
done

[ -n "$ROOT" ] || usage
# an argv word is visible to every process on the host; a file is not.
if [ -n "$WALLET_PASSWORD_FILE" ]; then
    [ -r "$WALLET_PASSWORD_FILE" ] || die "--wallet-password-file: cannot read $WALLET_PASSWORD_FILE"
    WALLET_PASSWORD=$(head -n 1 "$WALLET_PASSWORD_FILE")
fi
if [ -n "$WALLET_MNEMONIC_FILE" ]; then
    [ -r "$WALLET_MNEMONIC_FILE" ] || die "--wallet-mnemonic-file: cannot read $WALLET_MNEMONIC_FILE"
    [ -n "$WALLET_PASSWORD" ] || die "--wallet-mnemonic-file needs --wallet-password"
    # `wallet new` has printed the mnemonic alone and, later, under two lines
    # of prose: the mnemonic is the one line that is only lowercase words and
    # has a mnemonic's word count.
    WALLET_MNEMONIC=$(awk '/^[a-z]+( [a-z]+)*$/ && (NF==12||NF==15||NF==18||NF==21||NF==24) {print; exit}' "$WALLET_MNEMONIC_FILE")
    [ -n "$WALLET_MNEMONIC" ] || die "--wallet-mnemonic-file: no mnemonic line in $WALLET_MNEMONIC_FILE"
fi
case "$ROOT" in /*) :;; *) die "--root must be an absolute path";; esac
# Shell completion appends a slash to a directory, so `--root ~/.ducktape/dognet/`
# is what an operator actually types. Every match below compares "$root" and
# "$root"/* against a path with no trailing slash, so one left here makes the
# teardown find NOTHING, report "nothing running", and then move both roots out
# from under a live network. Strip it before anything uses it.
while [ "$ROOT" != "/" ] && [ "${ROOT%/}" != "$ROOT" ]; do ROOT=${ROOT%/}; done
[ "$ROOT" != "/" ] || die "--root must not be /"
[ -n "$JOINER_ROOT" ] || JOINER_ROOT="${ROOT}-joiner"
while [ "$JOINER_ROOT" != "/" ] && [ "${JOINER_ROOT%/}" != "$JOINER_ROOT" ]; do
    JOINER_ROOT=${JOINER_ROOT%/}
done
[ -n "$NAME" ] || NAME=$(basename "$ROOT")
# Each root IS a workspace, so the ducktape home is the directory holding them.
# Both roots live in it and share a chain id, which makes `-n` ambiguous — every
# verb below names `--config` or `--node`, and `--node` resolves by the http port
# each workspace actually serves, which is unique per root.
HOME_DIR=$(dirname "$ROOT")
CHECKOUT=$(cd "$(dirname "$0")/.." && pwd)
STAMP=$(date +%Y%m%d-%H%M%S)

if [ "$PORT_OFFSET" -ne 0 ]; then
    for v in F_P2P F_HTTP F_GATEWAY F_RPC F_WG F_INVITE \
             J_P2P J_HTTP J_GATEWAY J_RPC J_WG J_INVITE; do
        eval "$v=\$(( \$$v + PORT_OFFSET ))"
    done
fi

# An explicit http port is the final word, not an input to the sum: the offset
# is applied above, and a named port replaces the result.
check_port() {
    case "$2" in
        ''|*[!0-9]*) die "$1: '$2' is not a port number (1024–65535)";;
    esac
    { [ "$2" -ge 1024 ] && [ "$2" -le 65535 ]; } || die "$1: $2 is outside 1024–65535"
}
[ -z "$F_HTTP_SET" ] || { check_port --founder-http "$F_HTTP_SET"; F_HTTP=$F_HTTP_SET; }
[ -z "$J_HTTP_SET" ] || { check_port --resident-http "$J_HTTP_SET"; J_HTTP=$J_HTTP_SET; }

# Both nodes listen on this host, so no two of these tcp ports may be the same
# one. A collision here binds twice and surfaces three steps later as a join
# blaming the invite. WireGuard and the invite door are udp and cannot clash
# with an http listener, so they are not in the set. An offset shifts every
# port equally and never collides; only a named http port can.
TCP_PORTS="$F_HTTP $F_GATEWAY $F_RPC $F_P2P $J_HTTP $J_GATEWAY $J_RPC $J_P2P"
for p in $TCP_PORTS; do
    claims=0
    for q in $TCP_PORTS; do
        if [ "$p" = "$q" ]; then claims=$(( claims + 1 )); fi
    done
    [ "$claims" -eq 1 ] || die "port $p is claimed twice by this run's ports ($TCP_PORTS)"
done

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
# The stage is a binary and a founding set (~120 MB) on a /tmp that may be
# RAM, and nothing runs from it once the workspaces own their copies. It goes
# on EVERY exit — success, `die`, or a `set -e` stop — unless --keep-stage.
drop_stage() {
    if [ "$KEEP_STAGE" = 1 ]; then
        printf 'kept the stage at %s\n' "$STAGE" >&2
        return 0
    fi
    rm -rf -- "$STAGE"
}
trap drop_stage EXIT
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

# Counted with a nullglob array, NOT `ls glob | wc -l`: under `pipefail` a glob
# that matches nothing makes `ls` exit 2, the pipeline inherits it, and `set -e`
# kills the script on the assignment.
shopt -s nullglob
staged_entries=( "$MODULES_SRC"/* )
shopt -u nullglob

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
        # a binary that was replaced or renamed under a running process reads
        # `<path> (deleted)`. Every match below is on the path, so the suffix
        # has to come off first or a rebuild makes the process invisible.
        exe=${exe% (deleted)}
        # the node: its executable lives under the workspace.
        case "$exe" in "$root"/*) printf '%s\n' "${d#/proc/}"; continue;; esac
        # a process started with a RELATIVE selector — `node run --config
        # node.toml`, `service run agent --workspace .` — names nothing
        # absolute anywhere, and its binary can live outside the workspace
        # too. The only thing that places it is its working directory.
        # Narrowed to a ducktape executable so an operator's shell, editor or
        # `tail` sitting in the workspace is not swept up with the network.
        #
        # `ducktape*` and not an exact name: a rotated release directory keeps
        # the previous binary beside the current one as `ducktape.prev`, and a
        # process started before the rotation still points at it.
        base=${exe##*/}
        case "$base" in ducktape*) ;; *) continue;; esac
        cwd=$(readlink "$d/cwd" 2>/dev/null) || cwd=
        case "$cwd" in
            "$root" | "$root"/*) printf '%s\n' "${d#/proc/}"; continue;;
        esac
        # the launcher and the service daemons: both run a binary from
        # somewhere else and NAME this workspace in argv. Matching the absolute
        # root path in argv catches every one of them — and this arm is inside
        # the `ducktape*` gate above for the same reason the cwd arm is: an
        # argv match alone is a `pkill -f` pattern, and a `tail -f
        # <root>/daemon.log` in another pane matches it exactly.
        args=$( { tr '\0' ' ' < "$d/cmdline"; } 2>/dev/null ) || continue
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
        args=$( { tr '\0' ' ' < "$d/cmdline"; } 2>/dev/null ) || continue
        case "$args" in
            *"service run $kind "*"$FOUNDER_CFG"*) printf '%s\n' "${d#/proc/}"; return;;
        esac
    done
}

# ordered so a supervisor is signalled before the child it would restart.
# The launcher is matched anywhere in the path, not as an anchored suffix: a
# replaced binary reads `<path> (deleted)` and an anchored match would miss it,
# which silently turns this into an undifferentiated kill and lets the
# supervisor outlive its child and restart it under a pid nobody is waiting on.
stop_pids() {
    local pids=$1 p
    for p in $pids; do
        case "$(readlink "/proc/$p/exe" 2>/dev/null)" in
            *"/$LAUNCHER_EXE"*) kill "$p" 2>/dev/null || true;;
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
    # A validator answers SIGTERM from inside its consensus loop (graceful
    # checkpoint), so one that is mid-compaction or select-starved — the usual
    # reason anyone re-founds — does not exit inside 20 s. Without an
    # escalation the run dies here with the network already down, nothing
    # archived, and a survivor for the operator to find by hand. SIGKILL is no
    # broader than the SIGTERM these same pids already took.
    if [ -n "$LEFT" ]; then
        echo "  still alive after 20s, escalating: $LEFT"
        for p in $LEFT; do kill -9 "$p" 2>/dev/null || true; done
        sleep 2
        LEFT=$( { pids_under "$ROOT"; pids_under "$JOINER_ROOT"; } | sort -u )
    fi
    [ -z "$LEFT" ] || die "processes still alive under the roots: $LEFT"
    echo "stopped."
else
    echo "nothing running under $ROOT or $JOINER_ROOT"
fi

# --------------------------------------------------------------------------
# 3. archive. MOVED ASIDE, NEVER DELETED — a re-found resets content by
# design, and the only copy of what was there is the one this step keeps.
# --------------------------------------------------------------------------
# NOT `<root>.archived-<stamp>`: a workspace is `<home>/<dir>/network.toml` and
# the home is scanned exactly one level deep, so an archive left beside the
# root is still enumerated — the DEAD network would be offered by the app's
# picker and by `-n`. One level down inside a directory that holds no
# `network.toml` of its own, the scan skips it and the only way to reach it is
# by the path this step prints.
say "archive"
ARCHIVED=""
ARCHIVE_DIR="$(dirname "$ROOT")/archived-networks"
for d in "$ROOT" "$JOINER_ROOT"; do
    if [ -e "$d" ]; then
        [ "$ASSUME_YES" = 1 ] || die "refusing to archive an existing $d without --yes"
        mkdir -p "$ARCHIVE_DIR"
        dest="$ARCHIVE_DIR/$(basename "$d")-$STAMP"
        # `if`, not `[ … ] && die`: a false test makes the compound non-zero,
        # which under `set -e` exits the script on the HEALTHY path.
        if [ -e "$dest" ]; then
            die "archive target $dest already exists — refusing to overwrite"
        fi
        mv "$d" "$dest"
        ARCHIVED="$ARCHIVED $dest"
        echo "archived $d -> $dest"
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

# --root IS the workspace, not a home containing one.
#
# `node init` writes `<home>/<name>#<chain>`, and the ducktape home is scanned
# exactly ONE level deep for `<dir>/network.toml`
# (`workspace_config::list_workspaces_in`). A workspace at
# `<home>/<root>/<name>#<chain>/` is two levels down: invisible to the app's
# network picker and to every `-n` resolution, reachable only by `--config`.
# So it is founded into a staging home and moved to the root. Every path in
# `node.toml` is workspace-relative, so the move costs nothing.
say "found $NAME"
INIT_HOME="$STAGE/home"
mkdir -p "$INIT_HOME" "$(dirname "$ROOT")"
DUCKTAPE_HOME="$INIT_HOME" "$STAGED_BIN" node init --name "$NAME" \
    --modules "$STAGE/modules" \
    --listen "127.0.0.1:$F_P2P" --advertised "127.0.0.1:$F_P2P" \
    --http "127.0.0.1:$F_HTTP" --gateway "127.0.0.1:$F_GATEWAY" --rpc "127.0.0.1:$F_RPC" \
    --wireguard-listen "0.0.0.0:$F_WG" --invite-listen "0.0.0.0:$F_INVITE" \
    --primary-coordinator none
CHAIN=$(ls "$INIT_HOME")
[ -n "$CHAIN" ] || die "node init left no workspace under $INIT_HOME"
mv "$INIT_HOME/$CHAIN" "$ROOT"
FOUNDER_WS="$ROOT"
# Once two workspaces share a chain id, `-n <chain>` is AMBIGUOUS and resolves
# to whichever registration it finds first. Every verb below names its config.
FOUNDER_CFG="$FOUNDER_WS/node.toml"
echo "founded $CHAIN at $FOUNDER_WS"

# --------------------------------------------------------------------------
# 4b. the workspace's active wallet — a KEY, minted locally, before anything
# runs.
#
# Every keyless verb signs with it, and a service daemon refuses to boot
# without one ("no active wallet in this workspace"). A freshly founded
# workspace has none. It is minted HERE, and not beside the account it will
# later be founded on, because the install below needs its public key: that
# key is what the network's node releases are signed with, and a workspace
# that pins none follows no release channel at all.
#
# `wallet new` PRINTS A MNEMONIC. It is written to a 0600 file in the
# workspace and never to this script's stdout, which is a log an operator
# pastes around. The PASSWORD gets the same treatment and for the same
# reason: this key signs the network's node releases, so a password an
# operator did not choose is generated here — never carried in this file,
# where it would unlock every network ever founded by this script.
# --------------------------------------------------------------------------
say "wallet"
SECRETS="$FOUNDER_WS/wallet-$WALLET_NAME.secret"
PASSFILE="$FOUNDER_WS/wallet-$WALLET_NAME.password"
[ -n "$WALLET_PASSWORD" ] || WALLET_PASSWORD=$(head -c 24 /dev/urandom | base64 | tr -d '\n')
( umask 077; printf '%s\n' "$WALLET_PASSWORD" > "$PASSFILE" )
( umask 077; : > "$SECRETS" )
# A network re-founded under the SAME release key keeps every installed app and
# launcher that pinned it: they verify the new network's channel as they did
# the old one's. `--wallet-mnemonic-file` restores that wallet instead of
# minting one; the mnemonic was read before the teardown moved its file.
mint_or_restore_wallet() {
    if [ -n "$WALLET_MNEMONIC" ]; then
        printf '%s\n%s\n' "$WALLET_MNEMONIC" "$WALLET_PASSWORD" \
            | DUCKTAPE_HOME="$HOME_DIR" "$STAGED_BIN" wallet import "$WALLET_NAME" \
              --config "$FOUNDER_CFG" > /dev/null 2>&1 || return 1
        ( umask 077; printf '%s\n' "$WALLET_MNEMONIC" > "$SECRETS" )
        return 0
    fi
    printf '%s\n' "$WALLET_PASSWORD" \
        | DUCKTAPE_HOME="$HOME_DIR" "$STAGED_BIN" wallet new "$WALLET_NAME" \
          --config "$FOUNDER_CFG" > "$SECRETS" 2>&1
}
if mint_or_restore_wallet; then
    chmod 600 "$SECRETS"
    echo "wallet $WALLET_NAME ready — mnemonic in $SECRETS (0600), not echoed here"
    echo "password in $PASSFILE (0600) — feed it to the release lane with"
    echo "  RELEASE_WALLET_PASSWORD=\$(cat $PASSFILE)"
    DUCKTAPE_HOME="$HOME_DIR" "$STAGED_BIN" wallet use "$WALLET_NAME" --config "$FOUNDER_CFG" \
        || echo "could not set $WALLET_NAME active (continuing)"
else
    die "wallet new failed — see $SECRETS"
fi
WALLET_KEY="$FOUNDER_WS/keys/$WALLET_NAME.key"
RELEASE_PUB=$("$STAGED_BIN" user key status --key "$WALLET_KEY" | awk '{print $NF}') \
    || die "could not read the wallet's public key"
case "$RELEASE_PUB" in
    [0-9a-f]*) [ ${#RELEASE_PUB} = 64 ] || die "the wallet's public key is not 64 hex characters: $RELEASE_PUB";;
    *) die "the wallet's public key is not hex: $RELEASE_PUB";;
esac

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
# The staging directory is under /tmp, which this host empties at boot. The
# node itself survives that — the launcher installed its own copy — but
# anything still EXECUTING the staged path does not, so the service daemons
# below run a workspace-owned copy instead and a reboot leaves them
# restartable. Everything after this point uses it.
WS_BIN="$FOUNDER_WS/ducktape"
cp "$STAGED_BIN" "$WS_BIN"
# The supervisor gets a workspace-owned copy for the same reason, plus one of
# its own: `$LAUNCHER` is resolved beside the node binary, which on this host
# is a cargo target directory every checkout writes to. A sibling's rebuild
# replaces that file, the running launcher's `/proc/<pid>/exe` starts reading
# `<path> (deleted)`, and a teardown that matches its name stops recognising
# the one process that must be signalled FIRST. Under the workspace it is
# matched by path like the node itself.
WS_LAUNCHER="$FOUNDER_WS/$LAUNCHER_EXE"
cp "$LAUNCHER" "$WS_LAUNCHER"
# --release-key is what makes the release plane LIVE on this node. Without it
# the launcher supervises and restarts and nothing more: `pinned_keys` refuses
# with `no_release_key`, "this workspace pins no release key, so it follows no
# node channel", and the only way to move the binary is to found again. The key
# is read once per node life, so pinning it at the install — before the first
# `run` — is the one point where it costs no restart.
"$WS_LAUNCHER" install --workspace "$FOUNDER_WS" --config "$FOUNDER_CFG" \
    --from "$STAGED_BIN" --release-key "$RELEASE_PUB"
DUCKTAPE_MODULES_DIR="$FOUNDER_WS/modules" setsid nohup \
    "$WS_LAUNCHER" run --workspace "$FOUNDER_WS" --config "$FOUNDER_CFG" \
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
# Captured, not piped. As a pipeline under `pipefail` a failing `node invite`
# takes the whole script down on the spot — with its stderr sent to /dev/null
# and the `die` below never reached, so the operator gets a bare non-zero exit
# and no reason at all.
# In the founder's own workspace, beside the wallet mnemonic and password it
# is no less sensitive than: a bearer credential on a shared /tmp is readable
# by whoever gets to it first, and nothing ever cleaned the old ones up.
INVITE_FILE="$FOUNDER_WS/invite-$STAMP.invite"
INVITE_OUT=$(DUCKTAPE_HOME="$HOME_DIR" "$STAGED_BIN" node invite --config "$FOUNDER_CFG" 2>&1) \
    || die "node invite failed: $INVITE_OUT"
# An invite is a bearer credential: 0600 from its first byte, and only its
# path is ever printed.
( umask 077; printf '%s\n' "$INVITE_OUT" | grep -o '🦆[A-Za-z0-9_+/=-]*' > "$INVITE_FILE" ) || true
if [ ! -s "$INVITE_FILE" ]; then
    die "node invite printed no invite blob. it said: $INVITE_OUT"
fi
echo "invite (a bearer credential, mode 0600): $INVITE_FILE"
JOIN_HOME="$STAGE/joiner-home"
mkdir -p "$JOIN_HOME" "$(dirname "$JOINER_ROOT")"
DUCKTAPE_HOME="$JOIN_HOME" "$STAGED_BIN" node join \
    --listen "127.0.0.1:$J_P2P" --advertised "127.0.0.1:$J_P2P" \
    --http "127.0.0.1:$J_HTTP" --gateway "127.0.0.1:$J_GATEWAY" --rpc "127.0.0.1:$J_RPC" \
    --wireguard-listen "0.0.0.0:$J_WG" --invite-listen "0.0.0.0:$J_INVITE" \
    --primary-coordinator none < "$INVITE_FILE"
# moved to its own root for the same reason the founder is — see `found`.
[ -d "$JOIN_HOME/$CHAIN" ] || die "node join left no workspace under $JOIN_HOME"
mv "$JOIN_HOME/$CHAIN" "$JOINER_ROOT"
JOINER_WS="$JOINER_ROOT"
JOINER_CFG="$JOINER_WS/node.toml"
[ -f "$JOINER_CFG" ] || die "node join left no workspace at $JOINER_WS"

if [ -n "$GUEST_SRC" ]; then
    mkdir -p "$JOINER_WS/guest"
    cp "$GUEST_SRC/vmlinux" "$GUEST_SRC/rootfs.ext4" "$JOINER_WS/guest/"
fi

install_set "$JOINER_WS"
J_LAUNCHER="$JOINER_WS/$LAUNCHER_EXE"
cp "$WS_BIN" "$JOINER_WS/ducktape"
cp "$LAUNCHER" "$J_LAUNCHER"
# the SAME release key: one flip moves the whole network, and a resident that
# pins nothing would sit on the old binary while the validator moved.
"$J_LAUNCHER" install --workspace "$JOINER_WS" --config "$JOINER_CFG" \
    --from "$WS_BIN" --release-key "$RELEASE_PUB"
DUCKTAPE_MODULES_DIR="$JOINER_WS/modules" setsid nohup \
    "$J_LAUNCHER" run --workspace "$JOINER_WS" --config "$JOINER_CFG" \
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
# `agent install` has no --config: it resolves the workspace out of the home by
# the http base it serves, which is why --node is the selector here and -n
# never is. The founder and the resident share a chain id, so a chain id names
# both; only one of them serves this port.
DUCKTAPE_HOME="$HOME_DIR" "$WS_BIN" agent install claude \
    --node "http://127.0.0.1:$F_HTTP" || die "agent install failed"

say "account"
# A wallet is a KEY; an account is the on-chain identity that key belongs to,
# and founding one is a submitted, user-signed transaction — so it needs the
# node already serving, which is why this is here and not beside `node init`.
# A daemon does not stop at "no wallet": with a key that is on no account it
# enables, announces, and THEN exits `FATAL: the active wallet key is on no
# account`, which reads like a grant that worked.
# `--node`, not `-n`: the two workspaces share a chain id from here on, so a
# chain id names both and only the port tells them apart.
if ! DUCKTAPE_HOME="$HOME_DIR" "$WS_BIN" account show \
    --node "http://127.0.0.1:$F_HTTP" > /dev/null 2>&1; then
    printf '%s\n' "$WALLET_PASSWORD" \
        | DUCKTAPE_HOME="$HOME_DIR" "$WS_BIN" account create --name "$WALLET_NAME" \
          --node "http://127.0.0.1:$F_HTTP" \
        || die "account create failed — the service daemons will not boot without one"
fi

say "release keys"
# The founders pin these keys at install and nobody else can: a member that
# joins by invite learns which key signs a release from what the NETWORK
# COMMITTED, reading the `release_key` lines back through `release status` and
# pinning them on first read. A founding that never commits one leaves every
# later member refusing every designated release of that kind with
# `no_release_key`, on a machine nobody is going to touch. BOTH kinds are
# committed: the node binary and the app are two channels and a member that
# reads only one of them follows only one.
#
# One key signs both — the operator wallet's — because one operator publishes
# both; the network still records the two decisions separately, since the two
# channels are designated separately.
#
# Here and not beside `node init`: these are governance decisions driven
# through a running node and signed by the operator's wallet key, so they need
# both the founder serving and the account above.
#
# Captured, not piped onward, for the reason `node invite` is: the verb's own
# output is the only account of why it refused.
for kind in node app; do
    KEY_SET=$(printf '%s\n' "$WALLET_PASSWORD" \
        | DUCKTAPE_HOME="$HOME_DIR" "$WS_BIN" release key set --kind "$kind" \
          --pubkey "$RELEASE_PUB" --config "$FOUNDER_CFG" 2>&1) \
        || die "release key set --kind $kind failed: $KEY_SET"
    echo "$KEY_SET"
done

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
#
# Each daemon runs under the launcher's `service` role, never as a bare
# `service run`: the role starts `<workspace>/current/ducktape` and restarts
# its child when a release flip moves that link, so a node release carries
# its daemons with it. A bare daemon keeps the binary it started from until
# someone with a shell restarts it, and `service status` shows the skew
# (`build X (this node: Y)`) for as long as that takes. The role appends
# `--config` itself; passing it again is refused as a duplicate.
for svc in $SERVICES; do
    log="$FOUNDER_WS/service-$svc.log"
    DUCKTAPE_MODULES_DIR="$FOUNDER_WS/modules" setsid nohup \
        "$WS_LAUNCHER" service --workspace "$FOUNDER_WS" --config "$FOUNDER_CFG" \
        -- service run "$svc" --enable \
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
    DUCKTAPE_HOME="$HOME_DIR" "$WS_BIN" service status --config "$FOUNDER_CFG" 2>/dev/null \
        | grep -q "✓ $svc  enabled" \
        || die "service $svc is running but the node does not read it as enabled — see $FOUNDER_WS/service-$svc.log"
done
DUCKTAPE_HOME="$HOME_DIR" "$WS_BIN" service status --config "$FOUNDER_CFG" 2>&1 | head -20 || true

# --------------------------------------------------------------------------
# 8. mirror a repo into the network's own forge, so a run clones from the
# network rather than from the host's disk.
# --------------------------------------------------------------------------
if [ -n "$MIRROR_REPO" ]; then
    say "forge mirror"
    # `cd` FIRST: forge-import runs bare `git rev-parse`/`pack-objects` in the
    # invoking shell's working directory and never looks at --repo as a path,
    # so without this the forge is filled from wherever the operator happened
    # to launch the script — under the name of the repo they asked for.
    [ -d "$MIRROR_REPO/.git" ] || die "--mirror $MIRROR_REPO is not a git checkout"
    ( cd "$MIRROR_REPO" && python3 "$CHECKOUT/ops/forge-import.py" push \
        --node-url "http://127.0.0.1:$F_HTTP" \
        --token-file "$FOUNDER_WS/admin.token" \
        --repo "$(basename "$MIRROR_REPO")" --branch dev --tip HEAD ) \
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
# 10. the assertion.
#
# Every step above proves one part: the node serves, the services read back
# enabled, the set is staged, the forge has the repo. A mention crosses all of
# them at once — attribution, model registration, the capability announcement,
# the sandbox, the executor credential and the reply — so it is the only check
# that fails when a re-found quietly breaks the chain rather than a part.
#
# It runs last because it needs everything, and its result is reported rather
# than thrown: by now the network exists, and the operator needs the ports and
# the archive paths whether or not a mention got through. The exit code carries
# the verdict.
# --------------------------------------------------------------------------
# --------------------------------------------------------------------------
# 9b. does this network follow a release channel at all.
#
# A workspace that pins no release key follows none: the launcher supervises
# it forever and the only way to move its binary is to found again. That state
# is silent on disk — the ABSENCE of a file — so it is read back here rather
# than assumed from the flag that was passed.
# --------------------------------------------------------------------------
RELEASE_PINNED="pinned $RELEASE_PUB (both nodes)"
for ws in "$FOUNDER_WS" "$JOINER_WS"; do
    pin=$(cat "$ws/updates/keys/release.pub" 2>/dev/null) || pin=""
    if [ "$pin" != "$RELEASE_PUB" ]; then
        RELEASE_PINNED="NONE on $ws — that node follows no release channel"
    fi
done

# A pin is one node's; the COMMITTED key is the network's, and it is the only
# one a member that joins later can read. So it is read back off the RUNNING
# founder rather than trusted from the verb that passed.
#
# `release status` labels each line by CHANNEL, not by the `--kind` that
# committed it: kind `node` prints `release_key node`, kind `app` prints
# `release_key stable`. That mapping lives here and nowhere else.
committed_release_key() {
    local kind=$1 cfg=$2 line
    case $kind in
        node) line="release_key node";;
        app) line="release_key stable";;
        *) die "no release status line is known for release kind $kind";;
    esac
    DUCKTAPE_HOME="$HOME_DIR" "$WS_BIN" release status --config "$cfg" 2>/dev/null \
        | grep -m1 "^$line" | cut -f2
}

# THE check, and it refuses: a founding that did not commit a release key is a
# founding whose every later member follows no channel of that kind, which is
# silent until a release is designated weeks later. Every founding proves both
# kinds here instead.
check_committed_release_key() {
    local kind=$1 want=$2 got=$3
    if [ "$got" = "$want" ]; then
        return 0
    fi
    printf '\nrefound-net: the founder says the %s release key this network committed is %s, not the %s it pinned.\n' \
        "$kind" "$got" "$want" >&2
    printf '  a member that joins this network pins what the network committed, so\n' >&2
    printf '  it refuses every designated %s release with no_release_key.\n' "$kind" >&2
    return 1
}

NODE_COMMITTED=$(committed_release_key node "$FOUNDER_CFG") || NODE_COMMITTED=""
[ -n "$NODE_COMMITTED" ] \
    || NODE_COMMITTED="(no release_key node line — the founder did not answer release status)"
APP_COMMITTED=$(committed_release_key app "$FOUNDER_CFG") || APP_COMMITTED=""
[ -n "$APP_COMMITTED" ] \
    || APP_COMMITTED="(no release_key stable line — the founder did not answer release status)"

SMOKE="skipped (--no-smoke)"
if [ "$SKIP_SMOKE" = 1 ]; then
    :
elif [ -z "$GUEST_SRC" ]; then
    # compute and agent are not even started without a guest image, so nothing
    # would ever announce the capability and the run would sit pending for
    # hours. That is not a red; it is a network with no executor.
    SMOKE="skipped (no --guest: no agent service to run it)"
else
    say "mention smoke"
    if printf '%s\n' "$WALLET_PASSWORD" | python3 "$CHECKOUT/ops/refound-smoke.py" \
        --node "http://127.0.0.1:$F_HTTP" \
        --workspace "$FOUNDER_WS" \
        --binary "$WS_BIN" \
        --key "$FOUNDER_WS/keys/$WALLET_NAME.key"; then
        SMOKE="green — a mention reached the agent and it replied"
    else
        SMOKE="RED"
    fi
fi

# A resident that is not following is a one-node network wearing two hats, and
# the founding step cannot see it: `node join` returns as soon as the workspace
# is written, long before the first block arrives. This reads LAST, after the
# smoke has put real work through the chain, so the two numbers it prints are
# the ones the operator is about to walk away from.
FOLLOWS="?"
f_h=$(curl -fsS "http://127.0.0.1:$F_HTTP/v1/status" 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("height",-1))' 2>/dev/null) || f_h=-1
j_h=$(curl -fsS "http://127.0.0.1:$J_HTTP/v1/status" 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("height",-1))' 2>/dev/null) || j_h=-1
if [ "$f_h" -lt 0 ] || [ "$j_h" -lt 0 ]; then
    FOLLOWS="UNKNOWN — a node did not answer /v1/status"
else
    gap=$((f_h - j_h))
    # A resident is always a little behind a validator that is still producing;
    # what matters is that it is not STOPPED. Anything past a few seconds of
    # blocks is the wedge, not lag.
    if [ "$gap" -le 50 ] && [ "$gap" -ge -50 ]; then
        FOLLOWS="yes — founder h$f_h, resident h$j_h"
    else
        FOLLOWS="NO — founder h$f_h, resident h$j_h (gap $gap)"
    fi
fi

# --------------------------------------------------------------------------
# 11. what the operator needs.
# --------------------------------------------------------------------------
say "up"
# `|| CONTRACT="?"`: this is the LAST step, after everything worked. A hiccup
# on one status read must not take the script down under `pipefail` and swallow
# the report — the paths below are the only record of where the old network
# went and where the new one is.
CONTRACT=$(curl -fsS "http://127.0.0.1:$F_HTTP/v1/status" 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("contract","?"))' 2>/dev/null) \
    || CONTRACT="?"
cat <<REPORT
  network     $CHAIN
  contract    $CONTRACT
  founder     http 127.0.0.1:$F_HTTP   rpc :$F_RPC   config $FOUNDER_CFG
  resident    http 127.0.0.1:$J_HTTP   rpc :$J_RPC   config $JOINER_CFG
  binary      $VOUCH
  set         $MODULES_SRC
  release key $RELEASE_PINNED
  committed   $NODE_COMMITTED as the node release key
  committed   $APP_COMMITTED as the app release key
  wallet      $WALLET_NAME — mnemonic $SECRETS, password $PASSFILE (both 0600)
  follows     $FOLLOWS
  smoke       $SMOKE

  the old content is not gone, it moved. these two lines are the whole story:
  archived   ${ARCHIVED:- (nothing)}
  now at      $FOUNDER_WS $JOINER_WS

  both nodes are supervised by ducktape-node-launcher, so a core update flips
  through the release plane. Use --config, never -n: two workspaces now share
  this chain id and -n resolves to whichever it finds first.
REPORT

# The network is founded either way — these are the verdicts on whether it
# WORKS. Each one names what is wrong; the exit code carries all of them.
VERDICT=0
case "$FOLLOWS" in
    yes*) :;;
    *)
        printf '\nrefound-net: the resident is not following the founder.\n' >&2
        printf '  %s\n' "$FOLLOWS" >&2
        printf '  a network whose resident cannot follow is a one-node network.\n' >&2
        VERDICT=1
        ;;
esac
case "$RELEASE_PINNED" in
    pinned*) :;;
    *)
        printf '\nrefound-net: %s\n' "$RELEASE_PINNED" >&2
        printf '  the only way to move that node onto a new binary is to found again.\n' >&2
        VERDICT=1
        ;;
esac
check_committed_release_key node "$RELEASE_PUB" "$NODE_COMMITTED" || VERDICT=1
check_committed_release_key app "$RELEASE_PUB" "$APP_COMMITTED" || VERDICT=1
if [ "$SMOKE" = "RED" ]; then
    printf '\nrefound-net: the network is up, but a mention does not reach an agent.\n' >&2
    printf '  the smoke output above says which link of the chain broke.\n' >&2
    VERDICT=1
fi
exit "$VERDICT"
