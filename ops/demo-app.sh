#!/usr/bin/env bash
# make demo-app — serve the "user-hosted" web app behind the demo's
# app.<id>.duck gateway route.
#
# demo-seed publishes a `loopback_http` route named "app"; the gateway proxies
# it to a node-local loopback port recorded in the workspace's
# gateway-routes.json. This maps that route to a port and runs a small live
# server on it — the "user-hosted" half of the demo (a plain process you own,
# vs. the network-hosted static site that lives in consensus).
#
# Foreground: Ctrl-C stops it. The demo node must be running (open the app on
# the "$ID" workspace) for app.<id>.duck to resolve to this server.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ID="${DEMO_WORKSPACE_ID:-demo}"
DUCK="${DUCKTAPE_HOME:-$HOME/.ducktape}"
WSDIR="$DUCK/$ID"

log(){ printf '\033[36m[demo-app]\033[0m %s\n' "$*"; }
die(){ printf '\033[31m[demo-app] %s\033[0m\n' "$*" >&2; exit 1; }

command -v bun >/dev/null || die "bun is required"
[ -d "$WSDIR" ] || die "no '$ID' workspace — run 'make demo-seed' first"

# Resolve the same freshly-built binary demo-seed uses. Route files have one
# canonical writer; duplicating its JSON shape here is how the retired
# top-level `version` field survived the schema purge.
NODE_BIN="${DUCKTAPE_NODE_BIN:-}"
if [ -z "$NODE_BIN" ]; then
  log "building ducktape (cargo build -p node-bin)…"
  BUILD_LOG="$(mktemp)"
  if ! cargo build --manifest-path "$SCRIPT_DIR/../Cargo.toml" -p node-bin >"$BUILD_LOG" 2>&1; then
    die "node-bin build failed — see $BUILD_LOG"
  fi
  rm -f "$BUILD_LOG"
  NODE_BIN="$(cargo metadata --manifest-path "$SCRIPT_DIR/../Cargo.toml" \
    --no-deps --format-version 1 \
    | bun -e 'console.log((await Bun.stdin.json()).target_directory)')/debug/ducktape"
fi
[ -x "$NODE_BIN" ] || die "node binary not executable: $NODE_BIN"

# Reuse the port already mapped for route "app", else allocate a fresh one.
# `gateway list` strictly reads the latest schema; `gateway bind` preserves the
# airlock and every other local route, validates the result, and writes it
# atomically.
ROUTES="$("$NODE_BIN" gateway list --workspace "$WSDIR")" \
  || die "could not read local gateway routes — re-bind them with '$NODE_BIN gateway bind'"
PORT="$(bun - "$ROUTES" <<'JS'
const routes = JSON.parse(process.argv[2]);
let port = Number(routes.find((route) => route.name?.label === "app")?.port);
if (!Number.isInteger(port) || port < 1 || port > 65535) {
  const listener = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  port = listener.port;
  listener.stop();
}
console.log(port);
JS
)"
"$NODE_BIN" gateway bind --workspace "$WSDIR" --label app --port "$PORT" --trusted-loopback >/dev/null \
  || die "could not bind the app gateway route"

log "route app.$ID.duck -> 127.0.0.1:$PORT"
log "KEEP THIS RUNNING: app.$ID.duck is Unavailable whenever this process is not up."
log "open the app on the '$ID' workspace (its node must be running), then browse to app.$ID.duck  (Ctrl-C here stops it)"

# a small LIVE server — the timestamp ticks on reload, proving it's a running
# process, not static consensus bytes.
exec bun - "$PORT" "$ID" <<'JS'
const [port, id] = process.argv.slice(2);
const page = (now) => `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>User-hosted app</title><style>
body{font:16px/1.6 system-ui,sans-serif;margin:0;min-height:100vh;display:grid;
place-items:center;background:#0e1116;color:#e6edf3}main{max-width:34rem;padding:2rem}
h1{margin:0 0 1rem;font-size:1.6rem}code{background:#1b2029;padding:.1em .35em;border-radius:4px}
</style></head><body><main>
<h1>🖥️ User-hosted web app</h1>
<p>This page is served <strong>live by a process on your machine</strong> and reached
through the gateway's <code>loopback_http</code> route <code>app.${id}.duck</code> —
contrast <code>site.${id}.duck</code>, which is static bytes in consensus.</p>
<p>Server time: <strong>${now}</strong> — reload and it ticks, proof it's a live process.</p>
</main></body></html>`;
Bun.serve({
  hostname: "127.0.0.1",
  port: Number(port),
  fetch(request) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("method not allowed", { status: 405 });
    }
    const body = page(`${new Date().toISOString().slice(11, 19)} UTC`);
    return new Response(request.method === "HEAD" ? null : body, {
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  },
});
console.log(`serving on 127.0.0.1:${port}`);
JS
