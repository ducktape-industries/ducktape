#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$ROOT"
say() { echo "==> $*"; }
as_root() { if (( EUID == 0 )); then "$@"; else sudo "$@"; fi; }

case "$(uname -s)" in
  Linux)
    say "installing README Linux prerequisites"
    as_root apt-get update
    as_root apt-get install -y curl build-essential pkg-config libclang-dev libasound2-dev \
      libx11-xcb-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev libfreetype6-dev
    HOST_IP=${FRESH_INSTALL_HOST:-$(hostname -I | tr ' ' '\n' | awk '$0 !~ /:/ && $0 !~ /^127\./ { print; exit }')}
    ;;
  Darwin)
    say "installing README macOS prerequisites"
    xcode-select -p >/dev/null 2>&1 || xcode-select --install
    command -v brew >/dev/null && brew install pkg-config
    HOST_IP=${FRESH_INSTALL_HOST:-$(ipconfig getifaddr en0)}
    ;;
  *) echo "unsupported host: $(uname -s)" >&2; exit 1 ;;
esac

[[ -n "$HOST_IP" && "$HOST_IP" != 127.* && "$HOST_IP" != "::1" ]] || {
  echo "could not find a non-loopback IPv4 address" >&2
  exit 1
}

say "make install (CLI and desktop app)"
make install
export PATH="$HOME/.cargo/bin:$PATH"
say "checking the installed CLI"
ducktape --version

say "founding a remotely reachable node"
ducktape node init --name fresh-install-ci \
  --http 0.0.0.0:8844 --gateway 127.0.0.1:8845 \
  --listen 0.0.0.0:8846 --advertised "$HOST_IP:8846" \
  --rpc 127.0.0.1:8847 --wireguard-listen 0.0.0.0:51820 \
  --wireguard-advertised "$HOST_IP:51820" --invite-listen 0.0.0.0:51821 \
  --primary-coordinator none

NODE_LOG="$ROOT/fresh-install-node.log"
cleanup() {
  if [[ -n ${NODE_PID:-} ]]; then kill "$NODE_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT
say "starting the node in the background"
ducktape node run >"$NODE_LOG" 2>&1 & NODE_PID=$!
HEALTH_URL="http://$HOST_IP:8844/v1/status"
say "waiting up to 60 seconds for Ready at $HEALTH_URL"
for attempt in {1..60}; do
  status=$(curl --fail --silent --max-time 2 "$HEALTH_URL" 2>/dev/null || true)
  if grep -Eq '"phase"[[:space:]]*:[[:space:]]*"(validating|serving)"' <<<"$status"; then
    echo "Ready"
    break
  fi
  if ! kill -0 "$NODE_PID" 2>/dev/null; then
    echo "node exited before Ready" >&2
    tail -100 "$NODE_LOG" >&2
    exit 1
  fi
  (( attempt == 60 )) && { echo "node was not Ready within 60 seconds" >&2; tail -100 "$NODE_LOG" >&2; exit 1; }
  sleep 1
done

say "creating an invite without logging its contents"
if ducktape node invite >/dev/null 2>&1; then invite_status=0; else invite_status=$?; fi
echo "invite creation exit code: $invite_status"
(( invite_status == 0 ))
echo "FRESH-INSTALL PASS"
