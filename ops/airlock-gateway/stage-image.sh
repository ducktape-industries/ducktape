#!/usr/bin/env bash
# Stage the airlock gateway image root: everything the enclave binary needs
# on its filesystem, laid out under one directory a confidential-VM image
# build copies in as-is.
#
#   ops/airlock-gateway/stage-image.sh [--out <dir>] [--dry-run]
#
#   <out>/usr/local/bin/airlock-gateway                       the enclave binary (release, --locked)
#   <out>/usr/local/bin/rcodesign                             the pinned signer (install-rcodesign.sh)
#   <out>/usr/local/share/airlock-gateway/entitlements.plist  app/packaging/entitlements.plist
#
# The three paths are the binary's defaults (`--rcodesign`, `--entitlements`
# in bin/airlock-gateway/src/main.rs), so a gateway booted from this root
# serves `POST /sign/macos-bundle` with no flags. `--out` defaults to
# target/airlock-gateway-image; `--dry-run` prints the steps and runs none.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
out="$repo/target/airlock-gateway-image"
dry_run=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) out=$2; shift 2 ;;
    --out=*) out=${1#--out=}; shift ;;
    --dry-run) dry_run=1; shift ;;
    *) echo "stage-image.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done

release_bin="${CARGO_TARGET_DIR:-$repo/target}/release"
steps=(
  "${CARGO:-cargo} build --locked --release -p airlock-gateway"
  "install -D -m 0755 $release_bin/airlock-gateway $out/usr/local/bin/airlock-gateway"
  "$repo/ops/airlock-gateway/install-rcodesign.sh --prefix $out/usr/local"
  "install -D -m 0644 $repo/app/packaging/entitlements.plist $out/usr/local/share/airlock-gateway/entitlements.plist"
)
if [[ $dry_run == 1 ]]; then
  printf '%s\n' "${steps[@]}"
  "$repo/ops/airlock-gateway/install-rcodesign.sh" --prefix "$out/usr/local" --dry-run | sed 's/^/  /'
  exit 0
fi
cd "$repo"
for step in "${steps[@]}"; do
  echo "+ $step"
  eval "$step"
done
echo "staged $out"
