#!/usr/bin/env bash
# Reap worktrees whose work is fully merged into origin/dev.
#
# A worktree's life ends when its change merges; what it leaves behind is ~20 GB of
# Cargo target and nothing else. Twelve of them once ate 250 GB.
#
# Dry-run by default. It removes worktrees; that is not something to do on a
# typo. Pass --yes to act.
#
#   ops/worktree-clean.sh              # show what would be reaped
#   ops/worktree-clean.sh --yes        # do it
#   ops/worktree-clean.sh --yes --force  # also take worktrees that have live processes
#
# Refuses, always, to touch a worktree that is dirty, holds no commit of its
# own, or carries undelivered work — commits neither ancestors of origin/dev nor
# one squash commit on it with the same patch. Unmerged work is never this
# script's to throw away.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

YES=0
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --yes) YES=1 ;;
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# The MAIN checkout, from git itself (always the first entry) — `pwd` would
# nominate the primary for sweeping whenever this runs from inside a worktree,
# leaving only the live-process refusal between it and removal.
PRIMARY="$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')"
say() { [ "$YES" = 1 ] && echo "  $*" || echo "  [dry-run] $*"; }
run() { [ "$YES" = 1 ] && "$@"; }

command -v python3 >/dev/null || { echo "python3 is required to check active caches" >&2; exit 1; }
[ -d /proc/self/fd ] || { echo "process reference checks require /proc; nothing removed" >&2; exit 1; }

git fetch -q origin dev 2>/dev/null || true

# ── does anything live in this directory? ────────────────────────────────────
# A borrowed Cargo target can be live under another worktree's cwd. Keep
# references through open files, mapped artifacts and CARGO_TARGET_DIR too.
# This tool handles same-user caches only; cross-user consumers are unsupported.
# A process the kernel will not show (a vanished pid, or one whose /proc entries
# refuse even its own user, as systemd --user does) holds no reference this
# tool can see, so it counts for nothing and the scan goes on.
# Print only PIDs: process environments may contain credentials.
pids_under() {
  python3 - "$1" <<'PYTHON'
import os
from pathlib import Path
import sys

root = os.path.realpath(sys.argv[1])
if os.stat(root).st_uid != os.geteuid():
    raise PermissionError("worktree is owned by another user")

UNSEEN = (FileNotFoundError, ProcessLookupError, PermissionError)

def inside(path):
    return path == root or path.startswith(root + "/")

def references(process):
    for name in ("cwd", "exe"):
        try:
            if inside(os.readlink(process / name)):
                return True
        except UNSEEN:
            pass
    try:
        for descriptor in (process / "fd").iterdir():
            try:
                if inside(os.readlink(descriptor)):
                    return True
            except UNSEEN:
                pass
    except UNSEEN:
        pass
    try:
        for entry in (process / "environ").read_bytes().split(b"\0"):
            if entry.startswith(b"CARGO_TARGET_DIR="):
                target = os.fsdecode(entry.split(b"=", 1)[1])
                if not os.path.isabs(target):
                    target = os.path.join(os.readlink(process / "cwd"), target)
                if inside(os.path.realpath(target)):
                    return True
    except UNSEEN:
        pass
    try:
        for line in (process / "maps").read_text().splitlines():
            fields = line.split(maxsplit=5)
            if len(fields) == 6 and inside(fields[5]):
                return True
    except UNSEEN:
        pass
    return False

for process in Path("/proc").iterdir():
    if not process.name.isdigit() or int(process.name) == os.getpid():
        continue
    try:
        owner = process.stat().st_uid
    except UNSEEN:
        continue
    if owner == os.geteuid() and references(process):
        print(process.name)
PYTHON
}

# ── did this branch land as a squash merge? ──────────────────────────────────
# A squash merge lands the change as ONE new commit on dev; the branch's own
# commits never become ancestors, so ancestry (`rev-list origin/dev..`) calls
# every squash-merged worktree unmerged forever. The patch is the proof here:
# a commit on origin/dev since the fork whose patch-id equals that of the
# worktree's whole diff from the fork to HEAD proves the content landed and
# nothing was committed after the merge. No such commit = not proven = refuse.
squash_merged() {
  local wt="$1" fork patch
  fork=$(git -C "$wt" merge-base origin/dev HEAD 2>/dev/null) || return 1
  patch=$(git -C "$wt" diff "$fork" HEAD | git patch-id --stable | cut -d' ' -f1)
  [ -n "$patch" ] || return 1
  git -C "$wt" log -p "$fork..origin/dev" | git patch-id --stable | cut -d' ' -f1 | grep -qxF "$patch"
}

echo "Worktrees whose work is fully in origin/dev:"
found=0
# Process substitution, not a pipe: a piped `while` runs in a subshell, so
# `found` would never survive the loop and the summary would always say "none".
while read -r wt; do
  [ "$wt" = "$PRIMARY" ] && continue
  [ -d "$wt" ] || continue
  branch=$(git -C "$wt" branch --show-current 2>/dev/null)
  [ -n "$branch" ] || continue

  dirty=$(git -C "$wt" status --porcelain 2>/dev/null | wc -l)
  ahead=$(git -C "$wt" rev-list --count origin/dev..HEAD 2>/dev/null || echo 1)
  if ! live_pids=$(pids_under "$wt"); then
    echo "- SKIP $(basename "$wt") — process references could not be inspected"; continue
  fi
  live=$(wc -w <<< "$live_pids")

  # The refusals that make this safe to run without reading the code.
  if [ "$ahead" != "0" ] && ! squash_merged "$wt"; then
    echo "- SKIP $branch — $ahead commit(s) not in dev, no commit on dev with this patch"; continue
  fi
  # A worktree parked at or behind dev HEAD holds no commit of its own and is
  # indistinguishable from a merged one by ancestry — but every merge is a
  # squash commit, so delivered work always leaves the worktree ahead of dev.
  # One at dev is a freshly created worktree someone is about to work in; one
  # was swept mid-setup exactly this way (2026-08-26). Not this script's to reap.
  if [ "$ahead" = "0" ]; then
    echo "- SKIP $branch — no commit of its own; a just-created worktree looks merged"; continue
  fi
  if [ "$dirty" != "0" ]; then
    echo "- SKIP $(basename "$wt") — $dirty uncommitted change(s)"; continue
  fi
  if [ "$live" != "0" ] && [ "$FORCE" != 1 ]; then
    echo "- SKIP $(basename "$wt") — $live live process(es); stop them or pass --force"; continue
  fi

  found=1
  echo "- $(basename "$wt")  [$branch]  $(du -sh "$wt" 2>/dev/null | cut -f1)"
  if [ "$live" != "0" ]; then
    say "killing $live process(es) under it (--force)"
    [ "$YES" = 1 ] && { kill -TERM $(pids_under "$wt") 2>/dev/null; sleep 2; kill -KILL $(pids_under "$wt") 2>/dev/null; }
  fi
  say "git worktree remove --force $wt"
  run git worktree remove --force "$wt"
  # -D, not -d: git's own merged-ness test is ancestry, so it refuses a
  # squash-merged branch — the proof of delivery already happened above.
  say "git branch -D $branch"
  run git branch -D "$branch"
done < <(git worktree list --porcelain | awk '/^worktree /{print $2}')
[ "$found" = 0 ] && echo "  none"

run git worktree prune
echo
[ "$YES" = 1 ] || echo "Nothing was changed. Re-run with --yes to apply."
