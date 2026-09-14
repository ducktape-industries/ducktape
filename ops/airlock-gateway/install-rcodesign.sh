#!/usr/bin/env bash
# Install the pinned `rcodesign` (apple-codesign) release the airlock gateway
# signs with (`crates/airlock/src/sign.rs`): download the musl tarball for
# this machine's architecture, check it against the SHA-256 pinned here, and
# put the one executable at <prefix>/bin/rcodesign.
#
#   ops/airlock-gateway/install-rcodesign.sh [--prefix <dir>] [--dry-run]
#
# `--prefix` defaults to /usr/local (the path `bin/airlock-gateway` looks in);
# `--dry-run` prints what would be fetched and where, and fetches nothing.
# The pin is the whole point: bumping it is a change to what the enclave
# signs with, made here on purpose, never by a floating "latest".
set -euo pipefail

RCODESIGN_VERSION=0.29.0
# `apple-codesign-<version>-<triple>.tar.gz.sha256` from the release, per triple.
SHA256_X86_64=dbe85cedd8ee4217b64e9a0e4c2aef92ab8bcaaa41f20bde99781ff02e600002
SHA256_AARCH64=4af92c87ddf52f5f2d1258a3b4e56c7dcb8f1b2468df744976c5f139e031961f

prefix=/usr/local
dry_run=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) prefix=$2; shift 2 ;;
    --prefix=*) prefix=${1#--prefix=}; shift ;;
    --dry-run) dry_run=1; shift ;;
    *) echo "install-rcodesign.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done

case "$(uname -m)" in
  x86_64|amd64) triple=x86_64-unknown-linux-musl; sha256=$SHA256_X86_64 ;;
  aarch64|arm64) triple=aarch64-unknown-linux-musl; sha256=$SHA256_AARCH64 ;;
  *) echo "install-rcodesign.sh: no pinned rcodesign for $(uname -m)" >&2; exit 1 ;;
esac
[[ "$(uname -s)" == Linux ]] || { echo "install-rcodesign.sh: the gateway image is Linux; run this on Linux" >&2; exit 1; }

name="apple-codesign-$RCODESIGN_VERSION-$triple"
url="https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F$RCODESIGN_VERSION/$name.tar.gz"
dest="$prefix/bin/rcodesign"

if [[ $dry_run == 1 ]]; then
  echo "fetch  $url"
  echo "sha256 $sha256"
  echo "install $dest"
  exit 0
fi

if [[ -x "$dest" ]] && "$dest" --version 2>/dev/null | grep -q "apple-codesign $RCODESIGN_VERSION"; then
  echo "rcodesign $RCODESIGN_VERSION already at $dest"
  exit 0
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/rcodesign.XXXXXX")
trap 'rm -rf "$work"' EXIT
curl -fsSL --retry 3 -o "$work/$name.tar.gz" "$url"
echo "$sha256  $work/$name.tar.gz" | sha256sum -c - >/dev/null \
  || { echo "install-rcodesign.sh: sha256 mismatch for $name.tar.gz (pinned $sha256)" >&2; exit 1; }
tar -xzf "$work/$name.tar.gz" -C "$work" "$name/rcodesign"
mkdir -p "$prefix/bin"
install -m 0755 "$work/$name/rcodesign" "$dest"
"$dest" --version
echo "installed $dest"
