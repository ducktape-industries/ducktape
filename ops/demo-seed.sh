#!/usr/bin/env bash
# make demo-seed — a self-contained "demo" network preloaded with sample data.
#
# Inits a solo (1-validator) workspace named "demo" under the ducktape home —
# the SAME directory listing the desktop app reads — builds its guest images,
# starts its node briefly, POSTs a batch of seed ops
# over the node's /v1/submit lane (each finalized into DURABLE qmdb state),
# then stops the node. Open the app and pick the "demo" network: the app
# respawns the node from the same durable dir, fully populated.
#
# Re-runnable: stops whatever still serves the "demo" workspace, wipes it and
# recreates it each time (ops/demo-clear.sh; other workspaces under the home
# are untouched). Ports are freshly allocated.
#
# It also publishes two gateway web-app routes (see ops/demo-gateway.mjs): a
# NETWORK-hosted static site served from DuckFS, and a USER-hosted route that
# proxies to a node-local server. The frameless /v1/submit lane stamps the
# node's own validator key as the op origin. The demo wallet signs the model
# provisioning and configuration, so the app's wallet controls its runs as
# well as its gateway routes.
#
# The model user (ChiefDuck) is the network's resident maintainer: a real
# agent on the `claude` capability, acting as its program account (which may
# submit whatever a member may, to any module), with its persona curated as
# an always-loaded skill (ops/chiefduck/SKILL.md). The two seeded @mentions (one
# in #general, one on a playground issue) run once `make dev` has installed
# the claude CLI into the workspace and the compute service announces it:
# a chat reply, and a pull request opened from a microVM.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# no wasm is embedded in the binary: founding composes the workspace genesis
# out of the founding set the build staged beside the binary (`<target>/
# <profile>/modules`), which `node init` finds by itself — nothing to point at.
ID="${DEMO_WORKSPACE_ID:-demo}"
# The SAME home the CLI and the app resolve: `$DUCKTAPE_HOME` when set, else
# `~/.ducktape`. The home holds workspaces and nothing else; everything the
# demo network owns — its config, its keystore, its guest images, its
# executors, its capability specs — lives under $WSDIR and goes with it.
DUCK="${DUCKTAPE_HOME:-$HOME/.ducktape}"
WSDIR="$DUCK/$ID"
USERKEY="$WSDIR/keys/demo.key"    # the app signs writes with THIS local key
DEMO_PASSWORD="${DEMO_KEY_PASSWORD:-ducktape}"  # unlock password for the demo identity

# DEV_LISTEN widens the p2p mesh + HTTP API binds so a second machine can
# reach this node: default stays 127.0.0.1 (a localhost-only dev loop);
# DEV_LISTEN=0.0.0.0 (or [::]) binds wide. The WireGuard plane is bound wide
# regardless (see --wireguard-listen below).
DEV_LISTEN="${DEV_LISTEN:-127.0.0.1}"
# advertised is the dial-hint peers actually use, so widening `listen` alone
# leaves a second machine learning a loopback address it cannot dial (#1240
# half of #1241) — reaching this node from box B needs
#   DEV_LISTEN=0.0.0.0 DEV_ADVERTISED=<this box's LAN ip>
DEV_ADVERTISED="${DEV_ADVERTISED:-127.0.0.1}"

log(){ printf '\033[36m[demo-seed]\033[0m %s\n' "$*"; }
die(){ printf '\033[31m[demo-seed] %s\033[0m\n' "$*" >&2; exit 1; }

command -v bun     >/dev/null || die "bun is required: curl -fsSL https://bun.sh/install | bash"
command -v curl    >/dev/null || die "curl is required (apt install curl / brew install curl)"

# ── 1. node binary ─────────────────────────────────────────────
NODE_BIN="${DUCKTAPE_NODE_BIN:-}"
if [ -z "$NODE_BIN" ]; then
  log "building ducktape (cargo build -p node-bin)…"
  blog="$(mktemp)"
  cargo build -p node-bin >"$blog" 2>&1 || die "node-bin build failed — see $blog"
  NODE_BIN="$(cargo metadata --no-deps --format-version 1 \
    | bun -e 'console.log((await Bun.stdin.json()).target_directory)')/debug/ducktape"
fi
[ -x "$NODE_BIN" ] || die "node binary not executable: $NODE_BIN"

# ── 2. fresh demo workspace ────────────────────────────────────
# demo-clear stops the node and the service daemons a previous run left
# serving this workspace BEFORE anything under it is deleted — a daemon whose
# directory was pulled out from under it is not a fresh start — and refuses to
# delete while one is still alive.
bash "$SCRIPT_DIR/demo-clear.sh" || die "could not clear the previous '$ID' workspace"
log "creating a fresh '$ID' workspace at $WSDIR"
mkdir -p "$WSDIR"
# Free-port probe only — always loopback regardless of DEV_LISTEN, since it
# never binds anything the node itself serves from.
read -r P1 P2 P3 < <(bun -e 'const l=Array.from({length:3},()=>Bun.listen({hostname:"127.0.0.1",port:0,socket:{data(){}}}));process.stdout.write(l.map(x=>x.port).join(" ")+"\n");l.forEach(x=>x.stop())')
# A free UDP port for the overlay's WireGuard socket. This MUST be concrete: the
# reachability plane refuses to start on port 0 ("wireguard_listen needs a
# concrete UDP port — plane not started"), which leaves the overlay down.
WGP="$(bun -e 'const s=await Bun.udpSocket({port:0});process.stdout.write(String(s.port));s.close()')"
[[ "$WGP" =~ ^[0-9]+$ ]] || die "UDP port allocation produced non-numeric output"
# Gateway serving needs the app's workspace_create posture:
#   --listen             p2p mesh bind: DEV_LISTEN (#1241) — a second machine
#                        needs this wide to dial in for a peer join or huddle
#   --advertised         the dial-hint address WE announce to peers, not a
#     $DEV_ADVERTISED    bind — DEV_ADVERTISED (default 127.0.0.1), a
#                        SEPARATE knob from DEV_LISTEN: a wide `listen` with
#                        a loopback `advertised` still hands box B a dial
#                        hint it cannot use, so reaching this node from a
#                        second machine needs BOTH DEV_LISTEN=0.0.0.0 and
#                        DEV_ADVERTISED=<this box's LAN ip>
#   --http               the HTTP app API, widened with the mesh: reads are
#     $DEV_LISTEN        open to any peer and writes are signed, so a second
#                        machine's app can point straight at this node
#   --rpc                127.0.0.1     local operator rpc — the CLI on THIS
#                        box only, no peer or huddle plane reads it, stays
#                        loopback always
#   --gateway             binds the isolated browser plane that serves the routes;
#     127.0.0.1:0         the node refuses any bind but 127.0.0.1 for it
#   --wireguard-listen   a CONCRETE UDP port, already bound wide (0.0.0.0 =
#     0.0.0.0:$WGP       endpoint-less/roaming, like the app) regardless of
#                        DEV_LISTEN, so the overlay a peer join/huddle needs
#                        already reaches a second machine
#   --primary-coordinator a self-contained local demo does NOT phone home to the
#     none               public rendezvous coordinator; keeps network.toml (which
#                        the app reboots from) fully local.
INIT_ERR="$(mktemp)"
if ! CHAIN="$("$NODE_BIN" node init --name "$ID" --dir "$WSDIR" \
  --listen "$DEV_LISTEN:$P1" --advertised "$DEV_ADVERTISED:$P1" \
  --http "$DEV_LISTEN:$P2" --rpc "127.0.0.1:$P3" --gateway 127.0.0.1:0 \
  --primary-coordinator none \
  --wireguard-listen "0.0.0.0:$WGP" 2>"$INIT_ERR" | tail -1)"; then
  sed -n '1,120p' "$INIT_ERR" >&2
  rm -f "$INIT_ERR"
  die "node init failed"
fi
rm -f "$INIT_ERR"
[ -n "$CHAIN" ] || die "init produced no chain-id"
log "founded '$ID' (chain $CHAIN) at $WSDIR"

# ── 3. user identity ───────────────────────────────────────────
# The app signs writes with a wallet from the WORKSPACE's keystore: a wallet
# is an identity on one network. The demo gets its OWN named wallet ("demo",
# password $DEMO_PASSWORD), minted into the fresh workspace, so the seed
# always holds the signing password. Nothing outside this workspace is
# touched — the app's wallet list for the demo network is where this
# identity gets picked.
printf '%s\n' "$DEMO_PASSWORD" | "$NODE_BIN" wallet new demo --workspace "$WSDIR" >/dev/null \
  || die "could not mint the demo wallet"
log "minted the demo wallet (password: $DEMO_PASSWORD)"

# ── 3b. the guest images ──────────────────────────────────────
# Every run of this network boots the workspace's OWN guest
# (`<workspace>/guest`) and execs out of the workspace's OWN executors dir:
# two networks on one box share no image, and a lap rebuilds the image into
# the fresh workspace. The base rootfs and kernel downloads are cached under
# the repo's target dir, so a lap rebuilds the image, not the download. A box
# that cannot build one (no unsquashfs/mke2fs) still seeds; ChiefDuck's runs
# stay pending. The executors dir is filled by `ducktape agent install`
# (`make dev` offers it), and the capability dir stays empty: the claude spec
# is the binary's built-in default.
GUEST_DIR="$WSDIR/guest"
mkdir -p "$WSDIR/executors" "$WSDIR/capabilities" || die "cannot create the workspace's compute dirs"
if OUT="$GUEST_DIR" bash "$SCRIPT_DIR/build-guest-rootfs.sh" >"$WSDIR/guest-build.log" 2>&1; then
  log "built the guest images into $GUEST_DIR"
else
  log "guest image build failed — see $WSDIR/guest-build.log; ChiefDuck's runs stay pending on this box"
fi

# ── 4. start the node, wait for its published identity ─────────
log "starting node (http $DEV_LISTEN:$P2)…"
"$NODE_BIN" node run --config "$WSDIR/node.toml" >"$WSDIR/seed.log" 2>&1 &
NODE_PID=$!
trap 'kill "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null' EXIT
# This script always talks to the node it just started, from THIS box — a
# wildcard bind still accepts loopback, so the seeder's own curl calls stay
# on 127.0.0.1 regardless of DEV_LISTEN.
URL="http://127.0.0.1:$P2"
# The node binds its HTTP listener BEFORE it publishes its mesh identity, and
# `/v1/status` serves an empty `public_key` in between. The compute daemon
# below names its providers by that key and refuses an empty one, so the
# node is up when the key is there, not when the route answers.
identity_published(){
  curl -sf "$URL/v1/status" 2>/dev/null | grep -Eq '"public_key" *: *"[0-9a-fA-F]+"'
}
for _ in $(seq 1 80); do
  identity_published && break
  kill -0 "$NODE_PID" 2>/dev/null || die "node exited on start — see $WSDIR/seed.log"
  sleep 0.5
done
identity_published || die "node never published its identity — see $WSDIR/seed.log"

# ── 4b. grant the compute service ──────────────────────────────
# The compute plane is consent-gated: a [sandbox] table says HOW a run would
# be isolated, and the user's grant says WHETHER this node runs any. There is
# no init flag for it any more, so the demo mints the grant the same way an
# operator does — `service run` discovers this host's providers, signals them,
# and --enable grants from that live hello. It only needs to run long enough
# for the grant to land, so it is stopped once services.toml appears.
grant_compute(){
  "$NODE_BIN" service run compute --enable --workspace "$WSDIR" >"$WSDIR/service.log" 2>&1 &
  local svc_pid=$!
  for _ in $(seq 1 50); do [ -f "$WSDIR/services.toml" ] && break; sleep 0.1; done
  kill "$svc_pid" 2>/dev/null; wait "$svc_pid" 2>/dev/null
  if [ -f "$WSDIR/services.toml" ]; then
    log "compute granted — agent runs available"
    return
  fi
  # The reason is in service.log and nowhere else; guessing at it ("no usable
  # container runtime?") sends the reader hunting for the wrong thing.
  log "compute NOT granted:"
  tail -n 3 "$WSDIR/service.log" 2>/dev/null | sed 's/^/    /'
  log "  full log: $WSDIR/service.log"
  log "  the demo still runs, just without agent runs. Grant it later with:"
  log "  ducktape service run compute --workspace $WSDIR"
}

# A node.toml with no live [sandbox] table is a host that cannot isolate a run
# (`node init` writes the table only where it can): the compute daemon dies at
# boot on it, so there is no grant to mint and no log worth reading.
has_sandbox_table(){ grep -q '^\[sandbox\]' "$WSDIR/node.toml"; }
if has_sandbox_table; then
  grant_compute
else
  log "no [sandbox] table in node.toml — this host cannot isolate a run, so compute is not granted; the demo still runs, just without agent runs"
fi

# ── 5. seed ops ────────────────────────────────────────────────
# Every mutating /v1 route wants a credential: either a per-request user
# signature or this node's own operator credential, minted 0600 into the
# workspace at boot. The seeder acts as the node's operator, so it presents
# that one — a bare curl is no longer a way in.
OPERATOR="$(cat "$WSDIR/admin.token" 2>/dev/null)" \
  || die "the node minted no operator credential at $WSDIR/admin.token"
[ -n "$OPERATOR" ] || die "the node minted no operator credential at $WSDIR/admin.token"
N=0
submit(){ # submit <module> <payload-json>
  N=$((N+1))
  local body resp code
  body=$(bun -e 'const [target,payload]=process.argv.slice(1);console.log(JSON.stringify({target,payload:JSON.parse(payload)}))' "$1" "$2") \
    || die "op #$N ($1): payload is not valid json"
  resp=$(curl -s -w $'\n%{http_code}' "$URL/v1/submit" -H 'content-type: application/json' \
    -H "x-ducktape-admin-token: $OPERATOR" -d "$body")
  code=${resp##*$'\n'}
  [ "$code" = "200" ] || die "op #$N ($1) rejected [$code]: ${resp%$'\n'*}"
}

# Model ownership must belong to the wallet the desktop unlocks, not the
# validator identity attached to an operator-token request.
submit_user(){ # submit_user <module> <payload-json>
  N=$((N+1))
  local request frame resp code
  request=$(bun -e 'const [target,payload]=process.argv.slice(1);const bytes=Buffer.from(JSON.stringify(JSON.parse(payload)));process.stdout.write(`${target} ${BigInt(Date.now())*1000000n} ${bytes.toString("hex")}`)' "$1" "$2") \
    || die "op #$N ($1): payload is not valid json"
  frame=$(printf '%s\n%s\n' "$DEMO_PASSWORD" "$request" | "$NODE_BIN" user sign-frame --key "$USERKEY") \
    || die "op #$N ($1): demo wallet signing failed"
  resp=$(printf '%s' "$frame" | bun -e 'process.stdout.write(Buffer.from(await Bun.stdin.text(),"hex"))' | \
    curl -s -w $'\n%{http_code}' "$URL/v1/submit/frame" -H 'content-type: application/octet-stream' --data-binary @-) \
    || die "op #$N ($1): submit failed"
  code=${resp##*$'\n'}
  [ "$code" = "200" ] || die "op #$N ($1) rejected [$code]: ${resp%$'\n'*}"
}

log "seeding modules…"

# pages — the Pages surface: a welcome page with a few blocks
submit pages '{"create_page":{"page_id":"welcome","title":"Welcome to Ducktape"}}'
submit pages '{"insert_block":{"parent":"welcome","after":null,"block":{"id":"w-h","kind":"heading2","text":"This is a demo network"}}}'
submit pages '{"insert_block":{"parent":"welcome","after":"w-h","block":{"id":"w-p","kind":"paragraph","text":"Everything here was preloaded by make demo-seed. Poke around — chat, tasks, pages, agents."}}}'
submit pages '{"insert_block":{"parent":"welcome","after":"w-p","block":{"id":"w-t1","kind":"todo","text":"Open the general channel"}}}'
submit pages '{"insert_block":{"parent":"welcome","after":"w-t1","block":{"id":"w-t2","kind":"todo","text":"Check the tasks board"}}}'
submit pages '{"create_page":{"page_id":"runbook","title":"Team Runbook"}}'
submit pages '{"insert_block":{"parent":"runbook","after":null,"block":{"id":"rb-p","kind":"paragraph","text":"How we ship: branch off dev, PR, review, merge."}}}'

# chat — channels + messages + a reaction + an agent mention
submit chat '{"create_channel":{"channel_id":"general","name":"General","post_policy":"open"}}'
submit chat '{"create_channel":{"channel_id":"engineering","name":"Engineering","post_policy":"open"}}'
submit chat '{"create_channel":{"channel_id":"product","name":"Product","post_policy":"open"}}'
submit chat '{"post_message":{"channel_id":"general","message_id":"g1","blocks":[{"paragraph":[{"text":"Welcome to the demo network 👋","marks":[]}]}],"thread":null}}'
submit chat '{"post_message":{"channel_id":"general","message_id":"g2","blocks":[{"paragraph":[{"text":"This whole workspace is seeded — messages, tasks, pages and an agent.","marks":[]}]}],"thread":null}}'
submit chat '{"post_message":{"channel_id":"general","message_id":"g3","blocks":[{"paragraph":[{"text":"Nice, it even threads.","marks":[]}]}],"thread":1}}'
submit chat '{"add_reaction":{"channel_id":"general","seq":1,"emoji":"🦆"}}'
submit chat '{"post_message":{"channel_id":"engineering","message_id":"e1","blocks":[{"paragraph":[{"text":"CI is green on dev.","marks":[]}]}],"thread":null}}'
submit chat '{"post_message":{"channel_id":"product","message_id":"p1","blocks":[{"paragraph":[{"text":"Demo script for the deck is ready.","marks":[]}]}],"thread":null}}'

# tasks — a small board with mixed statuses. both boards ride ONE wire envelope
# (WorkMsg): task-board ops are wrapped `{"task":{…}}`, job-board ops `{"job":{…}}`.
submit tasks '{"task":{"create_task":{"task_id":"t1","title":"Draft the launch announcement"}}}'
submit tasks '{"task":{"create_task":{"task_id":"t2","title":"Review the onboarding flow"}}}'
submit tasks '{"task":{"create_task":{"task_id":"t3","title":"Fix flaky identity test"}}}'
submit tasks '{"task":{"update_status":{"task_id":"t2","status":"in_progress"}}}'
submit tasks '{"task":{"update_status":{"task_id":"t3","status":"done"}}}'

# model user — the demo wallet controls a keyless programmable account.
# The recipe is emitted by the current binary, never copied into this script.
# Its capability is `claude`: the run executes on a node whose compute
# service announces that tag, which `make dev` arranges by installing the
# claude CLI into this workspace's executors dir. Until then the @mention
# below is a pending run, not a lost one.
query(){ # query <module> <query-json>
  local body
  body=$(bun -e 'const [target,query]=process.argv.slice(1);process.stdout.write(JSON.stringify({target,query:JSON.parse(query)}))' "$1" "$2") || die "invalid query"
  curl -fsS "$URL/v1/query" -H 'content-type: application/json' -d "$body" || die "query failed"
}
# Operator-authored seed content (including automation rules) needs the
# node's account. Model control uses the separate desktop wallet below.
submit identity '{"create":{"name":"Demo operator","scheme":"ed25519"}}'
USER_PUB=$("$NODE_BIN" user key status --key "$USERKEY" | awk '{print $NF}') || die "cannot read the demo public key"
USER_BYTES=$(bun -e 'process.stdout.write(JSON.stringify([...Buffer.from(process.argv[1],"hex")]))' "$USER_PUB")
CONTROLLER=$(query identity "{\"of_key\":{\"key\":$USER_BYTES}}" | bun -e 'process.stdout.write(String((await Bun.stdin.json()).account?.number ?? ""))')
if [ -z "$CONTROLLER" ]; then
  submit_user identity '{"create":{"name":"demo","scheme":"ed25519"}}'
  CONTROLLER=$(query identity "{\"of_key\":{\"key\":$USER_BYTES}}" | bun -e 'process.stdout.write(String((await Bun.stdin.json()).account?.number ?? ""))')
fi
[ -n "$CONTROLLER" ] || die "the demo wallet has no controller account"
AGENT_ID="chiefduck"
AGENT_NAME="ChiefDuck"
# The persona: an always-loaded skill in the shared library, which the host
# assembles into the context document the CLI auto-loads for every run.
curl -fsS -X PUT "$URL/v1/files/object/shared/skills/$AGENT_ID/SKILL.md" \
  -H "x-ducktape-admin-token: $OPERATOR" \
  --data-binary @"$SCRIPT_DIR/chiefduck/SKILL.md" >/dev/null \
  || die "cannot stage the $AGENT_NAME persona skill"
PROGRAM=$("$NODE_BIN" agent model-program "$AGENT_ID") || die "cannot encode the default model program"
PROVISION=$(printf '%s' "$PROGRAM" | bun -e 'process.stdout.write(JSON.stringify({provision:{name:process.argv[1],program:await Bun.stdin.json()}}))' "$AGENT_NAME") || die "invalid program"
submit_user agent "$PROVISION"
MODEL_ACCOUNT=$(query identity "{\"controlled\":{\"by\":$CONTROLLER,\"from\":0,\"limit\":256}}" | bun -e '
  const matches=(await Bun.stdin.json()).accounts.filter(account=>account.name===process.argv[1] && account.control.program?.executor==="agent");
  if(matches.length!==1) throw new Error(`expected exactly one ${process.argv[1]} program account`);
  process.stdout.write(String(matches[0].number));
' "$AGENT_NAME") || die "cannot resolve the model account"
# The record is the program account, the capability and the persona skill:
# the agent acts as that account, which may submit whatever a member may to
# any module — every repository (the playground seeded below, the dogfood
# mirror `make dev` pushes — `ops/dogfood-forge.sh`, repo `ducktape` — and
# any registered after), every page, and the whole of duckfs.
PLAYGROUND="playground"
REGISTER=$(bun -e 'process.stdout.write(JSON.stringify({configure_model:{operation:{register_model:{
  account:Number(process.argv[1]),agent_id:process.argv[2],display_name:process.argv[3],capability:"claude",
  skills:[{name:process.argv[2],source_prefix:`/shared/skills/${process.argv[2]}`,load:"always"}]
}}}}))' "$MODEL_ACCOUNT" "$AGENT_ID" "$AGENT_NAME") || die "invalid registration"
submit_user runs "$REGISTER"
# A mention span holds ONLY the `@name` token — the chat view draws a
# mention-marked span as the account's current name and nothing else, so
# the rest of the sentence rides in its own plain span (the composer's shape).
MENTION=$(bun -e 'process.stdout.write(JSON.stringify({post_message:{channel_id:"general",message_id:"g4",blocks:[{paragraph:[{text:`@${process.argv[2]}`,marks:[{mention:{account:Number(process.argv[1])}}]},{text:" introduce yourself: what can you do on this network?",marks:[]}]}],thread:null}}))' "$MODEL_ACCOUNT" "$AGENT_ID")
submit chat "$MENTION"

# forge — a playground repo, an issue on it, and a ChiefDuck mention in the
# issue's discussion channel: the trigger the dogfood e2e drives. A push must
# prove itself, so this one carries the operator credential (the node becomes
# the repo's owner) through GIT_CONFIG_*, never an argv. The run itself waits
# for `make dev`: the compute service clones the repo into a microVM, the
# agent works the issue there, the host commits and pushes agent/item-1, and
# the PR sink opens the pull request onto dev.
if command -v git >/dev/null; then
  SEED_REPO="$(mktemp -d)"
  ( cd "$SEED_REPO" \
    && git -c init.defaultBranch=dev init -q \
    && printf '# %s\n\nA scratch repository the demo seeds for %s. Mention @%s on an issue here and it opens a pull request.\n' "$PLAYGROUND" "$AGENT_NAME" "$AGENT_ID" > README.md \
    && git add README.md \
    && git -c user.name="Demo seed" -c user.email="seed@demo.duck" -c commit.gpgsign=false commit -q -m "seed the playground" \
    && GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader GIT_CONFIG_VALUE_0="x-ducktape-admin-token: $OPERATOR" \
       git push -q "$URL/forge/$PLAYGROUND" HEAD:dev )
  pushed=$?
  rm -rf "$SEED_REPO"
  [ "$pushed" -eq 0 ] || die "cannot push the $PLAYGROUND repo into the forge"
  submit forge "{\"open_issue\":{\"repo\":\"$PLAYGROUND\",\"title\":\"Say hello from a microVM\",\"body\":\"Mention @$AGENT_ID here: it clones this repo inside a microVM, adds a HELLO.md that says who it is, and opens a pull request.\"}}"
  ISSUE_CHANNEL=$(query forge "{\"get_item\":{\"repo\":\"$PLAYGROUND\",\"number\":1}}" | bun -e 'process.stdout.write(String((await Bun.stdin.json()).item?.channel_id ?? ""))')
  [ -n "$ISSUE_CHANNEL" ] || die "the $PLAYGROUND issue has no discussion channel"
  ISSUE_MENTION=$(bun -e 'process.stdout.write(JSON.stringify({post_message:{channel_id:process.argv[2],message_id:"i1",blocks:[{paragraph:[{text:`@${process.argv[3]}`,marks:[{mention:{account:Number(process.argv[1])}}]},{text:" say hello",marks:[]}]}],thread:null}}))' "$MODEL_ACCOUNT" "$ISSUE_CHANNEL" "$AGENT_ID")
  submit chat "$ISSUE_MENTION"
else
  log "no host git — skipping the $PLAYGROUND forge repo and its $AGENT_NAME issue"
fi

# jobs — a job on the board. the job board shares the "tasks" target under the
# WorkMsg `{"job":{…}}` arm (there is no separate "jobs" module).
submit tasks '{"job":{"submit":{"job_id":"j1","kind":"demo","spec":"render the welcome deck"}}}'

# automations — a rule that files a task whenever someone says "deploy"
submit automations '{"create_rule":{"rule_id":"deploy-watch","trigger":{"channel_id":null,"mention":null,"text_contains":"deploy"},"action":{"create_task":{"task_id_prefix":"deploy","title_template":"Follow up on a deploy mention"}}}}'

# gateway — publish three web-app routes. The helper binds an Identity account to
# this node, stages the static site into DuckFS, and signs + submits the routes:
#   • site — a NETWORK-hosted static app, served from DuckFS by consensus
#   • app  — a USER-hosted app the gateway proxies to a node-local server
#   • board — the network-visible kanban reference app
# Sign the routes with the demo wallet the seed itself minted — the seed
# always holds its password, so the sign step always runs (though failure is
# non-fatal; chat/tasks/pages are already durable regardless).
GATEWAY_ROUTES=3
GATEWAY_PW="$DEMO_PASSWORD"
bun "$SCRIPT_DIR/demo-gateway.mjs" "$URL" "$NODE_BIN" "$WSDIR" "$CHAIN" "$ID" "$USERKEY" "$GATEWAY_PW"
gateway_status=$?
# Route publishing is a demo garnish — its failure never kills the seed. The
# core workspace (chat, tasks, pages, identity) is committed before this runs.
case "$gateway_status" in
  0) ;;
  *)
    GATEWAY_ROUTES=0
    log "gateway routes skipped (exit $gateway_status) — see $WSDIR/seed.log"
    ;;
esac

log "seeded $N ops + $GATEWAY_ROUTES gateway web-app routes across pages, chat, tasks, agent, runs, forge, jobs, automations, files, gateway"

# ── 6. stop the node (state is durable on disk) ────────────────
kill "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null; trap - EXIT

cat <<EOF

$(printf '\033[32m[demo-seed] done.\033[0m')
Open the Ducktape app and pick the "$ID" network — it boots preloaded.

To WRITE (send a message, add a reaction, edit): the app signs with a wallet
from the network's keystore. Picking the network opens its wallet list — pick
the "demo" row, type its password into that row, and Unlock. That also makes
it the network's active wallet, so the next pick opens on it.

  wallet: demo   password: $DEMO_PASSWORD

EOF

if [ "$GATEWAY_ROUTES" -eq 0 ]; then
cat <<EOF
Gateway web apps were not published — see $WSDIR/seed.log for the exact
rejection. This is a demo garnish only: chat, tasks, pages and your identity are
all live.
EOF
else
cat <<EOF
Gateway web apps published on this node (open in the app's browser):
  • site.$ID.duck — a bouncing-DVD web app, served static from DuckFS by
                    consensus. Works now — nothing else to run.
  • app.$ID.duck  — user-hosted web app. The route is published, but its upstream
                    is a plain process YOU host. It shows "Unavailable" in the
                    Gateway view until you serve it, and goes Unavailable again if
                    that process stops:

                        make demo-app        # keep this running (foreground)

                    Re-run it after every \`make demo-seed\` (a re-seed wipes the
                    workspace and re-mints the loopback binding).
EOF
fi

cat <<EOF
Done with the demo? \`make demo-clear\` removes the workspace entirely.
EOF
