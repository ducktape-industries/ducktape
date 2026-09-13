#!/usr/bin/env bash
# make dogfood-forge — host ducktape's OWN source in ducktape's forge module.
# This flows GitHub origin/dev -> Forge dev without moving release-only main.
#
# Registers a static git remote `ducktape-dev` pointing at the local dev node's
# forge git smart-HTTP endpoint, fetches the canonical development branch, then
# synchronizes that exact history into Forge `dev`. Re-running this command
# refreshes Forge before agent work. A fast-forward is direct; equal-tree
# mirror divergence is joined with a two-parent bridge; differing trees fail
# closed for a reviewed reconciliation.
#
# This is the INTENDED big-repo path: `git-receive-pack` lifts the body cap to
# the relay-safe pack limit and stores each packfile node-locally, submitting a tiny
# `forge Push` (32-byte digest + oids) through consensus — the pack NEVER crosses
# consensus. (Contrast POST /v1/files/blob, which is capped at the 4 MB chunk
# size and 413s a whole-repo pack.)
#
# Resolution of the node's forge base URL, in order:
#   1. $DUCKTAPE_DEV_FORGE_URL           — explicit base, e.g. http://127.0.0.1:8844
#   2. the ONE workspace under the ducktape home — <home>/<dir>/node.toml's
#      http_listen, where <home> is $DUCKTAPE_HOME when set, else ~/.ducktape;
#      with several workspaces there is no default, name the node explicitly
#      (the workspace flow assigns a RANDOM http port, so this is not a fixed :8844)
#
# Env knobs:
#   DUCKTAPE_DEV_FORGE_URL  node base URL override (no trailing /forge/<repo>)
#   FORGE_REPO              forge repo name in the URL   (default: ducktape)
#   FORGE_REMOTE            local git remote name        (default: ducktape-dev)
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
# NOTE: `ducktape-dev` is a normal git remote, and git stores remotes in the
# SHARED .git/config (git-common-dir) — visible to every `git worktree` of this
# repo. If you run several worktrees each with their own node, they share this
# one remote; the script re-resolves and re-points it every run (and warns when
# the URL changes) so a run always targets the resolved node, but a stale
# `git push ducktape-dev` from another worktree could hit the wrong node. Set
# FORGE_REMOTE to a per-worktree name if you run many nodes at once.
set -euo pipefail
cd "$(dirname "$0")/.."

FORGE_REPO="${FORGE_REPO:-ducktape}"
FORGE_REMOTE="${FORGE_REMOTE:-ducktape-dev}"
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
REMOTE_URL="$BASE_URL/forge/$FORGE_REPO"

# A push MUST prove itself: `git-receive-pack` takes git's own push certificate
# (`git push --signed`, whose signer becomes the repo's owner on chain) or this
# node's operator credential, which makes the NODE the owner. This script seeds
# the node's own mirror of the canonical repo, so the node is the right owner
# and this is the right proof.
#
# GIT_CONFIG_*, not `git -c`: an argv is world-readable through /proc, and this
# is a secret. Exported so `git push`/`git fetch` below inherit it.
WORKSPACE="$(resolve_workspace)"
if [ -n "$WORKSPACE" ] && [ -r "$WORKSPACE/admin.token" ]; then
  export GIT_CONFIG_COUNT=1
  export GIT_CONFIG_KEY_0=http.extraHeader
  export GIT_CONFIG_VALUE_0="x-ducktape-admin-token: $(cat "$WORKSPACE/admin.token")"
else
  log "WARNING: no operator credential found; the node will refuse an unsigned push"
fi

log "node forge endpoint: $REMOTE_URL"

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

# a healthy node is required (git-receive-pack is served off the node's http
# surface). fail fast with an actionable message rather than a git transport error.
if ! curl -fsS -m 5 "$BASE_URL/v1/status" >/dev/null 2>&1; then
  # NOTE: no backticks in this string — it is double-quoted, so they would be
  # command substitution, and the die message would RUN whatever it names.
  die "no node responding at $BASE_URL — start a node first \
(cargo run -p noded-bin: the build stages the founding set its genesis \
composes from beside the binary), or set DUCKTAPE_DEV_FORGE_URL to a \
running node."
fi

# idempotent remote wiring: add, or re-point if it already exists.
if existing="$(git remote get-url "$FORGE_REMOTE" 2>/dev/null)"; then
  if [ "$existing" != "$REMOTE_URL" ]; then
    log "WARNING: '$FORGE_REMOTE' currently points at $existing"
    log "         re-pointing to $REMOTE_URL — this remote is SHARED across all git"
    log "         worktrees of this repo, so this also moves it for other worktrees."
  fi
  git remote set-url "$FORGE_REMOTE" "$REMOTE_URL"
  log "remote '$FORGE_REMOTE' -> $REMOTE_URL"
else
  git remote add "$FORGE_REMOTE" "$REMOTE_URL"
  log "added remote '$FORGE_REMOTE' -> $REMOTE_URL"
fi

# Retry only an HTTP body-limit rejection, splitting the remaining first-parent
# history in half. Each accepted tip is an ancestor of the final tip; no history
# is rewritten. A single commit (including a large merge) may still exceed the
# server cap, in which case stop with the last accepted ancestor intact.
push_history() {
  local tip="$1" base="${2:-}" output remaining count middle
  output="$(mktemp)"
  if LC_ALL=C git push "$FORGE_REMOTE" "$tip:$FORGE_REF" >"$output" 2>&1; then
    cat "$output"
    rm -f "$output"
    return
  fi
  cat "$output" >&2
  if ! grep -Eq 'HTTP 413|HTTP code = 413|returned error: 413' "$output"; then
    rm -f "$output"
    die "Forge push failed; any accepted ancestor remains safe to resume from"
  fi
  rm -f "$output"
  if [ -n "$base" ]; then
    remaining="$(git rev-list --first-parent --ancestry-path --reverse "$tip" "^$base")"
  else
    remaining="$(git rev-list --first-parent --reverse "$tip")"
  fi
  count="$(printf '%s\n' "$remaining" | grep -c .)"
  if [ "$count" -le 1 ]; then
    die "commit $tip exceeds the Forge HTTP pack limit even on its own; cannot split its history further"
  fi
  middle="$(printf '%s\n' "$remaining" | sed -n "$((count / 2))p")"
  log "pack exceeds the node limit; importing ancestor $middle first"
  push_history "$middle" "$base"
  push_history "$tip" "$middle"
}

FORGE_REF=refs/heads/dev
FORGE_OID="$(git ls-remote "$REMOTE_URL" "$FORGE_REF" | awk 'NR == 1 { print $1 }')"
EXPECTED_OID=$SOURCE_OID

if [ -z "$FORGE_OID" ]; then
  log "creating Forge dev at $SOURCE_OID"
  push_history "$SOURCE_OID"
else
  TMP_REF="refs/dogfood-sync/$$/forge-dev"
  trap 'git update-ref -d "$TMP_REF" >/dev/null 2>&1 || true' EXIT
  git fetch --no-tags "$FORGE_REMOTE" "$FORGE_REF:$TMP_REF"
  if [ "$FORGE_OID" = "$SOURCE_OID" ]; then
    log "Forge dev already matches GitHub dev"
  elif git merge-base --is-ancestor "$FORGE_OID" "$SOURCE_OID"; then
    log "fast-forwarding Forge dev to GitHub dev"
    push_history "$SOURCE_OID" "$FORGE_OID"
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
    push_history "$EXPECTED_OID" "$FORGE_OID"
  else
    die "Forge dev $FORGE_OID and GitHub dev $SOURCE_OID diverged with different trees; reconcile them in a reviewed PR"
  fi
fi

# A successful git process is not enough evidence for the next dispatch.
VERIFIED_OID="$(git ls-remote "$REMOTE_URL" "$FORGE_REF" | awk 'NR == 1 { print $1 }')"
if [ "$VERIFIED_OID" != "$EXPECTED_OID" ]; then
  die "Forge dev verification failed: expected $EXPECTED_OID, got ${VERIFIED_OID:-missing}"
fi

log "verified Forge dev at $VERIFIED_OID"
log "release-only Forge main was not changed."
log "re-run \`make dogfood-forge\` before creating agent work."
