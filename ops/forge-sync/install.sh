#!/usr/bin/env bash
# Installs the forge-sync user service on this machine: follower.py, the ducktape CLI release bin-COMMIT carries, git-remote-duck, the unit, and its state directory.
# Usage: ops/forge-sync/install.sh COMMIT; the #sync key (files key and password) must already be in ~/.local/state/forge-sync/keys.
set -euo pipefail
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
commit=$(git -C "$here" rev-parse --verify "${1:?usage: ops/forge-sync/install.sh COMMIT}^{commit}")
duck_release=git-remote-duck-7acf453
duck_sha256=c03d7b0d02327af0a1d8eef2d80e778ec8a67850aa42dd2b43022f85269b5b75
share="$HOME/.local/share/forge-sync"
state="$HOME/.local/state/forge-sync"
for file in key password; do
  if [ ! -f "$state/keys/$file" ]; then
    echo "install.sh: $state/keys/$file is missing: the #sync key is account 22's \"forge-sync follower\" key, sealed, and its password" >&2
    exit 1
  fi
done
mkdir -p "$share/bin" "$HOME/.config/systemd/user"
chmod 700 "$state" "$state/keys"
chmod 600 "$state/keys/key" "$state/keys/password"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
gh release download "bin-$commit" -R ducktape-industries/ducktape -p ducktape-x86_64-unknown-linux-gnu.tar.gz -D "$stage"
tar -xzf "$stage/ducktape-x86_64-unknown-linux-gnu.tar.gz" -C "$stage"
curl -fsSL -o "$stage/git-remote-duck" \
  "https://github.com/ducktape-industries/ducktape/releases/download/$duck_release/git-remote-duck-x86_64-unknown-linux-gnu"
echo "$duck_sha256  $stage/git-remote-duck" | sha256sum --check --quiet
install -m 755 "$stage/ducktape" "$stage/git-remote-duck" "$share/bin/"
install -m 644 "$here/follower.py" "$share/follower.py"
install -m 644 "$here/forge-sync.service" "$HOME/.config/systemd/user/forge-sync.service"
systemctl --user daemon-reload
systemctl --user enable forge-sync.service
systemctl --user restart forge-sync.service
