#!/usr/bin/env bash
# Publish a release to a network's duckfs — the desktop app's (--kind app,
# the default) or the node's (--kind node).
#
# Given built archives (one per platform), this composes the sealed manifest,
# signs it with the release wallet and lands the archives, the manifest and
# its signature under /shared/releases with `ducktape fs put`. Every step
# that hashes, seals, signs or chunks runs inside `ducktape`; this script only
# sequences them.
#
#   ops/release/publish.sh --node http://127.0.0.1:8844 \
#       --key ~/.ducktape/release/keys/release.key \
#       --sequence 18 --display "2026.09.2+9d71b254a" \
#       --archive macos-aarch64=target/Ducktape-macos-aarch64.tar.zst \
#       --archive linux-x86_64=target/Ducktape-linux-x86_64.tar.zst
#
# A node release is the same three steps under its own channel, and WHEN the
# network runs it is a separate governance decision:
#
#   ops/release/publish.sh --kind node --node http://127.0.0.1:8844 \
#       --key ... --sequence 3 --display "2026.09.3+e6352411a" \
#       --archive linux-x86_64=target/ducktape-linux-x86_64.tar.zst
#   ducktape release schedule --sha <archive sha256> --at <height>
#
# The release wallet is an ordinary ducktape wallet minted into a workspace
# of its own (`ducktape wallet new release --workspace ~/.ducktape/release`);
# its public key (what `ducktape release sign` prints) is what an install
# pins. Its password is read ONCE here and fed to each verb on stdin.
#
# Order: archives first, then the manifest, then the signature — a reader
# never sees a manifest naming an archive that is not there yet. Between the
# manifest and the signature landing a reader sees bad_signature once and
# retries on its next check; that is the whole cost of two files.
set -euo pipefail

DUCKTAPE_BIN="${DUCKTAPE_BIN:-ducktape}"
NODE=""
KEY=""
SEQUENCE=""
DISPLAY_TEXT=""
NOTES_URL=""
OUT_DIR="${PUBLISH_OUT_DIR:-target/release-publish}"
ARCHIVES=()
EXTRA=()

KIND="app"

usage() {
  sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --node) NODE="$2"; shift 2 ;;
    --key) KEY="$2"; shift 2 ;;
    --sequence) SEQUENCE="$2"; shift 2 ;;
    --display) DISPLAY_TEXT="$2"; shift 2 ;;
    --notes-url) NOTES_URL="$2"; shift 2 ;;
    --archive) ARCHIVES+=("$2"); shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --kind) KIND="$2"; shift 2 ;;
    # forwarded to `release manifest` verbatim (--node-contract,
    # --successor-key, --successor-from)
    --node-contract|--successor-key|--successor-from) EXTRA+=("$1" "$2"); shift 2 ;;
    -h|--help) usage ;;
    *) echo "publish.sh: unknown argument $1" >&2; usage ;;
  esac
done

[ -n "$NODE" ] || { echo "publish.sh: --node <url> is required" >&2; exit 2; }
[ -n "$KEY" ] || { echo "publish.sh: --key <release wallet key file> is required" >&2; exit 2; }
[ -n "$SEQUENCE" ] || { echo "publish.sh: --sequence <n> is required" >&2; exit 2; }
[ -n "$DISPLAY_TEXT" ] || { echo "publish.sh: --display <text> is required" >&2; exit 2; }
[ "${#ARCHIVES[@]}" -gt 0 ] || { echo "publish.sh: at least one --archive <os>-<arch>=<path> is required" >&2; exit 2; }
[ -f "$KEY" ] || { echo "publish.sh: no key file at $KEY" >&2; exit 1; }

case "$KIND" in
  app)  CHANNEL="stable" ;;
  node) CHANNEL="node" ;;
  *) echo "publish.sh: --kind takes app or node, not $KIND" >&2; exit 2 ;;
esac

mkdir -p "$OUT_DIR"
# The manifest's duckfs name IS its channel — `app_update::layout::Kind`.
MANIFEST="$OUT_DIR/$CHANNEL.json"
SIGNATURE="$MANIFEST.sig"
DUCKFS_MANIFEST="/shared/releases/$CHANNEL.json"

if [ -n "${RELEASE_WALLET_PASSWORD:-}" ]; then
  PASSWORD="$RELEASE_WALLET_PASSWORD"
else
  read -rsp "release wallet password: " PASSWORD </dev/tty
  echo >&2
fi
password() { printf '%s\n' "$PASSWORD"; }

# 1. compose + seal the manifest; capture `<local>\t<duckfs path>` per artifact.
ARCHIVE_ARGS=()
for archive in "${ARCHIVES[@]}"; do ARCHIVE_ARGS+=(--archive "$archive"); done
PLAN="$OUT_DIR/plan.tsv"
"$DUCKTAPE_BIN" release manifest --out "$MANIFEST" --sequence "$SEQUENCE" --kind "$KIND" \
  --display "$DISPLAY_TEXT" --notes-url "$NOTES_URL" "${ARCHIVE_ARGS[@]}" "${EXTRA[@]}" > "$PLAN"

# 2. sign it with the release wallet; the printed pubkey is the pin.
PUBKEY="$(password | "$DUCKTAPE_BIN" release sign "$MANIFEST" --key "$KEY" --kind "$KIND")"
echo "release key: $PUBKEY" >&2

# 3. land the archives, then the manifest, then its signature.
while IFS=$'\t' read -r local duckfs; do
  echo "put $local -> $duckfs" >&2
  password | "$DUCKTAPE_BIN" fs put "$local" "$duckfs" --node "$NODE" --key "$KEY" \
    --message "release $SEQUENCE: $(basename "$duckfs")"
done < "$PLAN"
echo "put $MANIFEST -> $DUCKFS_MANIFEST" >&2
password | "$DUCKTAPE_BIN" fs put "$MANIFEST" "$DUCKFS_MANIFEST" --node "$NODE" --key "$KEY" \
  --message "release $SEQUENCE: manifest"
echo "put $SIGNATURE -> $DUCKFS_MANIFEST.sig" >&2
password | "$DUCKTAPE_BIN" fs put "$SIGNATURE" "$DUCKFS_MANIFEST.sig" --node "$NODE" --key "$KEY" \
  --message "release $SEQUENCE: signature"

echo "published $KIND release $SEQUENCE ($DISPLAY_TEXT) to $NODE" >&2
