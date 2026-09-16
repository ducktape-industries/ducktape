#!/usr/bin/env bash
# make dev — the desktop-app dev loop against a fresh "demo" localnet.
#
# On macOS, offers the sandbox prerequisites as a prompt up front; then founds
# the "demo" network anew (ops/demo-seed.sh: it stops and clears whatever a
# previous lap left, then seeds from the founding set THIS build staged and
# builds the workspace's guest images), offers the agent CLIs this network
# lends to runs as a checklist, starts its node, starts the three local agent
# services, initializes its forge with ducktape's own repository
# (ops/dogfood-forge.sh), then runs the desktop app in the foreground.
#
# The demo network is disposable: every lap founds it from the modules, index
# guests and views the build just staged, and installs its guest and agent
# CLIs into the fresh workspace, so nothing from an earlier lap — a genesis
# without today's views, a service bound to a workspace that no longer
# exists, a CLI behind the vendor's latest — survives into the next. Every
# file the network owns lives under its workspace. Ctrl-C quits the app and leaves the node
# and services up for app-only relaunches (`cargo run -p ducktape-app`); the
# next `make dev` replaces them. `make dev-clear` stops that background
# runtime without deleting state; `make demo-clear` removes the workspace
# entirely.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ID="${DEMO_WORKSPACE_ID:-demo}"
DUCK="${DUCKTAPE_HOME:-$HOME/.ducktape}"
WSDIR="$DUCK/$ID"

log(){ printf '\033[36m[dev]\033[0m %s\n' "$*"; }
die(){ printf '\033[31m[dev] %s\033[0m\n' "$*" >&2; exit 1; }

# The node binary, resolved the way demo-seed resolves it. Built FIRST: the
# setup steps below are its own verbs.
NODE_BIN="${DUCKTAPE_NODE_BIN:-}"
if [ -z "$NODE_BIN" ]; then
  log "building ducktape (cargo build -p node-bin)…"
  blog="$(mktemp)"
  cargo build -p node-bin >"$blog" 2>&1 || die "node-bin build failed — see $blog"
  NODE_BIN="$(cargo metadata --no-deps --format-version 1 \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/debug/ducktape"
fi
[ -x "$NODE_BIN" ] || die "node binary not executable: $NODE_BIN"

# macOS: offer the sandbox prerequisites as an install prompt, once, up front —
# instead of the node's boot probe refusing them one at a time later. Declining
# (or an unfixable machine) is not fatal: the dev loop still runs, provider
# runs are what gets refused.
if [ "$(uname -s)" = "Darwin" ]; then
  bash "$SCRIPT_DIR/macos-preflight.sh" --prompt \
    || log "sandbox prerequisites incomplete — the node will refuse provider runs until they are installed"
fi

# Found the network anew. DEV_LISTEN/DEV_ADVERTISED are demo-seed.sh's own
# env-read knobs; this script has no listener config of its own to widen — it
# inherits whichever values are already in the caller's environment.
bash "$SCRIPT_DIR/demo-seed.sh" || die "seeding the '$ID' localnet failed"

# The seed copied the binary and the founding set into the workspace and
# founded from that copy; the rest of this lap runs from it too, so a sibling
# worktree's `cargo build` cannot swap the binary under this live node.
if [ -z "${DUCKTAPE_NODE_BIN:-}" ] && [ -x "$WSDIR/bin/ducktape" ]; then
  NODE_BIN="$WSDIR/bin/ducktape"
  export DUCKTAPE_MODULES_DIR="$WSDIR/bin/modules"
  log "running this network's own copy of the binary: $NODE_BIN"
fi

# What this network's guest lends to runs: the agent CLIs, installed into the
# fresh workspace's executors dir. A checklist, because it is the operator's
# call: each entry is the vendor's latest release, shown with its url and
# expected hash, and checking none is a complete answer. `make dev YES=1`
# takes the whole checklist without asking.
"$NODE_BIN" agent install -n "$ID" ${YES:+--yes} || log "agent CLI setup skipped — runs will refuse the providers that are missing"

# The compute plane's readiness, said now and where the operator is looking:
# `node sandbox` measures the [sandbox] table `node init` just wrote against
# this machine (hypervisor present, guest images built) and names the fix when
# they disagree — otherwise the disagreement is silent until the compute daemon
# dies at boot. --yes because the one write it may offer is enabling the table,
# and a dev node's own compute plane is not a separate decision. Its exit status
# IS the verdict: the two services that can only run inside that plane are
# started below on it, instead of started to die on the same missing table.
if "$NODE_BIN" node sandbox --config "$WSDIR/node.toml" --yes; then
  COMPUTE_PLANE=on
else
  COMPUTE_PLANE=off
  log "this workspace will refuse provider runs — see above"
fi

# The workspace's app endpoint — the same `http_listen` key the app reads.
LISTEN="$(sed -n 's/^[[:space:]]*http_listen[[:space:]]*=[[:space:]]*"\{0,1\}\([^"#]*\)"\{0,1\}.*/\1/p' \
  "$WSDIR/node.toml" | head -1 | tr -d '[:space:]')"
[ -n "$LISTEN" ] || die "no http_listen in $WSDIR/node.toml"
URL="http://$LISTEN"

log "starting node ($URL)…"
"$NODE_BIN" node run --config "$WSDIR/node.toml" >>"$WSDIR/dev-node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 1 80); do
  curl -sf "$URL/v1/status" >/dev/null 2>&1 && break
  kill -0 "$NODE_PID" 2>/dev/null || die "node exited on start — see $WSDIR/dev-node.log"
  sleep 0.25
done
curl -sf "$URL/v1/status" >/dev/null 2>&1 || die "node http never came up — see $WSDIR/dev-node.log"
# The node outlives this script on purpose: Ctrl-C quits the app, and an
# app-only relaunch (`cargo run -p ducktape-app`) wants the node it was just
# talking to. The next `make dev` replaces it; `make dev-clear` and
# `make demo-clear` sweep it by cmdline + /v1/shutdown.
disown "$NODE_PID"
log "node up (pid $NODE_PID, log $WSDIR/dev-node.log)"

# Shell needs all three local planes: compute owns durable runs, agent owns raw
# PTYs, and airlock lends the credential selected in the app. The seed cleared
# the previous lap's set with its workspace, so each is started here. The
# service catalog is the readiness event, so do not guess from stale pidfiles
# or fixed startup time.
service_state(){
  "$NODE_BIN" service list "$1" --workspace "$WSDIR" --json 2>/dev/null |
    sed -n 's/.*"state":"\([^"]*\)".*/\1/p' | head -1
}

start_service(){
  local kind="$1" state pid
  log "starting $kind service…"
  "$NODE_BIN" service run "$kind" --enable --workspace "$WSDIR" >>"$WSDIR/dev-$kind.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 80); do
    state="$(service_state "$kind")"
    [ "$state" = "enabled" ] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.25
  done
  if [ "$state" = "enabled" ]; then
    disown "$pid"
    log "$kind service up (pid $pid, log $WSDIR/dev-$kind.log)"
    return
  fi

  # It never reached `enabled`, and the two ways that happens want different
  # words: a live-but-slow service may still arrive, a dead one never will.
  # Either way the reason is in its log and NOWHERE else — pointing at a path
  # the reader has to go open loses hard failures (a workspace this build
  # refuses to decode, a port already held) behind a line that reads like a
  # timing hiccup, and `make dev` runs on to launch the app regardless.
  if kill -0 "$pid" 2>/dev/null; then
    disown "$pid"
    log "$kind service is still not ready after 20s — Shell may be unavailable:"
  else
    wait "$pid" 2>/dev/null
    log "$kind service exited before it was ready — Shell will be unavailable:"
  fi
  tail -n 3 "$WSDIR/dev-$kind.log" 2>/dev/null | sed 's/^/    /'
  log "full log: $WSDIR/dev-$kind.log"
}

case "$COMPUTE_PLANE" in
  on)
    start_service compute
    start_service agent
    ;;
  off)
    log "no compute plane on this host — the compute and agent services are not started; Shell is unavailable until the sandbox prerequisites above are installed"
    ;;
esac
start_service airlock

# The forge module starts empty, so the fresh demo node hosts no repo at all.
# Seed it with ducktape's own source (the `make dogfood-forge` script, pointed
# at THIS workspace's node). Non-fatal: an offline box cannot `git fetch
# origin`, and the app dev loop should not depend on that.
DUCKTAPE_DEV_FORGE_URL="$URL" bash "$SCRIPT_DIR/dogfood-forge.sh" ||
  log "forge init failed — run \`make dogfood-forge\` when origin is reachable"

log "launching the app — Ctrl-C quits the app; the node and services stay up for \`cargo run -p ducktape-app\`, the next \`make dev\` replaces them"
exec cargo run -p ducktape-app
