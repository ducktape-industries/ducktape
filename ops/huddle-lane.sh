#!/usr/bin/env bash
# ops/huddle-lane.sh — the two-node huddle lane: a live network for two
# SEPARATE app-side sessions, the arrangement a huddle actually breaks in.
#
# Stands up two real `ducktape` nodes in the dev shape (deterministic seeds,
# one namespace, real userspace WireGuard between them — huddle media rides
# the overlay and nothing else), creates a channel, and mints ONE USER KEY PER
# SIDE. Identity is process-global in the app, so two people means two homes.
#
# It then prints the two commands to run — one per terminal, one per person:
# the app's own live lane (`app/src/tests/huddle_live.rs`), which joins the
# huddle, waits for the other side, publishes this camera and asserts the
# other side's beacon AND picture arrive.
#
# The nodes keep running when this exits; `--stop` tears the lane down.
#
# DEVICES ARE THE ONE THING THIS CANNOT MINT. The lane asserts the other side
# is seen AND heard, so each side needs a camera and a microphone. A headless
# box can borrow both — one camera for the pair, one sound card per side
# (`ALSA_CARD` picks which one a process calls `default`):
#
#   sudo modprobe v4l2loopback devices=1 exclusive_caps=1 max_openers=8
#   sudo chmod a+rw /dev/video0
#   ffmpeg -re -f lavfi -i testsrc=size=640x480:rate=30 -pix_fmt yuyv422 \
#          -f v4l2 /dev/video0 &
#
#   sudo modprobe snd-aloop index=0,1 enable=1,1 pcm_substreams=4 id=lanea,laneb
#   sudo chmod -R a+rw /dev/snd
#   ffmpeg -y -f lavfi -i "sine=frequency=500:duration=600:sample_rate=48000" \
#          -ac 2 -c:a pcm_s16le /tmp/tone.wav
#   for card in lanea laneb; do             # LOOP them: a tone that runs out is
#     while true; do                        # a microphone that goes dead mid-run,
#       aplay -D hw:$card,1,0 /tmp/tone.wav # and the far side then waits on
#     done &                                # silence for no reason
#   done
#
# An aloop card loops device 1's playback into device 0's capture, which is
# what `default` records from — so the tone above IS that side's microphone.
# (`snd-aloop` and the v4l2 core live in linux-modules-extra-$(uname -r).)
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LANE="${HUDDLE_LANE_DIR:-${TMPDIR:-/tmp}/ducktape-huddle-lane}"
NODE_BIN="${DUCKTAPE_BIN:-$ROOT/target/debug/ducktape}"
CHANNEL="${HUDDLE_LANE_CHANNEL:-eng}"
# The default says what it is: these two identities live in a throwaway lane
# directory and are printed below in the clear. Nothing outside this lane is
# ever minted with it.
PASSWORD="${HUDDLE_LANE_PASSWORD:-scratch-only-password}"
NAMESPACE="${HUDDLE_LANE_NAMESPACE:-huddle-lane}"

log(){ printf '\033[36m[huddle-lane]\033[0m %s\n' "$*"; }
die(){ printf '\033[31m[huddle-lane] %s\033[0m\n' "$*" >&2; exit 1; }

# Stop only what this lane started, by the pid files it wrote — never a
# pattern match, which would cheerfully kill an editor or an unrelated node.
stop_lane(){
  for pid_file in "$LANE"/*.pid; do
    [ -e "$pid_file" ] || continue
    local pid; pid=$(cat "$pid_file")
    if kill -0 "$pid" 2>/dev/null; then
      log "stopping $(basename "$pid_file" .pid) (pid $pid)"
      kill "$pid" 2>/dev/null
    fi
    rm -f "$pid_file"
  done
}

if [ "${1:-}" = "--stop" ]; then
  stop_lane
  log "lane stopped; state kept at $LANE (rm -rf to reclaim)"
  exit 0
fi

[ -x "$NODE_BIN" ] || die "no ducktape binary at $NODE_BIN — cargo build -p node-bin --bin ducktape"

stop_lane
rm -rf "$LANE"
mkdir -p "$LANE"

# Six free ports: p2p/wireguard, rpc, and the app surface, per node. Asking
# the OS for each in turn is the same trick the e2e harness uses.
port(){ python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }
P2P_A=$(port); P2P_B=$(port)
RPC_A=$(port); RPC_B=$(port)
HTTP_A=$(port); HTTP_B=$(port)
INVITE_A=$(port); INVITE_B=$(port)

# The DEV SHAPE: both nodes are in one namespace with deterministic seeded
# keys, so there is no invite ceremony to run — the pair is a network the
# moment both are up. `wireguard_listen` is not optional: with no overlay the
# node refuses to wire a call hub at all, and every huddle join fails fast.
write_config(){ # write_config <idx> <p2p> <rpc> <http> <invite>
  local idx=$1 p2p=$2 rpc=$3 http=$4 invite=$5
  cat > "$LANE/node$idx.toml" <<EOF
id = $idx
listen = "127.0.0.1:$p2p"
namespace = "$NAMESPACE"
peer_seeds = [0, 1]
validator_seeds = [0, 1]
# no modules key: every node of this lane runs the binary whose own build
# staged the set, and that directory is named for the checkout
peer_addrs = ["127.0.0.1:$P2P_A", "127.0.0.1:$P2P_B"]
storage_dir = "$LANE/storage-$idx"
rpc_listen = "127.0.0.1:$rpc"
http_listen = "127.0.0.1:$http"
wireguard_listen = "127.0.0.1:$p2p"
invite_listen = "127.0.0.1:$invite"
EOF
}
write_config 0 "$P2P_A" "$RPC_A" "$HTTP_A" "$INVITE_A"
write_config 1 "$P2P_B" "$RPC_B" "$HTTP_B" "$INVITE_B"

for idx in 0 1; do
  "$NODE_BIN" node run --config "$LANE/node$idx.toml" > "$LANE/node$idx.log" 2>&1 &
  echo $! > "$LANE/node$idx.pid"
  log "node $idx starting (pid $(cat "$LANE/node$idx.pid"), log $LANE/node$idx.log)"
done

wait_for(){ # wait_for <log> <marker> <seconds>
  local log_file=$1 marker=$2 budget=$3
  for _ in $(seq "$budget"); do
    grep -q "$marker" "$log_file" && return 0
    sleep 1
  done
  return 1
}
for idx in 0 1; do
  wait_for "$LANE/node$idx.log" "converged root_hash=" 120 \
    || { tail -20 "$LANE/node$idx.log" >&2; die "node $idx did not converge"; }
  log "node $idx converged"
done

# CONSENSUS UP IS NOT MEDIA UP, and handing back a lane in between is how you
# get "the mesh overlay is not up on this node yet" thrown at a person who did
# exactly what they were told. The call hub binds when the overlay interface
# comes up (~12 s after boot here), and the tunnel only carries traffic once
# the WireGuard handshake completes.
for idx in 0 1; do
  wait_for "$LANE/node$idx.log" "hub bound" 120 \
    || { tail -20 "$LANE/node$idx.log" >&2; die "node $idx never bound a call hub"; }
  wait_for "$LANE/node$idx.log" "peer handshake COMPLETE" 120 \
    || { tail -20 "$LANE/node$idx.log" >&2; die "node $idx never handshook the overlay"; }
  log "node $idx has a call hub on a live overlay"
done

# The channel both sides huddle in. The frameless submit lane stamps `origin`
# as the author — this row is the room, not a person.
# A mutating route takes either a user signature or the node's own operator
# credential; this lane drives node 0 as its operator, from its storage dir.
create=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HTTP_A/v1/submit" \
  -H 'content-type: application/json' \
  -H "x-ducktape-admin-token: $(cat "$LANE/storage-0/admin.token")" \
  -d "{\"target\":\"chat\",\"payload\":{\"create_channel\":{\"channel_id\":\"$CHANNEL\",\"name\":\"Huddle lane\",\"post_policy\":\"open\"}},\"origin\":\"lane\"}")
[ "$create" = "200" ] || die "creating #$CHANNEL was rejected [$create]"
log "channel #$CHANNEL created"

# One user key per side: the app filters ITSELF out of the fan-out by the
# roster row matching its own key, so both sides sharing a key would be one
# person joining twice.
for side in a b; do
  mkdir -p "$LANE/home-$side"
  printf '%s\n' "$PASSWORD" | "$NODE_BIN" user key init --out "$LANE/home-$side/user.key" >/dev/null \
    || die "could not mint the $side side's user key"
done
log "two user identities minted (password: $PASSWORD)"

cat <<EOF

The lane is up. Run these in two terminals — one per person:

  ALSA_CARD=lanea \\
  DUCKTAPE_USER_KEY=$LANE/home-a/user.key \\
  DUCKTAPE_HOME=$LANE/home-a \\
  DUCKTAPE_NODE=http://127.0.0.1:$HTTP_A \\
  DUCKTAPE_HUDDLE_PASSWORD=$PASSWORD \\
  DUCKTAPE_HUDDLE_CHANNEL=$CHANNEL \\
  cargo test -p ducktape-app -- --ignored --nocapture huddle_live

  ALSA_CARD=laneb \\
  DUCKTAPE_USER_KEY=$LANE/home-b/user.key \\
  DUCKTAPE_HOME=$LANE/home-b \\
  DUCKTAPE_NODE=http://127.0.0.1:$HTTP_B \\
  DUCKTAPE_HUDDLE_PASSWORD=$PASSWORD \\
  DUCKTAPE_HUDDLE_CHANNEL=$CHANNEL \\
  cargo test -p ducktape-app -- --ignored --nocapture huddle_live

(ALSA_CARD only matters on a box borrowing sound cards — see this script's
header. On a laptop with one real microphone, drop it.)

Or point the desktop app at either side with the same two env vars.
Node logs: $LANE/node0.log, $LANE/node1.log
Tear down: $SCRIPT_DIR/huddle-lane.sh --stop
EOF
