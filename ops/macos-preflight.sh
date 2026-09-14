#!/usr/bin/env bash
# Everything a Mac needs BEFORE it can run the compute plane (airlock +
# provider runs under the vz sandbox), checked in one pass.
#
#   ops/macos-preflight.sh                       # report
#   ops/macos-preflight.sh --prompt              # …and offer to run the install steps (tty only)
#
# Reports EVERY missing prerequisite with the exact command that installs it,
# instead of the boot probe's one-refusal-at-a-time loop — the probe's job is
# refusing an unready node loudly, this script's job is getting the install
# done in one sitting. With --prompt (what `make dev` passes) the runnable
# steps are offered as a single "install now?" question; what cannot be run
# for the operator (Homebrew itself, rustup itself, a non-VM Mac) is only
# ever printed. Exit 0 = the host half of the node's sandbox probe will pass;
# the guest images are a WORKSPACE's own (`<workspace>/guest`, built by
# `OUT=<workspace>/guest ops/build-guest-rootfs.sh` — `make dev` builds the
# demo's), so no image is checked here.
set -uo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"

# brew lives outside a non-login shell's PATH (ssh commands, make, cron), so
# look for it at the standard prefixes rather than reporting a Homebrew that
# is in fact installed as missing.
for prefix in /opt/homebrew /usr/local; do
  [[ -x "$prefix/bin/brew" ]] && PATH="$PATH:$prefix/bin:$prefix/sbin"
done
PROMPT=0
[[ "${1:-}" == "--prompt" ]] && PROMPT=1

MISSING=0
FIX_CMDS=()
ok()   { printf '  ok    %s\n' "$1"; }
# miss <what> <fix-text> [runnable-cmd] — a non-empty third argument marks the
# fix as something this script may run for the operator under --prompt.
miss() {
  printf '  MISS  %s\n        fix: %s\n' "$1" "$2"
  MISSING=1
  [[ $# -ge 3 && -n "$3" ]] && FIX_CMDS+=("$3")
}
# note <what> <fix-text> — the same shape as miss(), for a section that is NOT
# a sandbox prerequisite. Nothing here can fail the run: this script's exit
# code answers "will the node's sandbox probe pass", and `make dev` gates on
# it, so a dev Mac with no Developer ID must still exit 0.
note() { printf '  n/a   %s\n        fix: %s\n' "$1" "$2"; }

[[ "$(uname -s)" == "Darwin" ]] || { echo "this preflight is for macOS; on Linux the probe wants firecracker + kvm" >&2; exit 1; }

echo "duck macOS preflight"

# ---- the machine ------------------------------------------------------------
if [[ "$(sysctl -n kern.hv_support 2>/dev/null)" == "1" ]]; then
  ok "Hypervisor.framework (kern.hv_support)"
else
  miss "Hypervisor.framework support" "this Mac cannot host VMs (Apple silicon or VT-x required); no install fixes this"
fi

if xcode-select -p >/dev/null 2>&1; then
  ok "Xcode command line tools"
else
  # runnable, but ASYNC: it opens Apple's install dialog and returns at once,
  # so the re-check after an install round can still miss it — finish the
  # dialog and re-run.
  miss "Xcode command line tools (swift + cc)" "xcode-select --install" "xcode-select --install"
fi

# ---- homebrew and the image tools ------------------------------------------
if command -v brew >/dev/null; then
  PREFIX="$(brew --prefix)"
  ok "Homebrew ($PREFIX)"
  # the node resolves keg-only tools from the STANDARD prefixes only
  # (sandbox host_tools.rs); a custom prefix passes here and fails there.
  case "$PREFIX" in
    /opt/homebrew|/usr/local) ;;
    *) miss "standard Homebrew prefix" "the node looks for keg-only tools under /opt/homebrew or /usr/local; a custom prefix ($PREFIX) is not searched" ;;
  esac
  if [[ -x "$PREFIX/opt/e2fsprogs/sbin/mke2fs" && -x "$PREFIX/opt/e2fsprogs/sbin/debugfs" ]]; then
    ok "e2fsprogs (mke2fs + debugfs, keg-only)"
  else
    miss "e2fsprogs (builds and reads each run's workspace image)" "brew install e2fsprogs" "brew install e2fsprogs"
  fi
  if command -v unsquashfs >/dev/null || [[ -x "$PREFIX/bin/unsquashfs" ]]; then
    ok "squashfs (extracts the guest base image)"
  else
    miss "squashfs (guest rootfs build)" "brew install squashfs" "brew install squashfs"
  fi
  if command -v zstd >/dev/null || [[ -x "$PREFIX/bin/zstd" ]]; then
    ok "zstd (unpacks the Kata VM kernel bundle)"
  else
    miss "zstd (guest kernel fetch)" "brew install zstd" "brew install zstd"
  fi
else
  miss "Homebrew" 'see https://brew.sh — then: brew install e2fsprogs squashfs zstd'
fi

# ---- rust -------------------------------------------------------------------
if command -v rustup >/dev/null; then
  ok "rustup"
  # rust-toolchain.toml lists this target, so installing the pinned toolchain
  # from inside the checkout is what puts it there — the manual `target add` is
  # only for a rustup too old to read `targets` out of that file.
  if rustup target list --installed 2>/dev/null | grep -q aarch64-unknown-linux-musl; then
    ok "aarch64-unknown-linux-musl target (guest init cross build)"
  else
    miss "musl target for the guest init" "rustup toolchain install (from the ducktape checkout)" \
      "cd '$HERE' && rustup toolchain install"
  fi
else
  miss "rustup" "see https://rustup.rs — then: rustup toolchain install (in the ducktape checkout)"
fi

# ---- the shim ---------------------------------------------------------------
# installed into brew's bin when we install it ourselves: it is on PATH for a
# brew user, writable without sudo, and deterministic — "<dir-on-PATH>" is for
# an operator who wants it elsewhere.
SHIM_INSTALL_CMD=""
command -v brew >/dev/null && SHIM_INSTALL_CMD="INSTALL=\"$(brew --prefix)/bin\" bash \"$HERE/bin/duck-vz-shim/build.sh\""
if SHIM="$(command -v duck-vz-shim)"; then
  if codesign --display --entitlements - --xml "$SHIM" 2>&1 | grep -q com.apple.security.virtualization; then
    ok "duck-vz-shim on PATH, virtualization entitlement signed"
  else
    miss "duck-vz-shim entitlement (Virtualization.framework refuses it unsigned)" \
      "INSTALL=<dir-on-PATH> bin/duck-vz-shim/build.sh" "$SHIM_INSTALL_CMD"
  fi
else
  miss "duck-vz-shim on PATH (the macOS VMM)" "INSTALL=<dir-on-PATH> bin/duck-vz-shim/build.sh" "$SHIM_INSTALL_CMD"
fi

# ---- release signing (informational) ----------------------------------------
# Not a prerequisite for running anything locally: `make app` ad-hoc signs, and
# an ad-hoc bundle runs on the machine that built it. It is a prerequisite for
# a bundle that leaves this Mac, and the two halves fail far apart (Gatekeeper
# on someone else's machine, or a notarytool rejection after the upload), so
# both are reported here. The recipe is app/README.md "Release build".
echo
echo "release signing (only for a bundle that leaves this Mac):"
DEVELOPER_ID="$(security find-identity -v -p codesigning 2>/dev/null | grep 'Developer ID Application' || true)"
if [[ -n "$DEVELOPER_ID" ]]; then
  # one line per identity, quoted name only — that string is what
  # DUCKTAPE_CODESIGN_IDENTITY takes.
  while IFS= read -r line; do
    ok "codesigning identity: ${line#*\"}"
  done <<< "$(printf '%s\n' "$DEVELOPER_ID" | sed 's/"$//')"
else
  note "no \"Developer ID Application\" identity in the keychain" \
    "enroll in the Apple Developer Program, then download the certificate; without one \`make app\` signs ad-hoc"
fi

if [[ -n "${DUCKTAPE_NOTARY_KEY:-}" && -n "${DUCKTAPE_NOTARY_KEY_ID:-}" && -n "${DUCKTAPE_NOTARY_ISSUER:-}" ]]; then
  if [[ -f "$DUCKTAPE_NOTARY_KEY" ]]; then
    ok "notary credentials (DUCKTAPE_NOTARY_KEY exists, KEY_ID and ISSUER set)"
  else
    note "DUCKTAPE_NOTARY_KEY points at no file ($DUCKTAPE_NOTARY_KEY)" \
      "point it at the App Store Connect API key .p8, stored outside this repo"
  fi
else
  note "notary credentials not exported" \
    "export DUCKTAPE_NOTARY_KEY=<path to .p8> DUCKTAPE_NOTARY_KEY_ID=<id> DUCKTAPE_NOTARY_ISSUER=<uuid>; all three or none"
fi

echo
if [[ "$MISSING" == 0 ]]; then
  echo "ready: the host half of the sandbox probe will pass. Build a workspace's"
  echo "guest (OUT=<workspace>/guest ops/build-guest-rootfs.sh) and smoke it with:"
  echo "  cargo run -p sandbox-host --example vm_smoke -- \\"
  echo "      --kernel <workspace>/guest/vmlinux --rootfs <workspace>/guest/rootfs.ext4"
  exit 0
fi

# ---- the install offer ------------------------------------------------------
# Only under --prompt, only on a tty, and only for steps this script may run.
# The prompt is the whole point of the flag: `make dev` asks ONCE, runs the
# accepted steps in dependency order (the checks above are ordered that way),
# then re-checks so the operator sees the fresh state, not a stale promise.
runnable=${#FIX_CMDS[@]}
if [[ "$PROMPT" == 1 && -t 0 && "$runnable" -gt 0 ]]; then
  echo "installable now:"
  for cmd in "${FIX_CMDS[@]}"; do printf '  $ %s\n' "$cmd"; done
  printf 'install these now? [Y/n] '
  read -r answer
  case "$answer" in
    n|N|no|NO) ;;
    *)
      for cmd in "${FIX_CMDS[@]}"; do
        printf '\n\033[36m[preflight]\033[0m %s\n' "$cmd"
        bash -c "$cmd" || { echo "install step failed — fix the error above and re-run" >&2; exit 1; }
      done
      echo
      exec bash "$0"
      ;;
  esac
fi
echo "install the missing pieces above, then re-run this script."
exit 1
