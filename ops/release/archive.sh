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
#
# What the archive holds, at its root, is what the launcher on that side
# extracts into `releases/<sha>/`:
#   app, macOS   Ducktape.app/                             (the whole bundle)
#   app, Linux   ducktape-launcher, ducktape-app, views/*.wasm
#   node         ducktape, ducktape-node-launcher, modules/
#
# THE NODE ARCHIVE IS NOT JUST THE BINARY. A node founds and joins from a
# FOUNDING SET of wasm files it reads at runtime — no binary carries one — and
# `workspace_config::modules_dir()` resolves that set as `modules/` beside the
# executable. `--from` is the profile directory `cargo build --release` wrote,
# which holds both binaries and the set that build staged (the name its
# `.staged-modules` pointer holds); the set is packed under the one name the
# resolver looks for. A release that shipped the binary alone leaves a
# stranger with a `node init` that cannot found anything.
#
# macOS REFUSES to pack a bundle that is not fit to leave this machine: an
# ad-hoc signature (`adhoc_bundle_refused` — Gatekeeper rejects it anywhere
# else) or no notarization ticket (`bundle_not_stapled` — the ticket is
# stapled here when the bundle was notarized but not yet stapled, and a
# bundle Apple never notarized is refused). `ducktape-launcher --qualify`
# runs `codesign --verify --deep --strict` + `spctl -a -t exec` on the
# extracted bundle before it flips, so a release must pass both here.
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

usage() {
  sed -n '2,42p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --from) FROM="$2"; shift 2 ;;
    --kind) KIND="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
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
  # so the bytes do not depend on who built them.
  macos) TAR_OWNER=(--uid 0 --gid 0 --numeric-owner) ;;
  linux) TAR_OWNER=(--owner=0 --group=0 --numeric-owner) ;;
esac

refuse() { echo "archive.sh: $1: $2" >&2; exit 1; }

require_executable() {
  if ! [ -f "$1" ] || ! [ -x "$1" ]; then
    echo "archive.sh: $2: $1 is not an executable file" >&2
    exit 1
  fi
}
require_views() {
  ls "$1"/*.wasm >/dev/null 2>&1 || { echo "archive.sh: views_missing: $1 holds no .wasm" >&2; exit 1; }
}

# The founding set the build that linked the binaries in $1 staged beside
# them: the directory that profile's `.staged-modules` pointer names (several
# checkouts share one target directory, so the set carries its checkout in its
# name), else the unkeyed `modules` an install leaves. Sets FOUNDING_SET.
resolve_founding_set() {
  local from="$1" pointed
  pointed=$(cat "$from/.staged-modules" 2>/dev/null || true)
  FOUNDING_SET="$from/${pointed:-modules}"
  [ -d "$FOUNDING_SET" ] \
    || refuse founding_set_missing "$FOUNDING_SET is not a directory; 'cargo build --release' stages the set beside the binaries it links"
  ls "$FOUNDING_SET"/*.component.wasm >/dev/null 2>&1 \
    || refuse founding_set_incomplete "$FOUNDING_SET holds no <id>.component.wasm"
  [ -f "$FOUNDING_SET/netstack.component.wasm" ] \
    || refuse founding_set_incomplete "$FOUNDING_SET holds no netstack.component.wasm, so a node unpacking this release could not reach the mesh"
  ! ls "$FOUNDING_SET"/*.view.pending >/dev/null 2>&1 \
    || refuse views_pending "$FOUNDING_SET carries a .view.pending marker; a genesis cannot be composed from it (run 'make views' and rebuild)"
  # The set records which build staged it, and the binary beside it refuses a
  # set another build wrote (noded::services::founding_set). Packing a set
  # with no record ships a release that refuses itself on first use.
  [ -f "$FOUNDING_SET/.staged-by" ] \
    || refuse founding_set_unowned "$FOUNDING_SET carries no .staged-by, so the binary beside it will refuse the set as a stranger's"
}

# The bundle's own checks, on macOS only: signature kind, then the ticket.
refuse_unfit_bundle() {
  local bundle="$1"
  local signature
  signature=$(codesign -dv "$bundle" 2>&1 | sed -n 's/^Signature=//p')
  if [ "$signature" = adhoc ] || [ -z "$signature" ]; then
    echo "archive.sh: adhoc_bundle_refused: $bundle is ad-hoc signed; build it with DUCKTAPE_CODESIGN_IDENTITY (make app-release)" >&2
    exit 1
  fi
  codesign --verify --deep --strict "$bundle" || { echo "archive.sh: codesign_refused: $bundle" >&2; exit 1; }
  if ! xcrun stapler validate "$bundle" >/dev/null 2>&1; then
    echo "archive.sh: no ticket stapled to $bundle; stapling" >&2
    xcrun stapler staple "$bundle" || { echo "archive.sh: bundle_not_stapled: $bundle was never notarized (set the three DUCKTAPE_NOTARY_* and rebuild)" >&2; exit 1; }
  fi
  spctl -a -t exec "$bundle" || { echo "archive.sh: gatekeeper_refused: $bundle" >&2; exit 1; }
}

pack_app() {
  PREFIX=Ducktape
  case "$OS" in
    macos)
      [ "$(basename "$FROM")" = Ducktape.app ] || { echo "archive.sh: --from must name Ducktape.app on macOS, not $FROM" >&2; exit 1; }
      require_executable "$FROM/Contents/MacOS/ducktape-launcher" launcher_missing
      require_executable "$FROM/Contents/MacOS/ducktape-app" app_missing
      require_views "$FROM/Contents/MacOS/views"
      refuse_unfit_bundle "$FROM"
      PARENT=$(cd "$FROM/.." && pwd -P)
      MEMBERS=(Ducktape.app)
      ;;
    linux)
      require_executable "$FROM/ducktape-launcher" launcher_missing
      require_executable "$FROM/ducktape-app" app_missing
      require_views "$FROM/views"
      PARENT=$(cd "$FROM" && pwd -P)
      MEMBERS=(ducktape-launcher ducktape-app views)
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
  STAGE=$(mktemp -d)
  trap 'rm -rf "$STAGE"' EXIT
  cp "$FROM/ducktape" "$FROM/ducktape-node-launcher" "$STAGE/"
  cp -R "$FOUNDING_SET" "$STAGE/modules"
  PARENT="$STAGE"
  MEMBERS=(ducktape ducktape-node-launcher modules)
  echo "founding set: $FOUNDING_SET (staged by $(cat "$FOUNDING_SET/.staged-by"))" >&2
}

case "$KIND" in
  app) pack_app ;;
  node) pack_node ;;
esac

mkdir -p "$OUT_DIR"
OUT_DIR=$(cd "$OUT_DIR" && pwd -P)
PARTIAL="$OUT_DIR/$PREFIX-$PLATFORM.tar.zst.partial"
# No resource forks or xattrs (a quarantine flag must never ride inside a
# release); owners dropped so the bytes do not depend on who built them.
COPYFILE_DISABLE=1 tar -C "$PARENT" "${TAR_OWNER[@]}" -cf - "${MEMBERS[@]}" \
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
