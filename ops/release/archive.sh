#!/usr/bin/env bash
# Pack a built release into the archive its channel downloads — the desktop
# app's (--kind app, the default) or the node's (--kind node).
#
# Takes the release as its build left it and writes ONE archive named by its
# own content and the platform it runs on, the shape `app_update::layout`
# fixes (crates/app-update/src/layout.rs): `Ducktape-<sha7>-<os>-<arch>.tar.zst`
# for the app, `ducktape-<sha7>-<os>-<arch>.tar.zst` for the node.
# `<os>-<arch>` are Rust's names (`macos`/`linux`, `aarch64`/`x86_64`), what
# the running app keys the manifest's artifact map on.
#
#   ops/release/archive.sh --from target/app-bundle/Ducktape.app   # macOS app
#   ops/release/archive.sh --from target/app-release               # Linux app
#   ops/release/archive.sh --kind node --from target/release       # the node
#   ops/release/archive.sh --kind node --from target/release \
#       --sequence 3 --display "2026.09.3+e6352411a"
#
# What the archive holds, at its root, is what the launcher on that side
# extracts into `releases/<sha>/`:
#   app, macOS   Ducktape.app/                             (the whole bundle)
#   app, Linux   ducktape-launcher, ducktape-app
#   node         ducktape, ducktape-node-launcher, modules/
#
# AN APP RELEASE CARRIES NO VIEW. Every view the app draws is founded into a
# network's genesis and served by it, so a `views` directory in an app
# release is a second copy the network never pinned — refused as
# `views_in_app_release`. The node archive's `modules/` carries every basic
# view (crates/topology/basic-views) as `<id>.view.wasm`, or is refused as
# `founding_view_missing: <id>` — the refusal `node init` makes of that set.
#   with --sequence and --display, also `release.json`:
#                {"sequence":3,"display":"2026.09.3+e6352411a"}
#
# `release.json` is the archive's identity (`app_update::ReleaseIdentity`):
# an archive is named by its own sha256 and so can never carry it, but it can
# carry its sequence, and an install made from the extracted directory takes
# that sequence as its pin — the channel publishing that same sequence is then
# the release it already runs. `ops/release/publish.sh` refuses an archive
# whose `release.json` disagrees with its own --sequence/--display.
#
# THE NODE ARCHIVE IS NOT JUST THE BINARY. A node founds and joins from a
# FOUNDING SET of wasm files it reads at runtime — no binary carries one — and
# `workspace_config::modules_dir()` resolves that set as `modules/` beside the
# executable. `--from` is the profile directory `cargo build --release` wrote,
# which holds both binaries and the set that build staged under this
# checkout's name (`modules%<checkout path>`); the set is packed under the one
# name the resolver looks for. A release that shipped the binary alone leaves
# a stranger with a `node init` that cannot found anything.
#
# macOS managed CLI releases accept valid ad-hoc and Developer ID signatures.
# The launcher checks `codesign --verify --deep --strict` on the extracted
# bundle; the signed release channel and archive digest establish trust.
# Packing checks the same integrity contract without modifying the bundle or
# requiring a notarization ticket. This is not Finder/DMG launch assessment.
#
# Prints the archive path, its sha256 and size; records `<os>-<arch>=<path>`
# in `<out-dir>/archives-<kind>.txt` (one line per platform, a rerun replaces
# its own line), which is what `ops/release/publish.sh --archive` takes.
# The manifest's sha256/size are recomputed by `ducktape release manifest`;
# the ones printed here name the file and are for the eye.
set -euo pipefail

FROM=""
KIND="app"
OUT_DIR="${RELEASE_ARCHIVE_DIR:-target/release-archive}"
SEQUENCE=""
DISPLAY_TEXT=""

usage() {
  sed -n '2,61p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --from) FROM="$2"; shift 2 ;;
    --kind) KIND="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --sequence) SEQUENCE="$2"; shift 2 ;;
    --display) DISPLAY_TEXT="$2"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "archive.sh: unknown argument $1" >&2; usage ;;
  esac
done
case "$KIND" in
  app|node) ;;
  *) echo "archive.sh: --kind takes app or node, not $KIND" >&2; exit 2 ;;
esac
[ -n "$FROM" ] || { echo "archive.sh: --from <built release> is required" >&2; exit 2; }
[ -d "$FROM" ] || { echo "archive.sh: $FROM is not a directory" >&2; exit 1; }
command -v zstd >/dev/null || { echo "archive.sh: zstd is not installed (brew install zstd / apt install zstd)" >&2; exit 1; }
# The identity is both fields or none: the reader refuses half of one. The
# sequence is written as a JSON number and the display as a JSON string, so
# only what needs no escaping in either is taken.
if [ -n "$SEQUENCE$DISPLAY_TEXT" ]; then
  { [ -n "$SEQUENCE" ] && [ -n "$DISPLAY_TEXT" ]; } \
    || { echo "archive.sh: --sequence and --display are given together or not at all" >&2; exit 2; }
  case "$SEQUENCE" in
    *[!0-9]*|0?*) echo "archive.sh: --sequence takes a number without leading zeros, not $SEQUENCE" >&2; exit 2 ;;
  esac
  case "$DISPLAY_TEXT" in
    *[\"\\]*|*[[:cntrl:]]*) echo "archive.sh: --display takes no quote, backslash or control character: $DISPLAY_TEXT" >&2; exit 2 ;;
  esac
fi

case "$(uname -s)" in
  Darwin) OS=macos ;;
  Linux) OS=linux ;;
  *) echo "archive.sh: unsupported host $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=aarch64 ;;
  x86_64) ARCH=x86_64 ;;
  *) echo "archive.sh: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
PLATFORM="$OS-$ARCH"
case "$OS" in
  # bsdtar's spelling of "owners dropped", then GNU tar's. Owners are dropped
  # so the bytes do not depend on who built them. GNU tar also drops what the
  # build and the copy stamp on a member — mtime, directory read order, the
  # packer's umask — so one commit packs to one sha256 (`publish.sh
  # --verified-sha` compares two builds); bsdtar has no spelling for those,
  # and a macOS archive records its copy times.
  macos) TAR_NORMAL=(--uid 0 --gid 0 --numeric-owner) ;;
  linux) TAR_NORMAL=(--owner=0 --group=0 --numeric-owner --format=gnu --sort=name --mtime=@0 --mode=u=rwX,go=rX) ;;
esac

refuse() { echo "archive.sh: $1: $2" >&2; exit 1; }

require_executable() {
  if ! [ -f "$1" ] || ! [ -x "$1" ]; then
    echo "archive.sh: $2: $1 is not an executable file" >&2
    exit 1
  fi
}
refuse_views() {
  local views
  for views in "$@"; do
    [ ! -e "$views" ] && [ ! -L "$views" ] \
      || refuse views_in_app_release "$views: the network serves every view out of its genesis; an app release carries none"
  done
}

# The founding set THIS checkout's build staged beside the binaries in $1:
# the directory named for the checkout this script sits in (several checkouts
# share one target directory, so a set carries its checkout in its name —
# `/` written `%`, as crates/workspace-config/src/staged_key.rs encodes it and
# as the Makefile spells it), else the unkeyed `modules` an install leaves.
# Nothing in the profile directory is read to choose it: every checkout's
# build writes there. Sets FOUNDING_SET.
resolve_founding_set() {
  local from="$1" checkout
  checkout=$(cd "$(dirname "$0")/../.." && pwd -P)
  FOUNDING_SET="$from/modules$(printf '%s' "$checkout" | tr / %)"
  [ -d "$FOUNDING_SET" ] || FOUNDING_SET="$from/modules"
  [ -d "$FOUNDING_SET" ] \
    || refuse founding_set_missing "$FOUNDING_SET is not a directory; 'cargo build --release' stages the set beside the binaries it links"
  ls "$FOUNDING_SET"/*.component.wasm >/dev/null 2>&1 \
    || refuse founding_set_incomplete "$FOUNDING_SET holds no <id>.component.wasm"
  [ -f "$FOUNDING_SET/netstack.component.wasm" ] \
    || refuse founding_set_incomplete "$FOUNDING_SET holds no netstack.component.wasm, so a node unpacking this release could not reach the mesh"
  # The set records which build staged it, and the binary beside it refuses a
  # set another build wrote (noded::services::founding_set). Packing a set
  # with no record ships a release that refuses itself on first use.
  [ -f "$FOUNDING_SET/.staged-by" ] \
    || refuse founding_set_unowned "$FOUNDING_SET carries no .staged-by, so the binary beside it will refuse the set as a stranger's"
  local id
  for id in $(cat "$checkout/crates/topology/basic-views"); do
    [ -f "$FOUNDING_SET/$id.view.wasm" ] \
      || refuse founding_view_missing "$id — $FOUNDING_SET holds no $id.view.wasm, so 'node init' from this release would refuse it"
  done
}

# Match the managed launcher's code-integrity check without changing its input.
refuse_unfit_bundle() {
  local bundle="$1"
  codesign --verify --deep --strict "$bundle" || { echo "archive.sh: codesign_refused: $bundle" >&2; exit 1; }
}

pack_app() {
  PREFIX=Ducktape
  case "$OS" in
    macos)
      [ "$(basename "$FROM")" = Ducktape.app ] || { echo "archive.sh: --from must name Ducktape.app on macOS, not $FROM" >&2; exit 1; }
      require_executable "$FROM/Contents/MacOS/ducktape-launcher" launcher_missing
      require_executable "$FROM/Contents/MacOS/ducktape-app" app_missing
      refuse_views "$FROM/Contents/MacOS/views" "$FROM/Contents/Resources/views"
      refuse_unfit_bundle "$FROM"
      PARENT=$(cd "$FROM/.." && pwd -P)
      MEMBERS=(Ducktape.app)
      ;;
    linux)
      require_executable "$FROM/ducktape-launcher" launcher_missing
      require_executable "$FROM/ducktape-app" app_missing
      refuse_views "$FROM/views"
      PARENT=$(cd "$FROM" && pwd -P)
      MEMBERS=(ducktape-launcher ducktape-app)
      ;;
  esac
}

# The node release: both binaries plus the founding set, staged under the ONE
# name `workspace_config::modules_dir()` resolves beside an executable. The
# set is copied rather than packed in place because a built set carries its
# checkout in its directory name and an unpacked release must not.
pack_node() {
  PREFIX=ducktape
  require_executable "$FROM/ducktape" node_missing
  require_executable "$FROM/ducktape-node-launcher" launcher_missing
  resolve_founding_set "$FROM"
  STAGE="$SCRATCH/node"
  mkdir "$STAGE"
  cp "$FROM/ducktape" "$FROM/ducktape-node-launcher" "$STAGE/"
  cp -R "$FOUNDING_SET" "$STAGE/modules"
  PARENT="$STAGE"
  MEMBERS=(ducktape ducktape-node-launcher modules)
  echo "founding set: $FOUNDING_SET (staged by $(cat "$FOUNDING_SET/.staged-by"))" >&2
}

SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT
case "$KIND" in
  app) pack_app ;;
  node) pack_node ;;
esac
# The identity rides at the root beside the members, written here rather
# than into --from.
if [ -n "$SEQUENCE" ]; then
  printf '{"sequence":%s,"display":"%s"}\n' "$SEQUENCE" "$DISPLAY_TEXT" >"$SCRATCH/release.json"
  MEMBERS+=(-C "$SCRATCH" release.json)
fi

mkdir -p "$OUT_DIR"
OUT_DIR=$(cd "$OUT_DIR" && pwd -P)
PARTIAL="$OUT_DIR/$PREFIX-$PLATFORM.tar.zst.partial"
# No resource forks or xattrs (a quarantine flag must never ride inside a
# release); owners dropped so the bytes do not depend on who built them.
COPYFILE_DISABLE=1 tar -C "$PARENT" "${TAR_NORMAL[@]}" -cf - "${MEMBERS[@]}" \
  | zstd -q -T0 -19 -f -o "$PARTIAL"
if command -v sha256sum >/dev/null; then
  SHA=$(sha256sum "$PARTIAL" | cut -d' ' -f1)
else
  SHA=$(shasum -a 256 "$PARTIAL" | cut -d' ' -f1)
fi
SIZE=$(wc -c <"$PARTIAL" | tr -d ' ')
ARCHIVE="$OUT_DIR/$PREFIX-${SHA:0:7}-$PLATFORM.tar.zst"
mv -f "$PARTIAL" "$ARCHIVE"
# One line per platform, per kind: a rerun for this platform replaces its line
# and leaves the other platforms' archives listed.
LIST="$OUT_DIR/archives-$KIND.txt"
touch "$LIST"
grep -v "^$PLATFORM=" "$LIST" >"$LIST.tmp" || true
echo "$PLATFORM=$ARCHIVE" >>"$LIST.tmp"
mv -f "$LIST.tmp" "$LIST"
echo "archive: $ARCHIVE"
echo "sha256:  $SHA"
echo "size:    $SIZE"
