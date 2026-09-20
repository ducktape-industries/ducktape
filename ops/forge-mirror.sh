#!/usr/bin/env bash
# Mirror GitHub branches into a node's Forge, fast-forward only.
#
# Per (repository, branch): fetch the branch from its source, read Forge's
# head with `forge-import.py head`, and push the fetched tip with
# `forge-import.py push` when Forge's head is empty or an ancestor of it. A
# rerun with nothing new pushes nothing. Anything else is a non-fast-forward
# and is REFUSED, never force-synced: the loop names which side moved,
# carries on with the other pairs, and exits non-zero. A human reconciles.
#
# Which side moved is told by the tip this mirror last pushed, kept as
# `refs/mirrored/<branch>` in its state repository (which also keeps that
# commit's objects alive): Forge still there means the source rewound; Forge
# anywhere else means something other than this mirror moved Forge.
#
# Configuration (environment; ops/node/ducktape-forge-mirror.service reads it
# from /etc/ducktape/forge-mirror.env):
#   FORGE_MIRROR_NODE        node API base, e.g. http://127.0.0.1:8844
#   FORGE_MIRROR_TOKEN_FILE  that node's admin.token (the import submits as the
#                            node operator)
#   FORGE_MIRROR_SOURCE      source base; repository <r> is fetched from
#                            <source>/<r>, e.g. https://github.com/ducktape-industries
#   FORGE_MIRROR_REPOS       space-separated repository names; each is also
#                            its Forge repository name
#   FORGE_MIRROR_BRANCHES    space-separated branches (default: dev main)
#   FORGE_MIRROR_STATE       directory holding one bare state repository per
#                            repository (default: $STATE_DIRECTORY)
set -euo pipefail

IMPORT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/forge-import.py"
NODE="${FORGE_MIRROR_NODE:?set FORGE_MIRROR_NODE to the node API base}"
TOKEN_FILE="${FORGE_MIRROR_TOKEN_FILE:?set FORGE_MIRROR_TOKEN_FILE to the node admin.token}"
SOURCE="${FORGE_MIRROR_SOURCE:?set FORGE_MIRROR_SOURCE to the source base url}"
REPOS="${FORGE_MIRROR_REPOS:?set FORGE_MIRROR_REPOS to the repositories to mirror}"
BRANCHES="${FORGE_MIRROR_BRANCHES:-dev main}"
STATE="${FORGE_MIRROR_STATE:-${STATE_DIRECTORY:?set FORGE_MIRROR_STATE}}"

log() { printf '[forge-mirror] %s\n' "$*"; }
refuse() { printf '[forge-mirror] REFUSED %s\n' "$*" >&2; }

forge_head() {
  python3 "$IMPORT" head --node-url "$NODE" --repo "$1" --branch "$2"
}

# mirror one (repository, branch); returns 1 on a refusal. Called under `||`,
# where `set -e` does not hold, so every step checks its own failure.
mirror() {
  local repo="$1" branch="$2" source forge mirrored landed
  source="$(git rev-parse --verify "refs/remotes/origin/$branch^{commit}")" || return 1
  if ! forge="$(forge_head "$repo" "$branch")"; then
    refuse "$repo $branch: cannot read Forge's head from $NODE"
    return 1
  fi
  mirrored="$(git rev-parse --verify -q "refs/mirrored/$branch" || true)"
  if [ "$forge" = "$source" ]; then
    log "$repo $branch: Forge is at $source already"
    git update-ref "refs/mirrored/$branch" "$source"
    return 0
  fi
  if [ -n "$forge" ] && ! git merge-base --is-ancestor "$forge" "$source" 2>/dev/null; then
    if [ -n "$mirrored" ] && [ "$forge" = "$mirrored" ]; then
      refuse "$repo $branch: the source rewound — Forge is at $forge, which this mirror pushed, and the source's $source does not contain it"
    else
      refuse "$repo $branch: Forge moved outside this mirror — Forge is at $forge, this mirror last pushed ${mirrored:-nothing}, and the source's $source does not contain it"
    fi
    return 1
  fi
  log "$repo $branch: ${forge:-empty} -> $source"
  if ! python3 "$IMPORT" push --node-url "$NODE" --token-file "$TOKEN_FILE" \
    --repo "$repo" --branch "$branch" --tip "refs/remotes/origin/$branch" >/dev/null; then
    refuse "$repo $branch: the push of $source failed"
    return 1
  fi
  landed="$(forge_head "$repo" "$branch" || true)"
  if [ "$landed" != "$source" ]; then
    refuse "$repo $branch: pushed $source but Forge reports ${landed:-nothing}"
    return 1
  fi
  git update-ref "refs/mirrored/$branch" "$source"
}

refused=0
for repo in $REPOS; do
  state="$STATE/$repo.git"
  [ -d "$state" ] || git init -q --bare "$state"
  for branch in $BRANCHES; do
    # the source's tip, as its fetch reports it: a forced update is taken
    # here, and refused above only if Forge cannot fast-forward to it.
    if ! git -C "$state" fetch -q --no-tags "$SOURCE/$repo" \
      "+refs/heads/$branch:refs/remotes/origin/$branch"; then
      refuse "$repo $branch: fetch from $SOURCE/$repo failed"
      refused=1
      continue
    fi
    # forge-import.py runs git in its working directory.
    (cd "$state" && mirror "$repo" "$branch") || refused=1
  done
done
exit "$refused"
