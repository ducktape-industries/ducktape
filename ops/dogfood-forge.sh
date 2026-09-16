#!/usr/bin/env bash
# make dogfood-forge — host ducktape's OWN source in ducktape's forge module.
# This flows GitHub origin/dev -> Forge dev without moving release-only main.
#
# Queries the committed Forge ref and imports local Git packs through generic
# blob and module-submit APIs as the node. Equal-tree divergent histories are
# joined; different trees require reviewed reconciliation. Local materialized
# objects supply remote history without a product HTTP route in the node.
#
# Resolution of the node's forge base URL, in order:
#   1. $DUCKTAPE_DEV_FORGE_URL           — explicit base, e.g. http://127.0.0.1:8844
#   2. the ONE workspace under the ducktape home — <home>/<dir>/node.toml's
#      http_listen, where <home> is $DUCKTAPE_HOME when set, else ~/.ducktape;
#      with several workspaces there is no default, name the node explicitly
#      (the workspace flow assigns a RANDOM http port, so this is not a fixed :8844)
#
# Env knobs:
#   DUCKTAPE_DEV_FORGE_URL  node API base URL override
#   FORGE_REPO              forge repository name   (default: ducktape)
#   SOURCE_REMOTE           canonical source remote      (default: origin)
#   SOURCE_BRANCH           canonical source branch      (default: dev)
#   SRC_REF                 explicit local ref override  (default: fetched
#                                                        SOURCE_REMOTE/BRANCH)
#
# The default deliberately does NOT use HEAD. A clean-but-stale primary checkout
# can trail origin/dev while still looking healthy, which silently pins every
# later agent run to an obsolete source tree. An explicit SRC_REF remains useful
# for intentional branch dogfood, but callers then own that override.
#
set -euo pipefail
cd "$(dirname "$0")/.."

FORGE_REPO="${FORGE_REPO:-ducktape}"
SOURCE_REMOTE="${SOURCE_REMOTE:-origin}"
SOURCE_BRANCH="${SOURCE_BRANCH:-dev}"
SRC_REF="${SRC_REF:-}"

log() { printf '\033[36m[dogfood]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[dogfood]\033[0m %s\n' "$*" >&2; exit 1; }

# The workspaces under the ducktape home ($DUCKTAPE_HOME when set, else
# ~/.ducktape): every directory holding a network.toml — the same listing
# `ducktape node list` and the app read. One node.toml path per line.
workspace_tomls() {
  local duck="${DUCKTAPE_HOME:-$HOME/.ducktape}" dir
  for dir in "$duck"/*/; do
    [ -f "$dir/network.toml" ] && [ -f "$dir/node.toml" ] && printf '%s\n' "${dir%/}/node.toml"
  done
}

http_listen_of() {
  sed -n 's/^[[:space:]]*http_listen[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -1
}

resolve_base_url() {
  if [ -n "${DUCKTAPE_DEV_FORGE_URL:-}" ]; then
    printf '%s' "${DUCKTAPE_DEV_FORGE_URL%/}"
    return
  fi
  local tomls listen
  tomls="$(workspace_tomls)"
  case "$(printf '%s\n' "$tomls" | grep -c .)" in
    1) listen="$(http_listen_of "$tomls")"
       if [ -n "$listen" ]; then
         printf 'http://%s' "$listen"
         return
       fi ;;
  esac
  die "no Forge node selected; set DUCKTAPE_DEV_FORGE_URL, or keep exactly one workspace under the ducktape home"
}

# The workspace behind the resolved node, when this box holds one — the
# directory holding the operator credential this script's pushes present.
# Matched on the port the node serves: `http_listen` is a wildcard bind by
# default, which the base url never spells.
resolve_workspace() {
  local toml listen
  while read -r toml; do
    [ -n "$toml" ] || continue
    listen="$(http_listen_of "$toml")"
    if [ -n "$listen" ] && [ "${listen##*:}" = "${BASE_URL##*:}" ]; then
      printf '%s' "$(dirname "$toml")"
      return
    fi
  done <<EOF
$(workspace_tomls)
EOF
}

BASE_URL="$(resolve_base_url)"
WORKSPACE="$(resolve_workspace)"
[ -n "$WORKSPACE" ] && [ -r "$WORKSPACE/admin.token" ] || die "a local node workspace and operator credential are required"
IMPORT_TOOL="$PWD/ops/forge-import.py"
FORGE_STORE=$(python3 - "$WORKSPACE" <<'PYCONFIG'
import pathlib, sys, tomllib
workspace = pathlib.Path(sys.argv[1])
config = tomllib.loads((workspace / 'node.toml').read_text())
storage = pathlib.Path(config.get('storage_dir', str(workspace / 'storage')))
if not storage.is_absolute():
    storage = workspace / storage
print(storage / 'forge-repo')
PYCONFIG
) || die "cannot resolve configured Git substrate"
forge_head() {
  python3 "$IMPORT_TOOL" head --node-url "$BASE_URL" --repo "$FORGE_REPO" --branch dev
}

if [ -z "$SRC_REF" ]; then
  log "fetching canonical source: $SOURCE_REMOTE $SOURCE_BRANCH"
  git fetch "$SOURCE_REMOTE" "$SOURCE_BRANCH"
  # Resolve the result of THIS fetch, not a synthesized remote-tracking ref.
  # A remote with a missing/nonstandard fetch refspec may update FETCH_HEAD
  # while leaving refs/remotes/<remote>/<branch> stale.
  SRC_REF="FETCH_HEAD"
fi

SOURCE_OID="$(git rev-parse --verify "$SRC_REF^{commit}")" ||
  die "source ref '$SRC_REF' does not resolve to a commit"
log "source commit: $SOURCE_OID ($SRC_REF)"

# A healthy node must serve generic query, blob, and submit APIs.
if ! curl -fsS -m 5 "$BASE_URL/v1/status" >/dev/null 2>&1; then
  # NOTE: no backticks in this string — it is double-quoted, so they would be
  # command substitution, and the die message would RUN whatever it names.
  die "no node responding at $BASE_URL — start a node first \
(cargo run -p noded-bin: the build stages the founding set its genesis \
composes from beside the binary), or set DUCKTAPE_DEV_FORGE_URL to a \
running node."
fi

# One push, whatever it weighs. The node's blob door streams what it receives
# onto disk and the relay carries it in an acknowledged window, so a first
# import of a whole history is the same operation as a one-commit update — no
# ranges, no retries, nothing here that a plain `git push` would not also get.
push_history() {
  local tip="$1"
  python3 "$IMPORT_TOOL" push --node-url "$BASE_URL" --token-file "$WORKSPACE/admin.token" --repo "$FORGE_REPO" --branch dev --tip "$tip" ||
    die "Forge push failed; any accepted ancestor remains safe to resume from"
}

FORGE_REF=refs/heads/dev
FORGE_OID="$(forge_head)"
EXPECTED_OID=$SOURCE_OID

if [ -z "$FORGE_OID" ]; then
  log "creating Forge dev at $SOURCE_OID"
  push_history "$SOURCE_OID"
else
  TMP_REF="refs/dogfood-sync/$$/forge-dev"
  trap 'git update-ref -d "$TMP_REF" >/dev/null 2>&1 || true' EXIT
  git fetch --no-tags "$FORGE_STORE/$FORGE_REPO" "$FORGE_OID:$TMP_REF"
  if [ "$FORGE_OID" = "$SOURCE_OID" ]; then
    log "Forge dev already matches GitHub dev"
  elif git merge-base --is-ancestor "$FORGE_OID" "$SOURCE_OID"; then
    log "fast-forwarding Forge dev to GitHub dev"
    push_history "$SOURCE_OID"
  elif git merge-base --is-ancestor "$SOURCE_OID" "$FORGE_OID"; then
    log "Forge dev already contains GitHub dev"
    EXPECTED_OID=$FORGE_OID
  elif git diff --quiet "$FORGE_OID" "$SOURCE_OID"; then
    command -v node >/dev/null || die "node is required to read the node identity"
    NODE_ID=$(
      curl -fsS -m 5 "$BASE_URL/v1/status" |
        node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const k=JSON.parse(s).public_key||"";if(!/^[0-9a-f]{64}$/i.test(k))process.exit(1);process.stdout.write(k.toLowerCase())})'
    ) || die "the node status has no valid public_key"
    TREE_OID=$(git rev-parse "$SOURCE_OID^{tree}")
    EXPECTED_OID=$(
      GIT_AUTHOR_NAME="$NODE_ID" \
      GIT_AUTHOR_EMAIL="$NODE_ID@nodes.duck" \
      GIT_COMMITTER_NAME="$NODE_ID" \
      GIT_COMMITTER_EMAIL="$NODE_ID@nodes.duck" \
        git commit-tree "$TREE_OID" -p "$FORGE_OID" -p "$SOURCE_OID" <<EOF
Synchronize GitHub dev into Forge dev

Join provenance-equivalent development histories without rewriting either side.
EOF
    )
    log "joining provenance-equivalent dev histories at $EXPECTED_OID"
    push_history "$EXPECTED_OID"
  else
    die "Forge dev $FORGE_OID and GitHub dev $SOURCE_OID diverged with different trees; reconcile them in a reviewed PR"
  fi
fi

# A successful git process is not enough evidence for the next dispatch.
VERIFIED_OID="$(forge_head)"
if [ "$VERIFIED_OID" != "$EXPECTED_OID" ]; then
  die "Forge dev verification failed: expected $EXPECTED_OID, got ${VERIFIED_OID:-missing}"
fi

log "verified Forge dev at $VERIFIED_OID"
log "release-only Forge main was not changed."
log "re-run \`make dogfood-forge\` before creating agent work."
