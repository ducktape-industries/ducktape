#!/usr/bin/env bash
# Pack a built desktop-app release into the archive the app downloads.
#
# Takes the release as `make app` leaves it and writes ONE archive named by
# its own content and the platform it runs on, the shape `app_update::layout`
# fixes (crates/app-update/src/layout.rs): `Ducktape-<sha7>-<os>-<arch>.tar.zst`.
# `<os>-<arch>` are Rust's names (`macos`/`linux`, `aarch64`/`x86_64`), what
# the running app keys the manifest's artifact map on.
#
#   ops/release/archive.sh --from target/app-bundle/Ducktape.app   # macOS
#   ops/release/archive.sh --from target/app-release               # Linux
#
# What the archive holds, at its root, is what `ducktape-launcher install
# --from` takes and what the app extracts into `releases/<sha>/`:
#   macOS   Ducktape.app/                                   (the whole bundle)
#   Linux   ducktape-launcher, ducktape-app, views/*.wasm
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
# in `<out-dir>/archives.txt` (one line per platform, a rerun replaces its
# own line), which `make publish-app` reads as ARCHIVES.
# The manifest's sha256/size are recomputed by `ducktape release manifest`;
# the ones printed here name the file and are for the eye.
set -euo pipefail

FROM=""
OUT_DIR="${RELEASE_ARCHIVE_DIR:-target/release-archive}"

usage() {
  sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --from) FROM="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "archive.sh: unknown argument $1" >&2; usage ;;
  esac
done
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

require_executable() {
  if ! [ -f "$1" ] || ! [ -x "$1" ]; then
    echo "archive.sh: $2: $1 is not an executable file" >&2
    exit 1
  fi
}
require_views() {
  ls "$1"/*.wasm >/dev/null 2>&1 || { echo "archive.sh: views_missing: $1 holds no .wasm" >&2; exit 1; }
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

case "$OS" in
  macos)
    [ "$(basename "$FROM")" = Ducktape.app ] || { echo "archive.sh: --from must name Ducktape.app on macOS, not $FROM" >&2; exit 1; }
    require_executable "$FROM/Contents/MacOS/ducktape-launcher" launcher_missing
    require_executable "$FROM/Contents/MacOS/ducktape-app" app_missing
    require_views "$FROM/Contents/MacOS/views"
    refuse_unfit_bundle "$FROM"
    PARENT=$(cd "$FROM/.." && pwd -P)
    MEMBERS=(Ducktape.app)
    # bsdtar's spelling of "owners dropped".
    TAR_OWNER=(--uid 0 --gid 0 --numeric-owner)
    ;;
  linux)
    require_executable "$FROM/ducktape-launcher" launcher_missing
    require_executable "$FROM/ducktape-app" app_missing
    require_views "$FROM/views"
    PARENT=$(cd "$FROM" && pwd -P)
    MEMBERS=(ducktape-launcher ducktape-app views)
    # GNU tar's spelling.
    TAR_OWNER=(--owner=0 --group=0 --numeric-owner)
    ;;
esac

mkdir -p "$OUT_DIR"
OUT_DIR=$(cd "$OUT_DIR" && pwd -P)
PARTIAL="$OUT_DIR/Ducktape-$PLATFORM.tar.zst.partial"
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
ARCHIVE="$OUT_DIR/Ducktape-${SHA:0:7}-$PLATFORM.tar.zst"
mv -f "$PARTIAL" "$ARCHIVE"
# One line per platform: a rerun for this platform replaces its line and
# leaves the other platforms' archives listed.
LIST="$OUT_DIR/archives.txt"
touch "$LIST"
grep -v "^$PLATFORM=" "$LIST" >"$LIST.tmp" || true
echo "$PLATFORM=$ARCHIVE" >>"$LIST.tmp"
mv -f "$LIST.tmp" "$LIST"
echo "archive: $ARCHIVE"
echo "sha256:  $SHA"
echo "size:    $SIZE"
