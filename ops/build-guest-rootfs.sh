#!/usr/bin/env bash
# Build one workspace's guest artifacts, which every run of that network's
# microVM boots from: the kernel and one read-only ext4 rootfs.
#
#   OUT=<workspace>/guest ops/build-guest-rootfs.sh
#   ROOTFS_SETUP=/path/to/setup.sh OUT=… ops/build-guest-rootfs.sh [setup arguments]
#     Linux installs the repository's pinned Rust and the wasm-tools CLI of
#     its componentizer's release by default
#     (requires Bubblewrap). ROOTFS_SETUP replaces that setup; an empty value
#     builds only the base image. Arguments are passed to a custom hook.
#
# OUT is required and is a WORKSPACE's guest directory: a guest is per
# workspace (two networks on one box share no image), and the workspace's
# [sandbox] table names no path because `<workspace>/guest` is where its node
# looks. `ducktape node sandbox` prints the exact invocation for a workspace.
# Rootless, so it builds under the operator's own directories.
#
# ROOTLESS on purpose, start to finish. `unsquashfs -no-xattrs` extracts the
# base without needing privileges and `mke2fs -d` builds the image without ever
# mounting it. A node that needs root to build its guest is a node that runs as
# root.
#
# NO AGENT CLI GOES IN, and that is the point: this image is a base, not an
# install target. The CLIs a node lends live in `<workspace>/executors`
# (`ducktape agent install`) and the node derives its own read-only image from
# that directory, mounted at /opt/duck/bin per run. Baking them in here instead
# made this 500 MB build the unit of installation and gave "which CLIs does
# this node have" a third answer that could disagree with the other two.
#
# What goes in:
#   /duck-guest-init      the static PID 1 (bin/duck-guest-init, musl)
#   /duck /agent /opt/duck/bin
#                         empty mountpoints for the per-run block devices
#
# What does NOT go in: any credential. The broker holds those on the host and
# the guest reaches it over vsock, so the image is safe to share across runs
# and across buyers — which is exactly why it can be read-only and shared.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:?set OUT=<workspace>/guest — a guest is built per workspace}"
# The download and extraction cache, under the repository's target dir: not
# under the workspace, which is disposable (a `make dev` lap founds a fresh
# one, and re-fetching 250 MB per lap is not a rebuild), and never under /tmp,
# which on this class of host is both memory-backed and periodically reaped.
WORK="${WORK:-$HERE/target/guest-build}"

if [[ -z "${ROOTFS_SETUP+x}" && "$(uname -s)" == "Linux" ]]; then
  ROOTFS_SETUP="$HERE/ops/guest-rust-tools.sh"
  RUST_CHANNEL="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$HERE/rust-toolchain.toml")"
  # the componentizer's release: `wasm-tools 1.x.y` ships with the
  # `wit-component 0.x.y` guest-builder pins, and writes the same bytes.
  WASM_TOOLS_VERSION="1.$(sed -n 's/^wit-component = "=0\.\([0-9.]*\)".*/\1/p' "$HERE/bin/guest-builder/Cargo.toml")"
  set -- "$RUST_CHANNEL" "$WASM_TOOLS_VERSION"
fi
# The setup step is the ONLY one that needs to be root inside the tree, and a
# host can refuse to give an unprivileged process the user namespace that gets
# it there: Ubuntu 23.10+ ships `kernel.apparmor_restrict_unprivileged_userns=1`,
# and bwrap dies with "setting up uid map: Permission denied" before any of
# this runs.
#
#   GUEST_SETUP_SUDO=sudo OUT=… ops/build-guest-rootfs.sh
#
# lends privilege to THAT STEP ALONE — the download, the extraction, the init
# build and the image all stay rootless, and an operator never has to loosen a
# host-wide kernel setting to build a guest.
#
# The two modes differ by exactly the flags that buy rootlessness. Under sudo
# we are ALREADY root, so asking for the namespace anyway buys nothing and
# costs the build: the same hosts attach an `unprivileged_userns` AppArmor
# profile to whatever creates one, root included, and the confined bwrap is
# then refused the bind itself ("Can't find source path …: Permission denied").
SETUP_PRIVILEGE="${GUEST_SETUP_SUDO:-}"
SETUP_AS_ROOT=(--unshare-user --uid 0 --gid 0)
[[ -z "$SETUP_PRIVILEGE" ]] || SETUP_AS_ROOT=()
if [[ -n "${ROOTFS_SETUP:-}" ]]; then
  command -v bwrap >/dev/null || { echo "guest setup requires bubblewrap" >&2; exit 1; }
fi

# The GUEST's architecture: the host's, because there is no cross-hypervisor.
# An x86_64 Linux box boots x86_64 guests under Firecracker; an Apple silicon
# Mac boots aarch64 guests under the vz shim. `uname -m` says arm64 on macOS
# and aarch64 on Linux for the same thing.
ARCH="$(uname -m)"
[[ "$ARCH" == "arm64" ]] && ARCH=aarch64

# macOS: e2fsprogs is keg-only in Homebrew, so mke2fs never reaches PATH —
# and in a non-login shell (ssh command, make, cron) brew's own bin dirs
# don't either.
export PATH="$PATH:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/opt/homebrew/opt/e2fsprogs/sbin:/usr/local/opt/e2fsprogs/sbin"

# The guest kernel differs per HYPERVISOR, not per taste: Firecracker attaches
# virtio over MMIO and its CI kernel carries only those drivers, while
# Virtualization.framework attaches virtio over PCI — measured, the CI kernel
# under VZ boots to a silent black hole (no console, no disks, no vsock). So
# Linux fetches the Firecracker CI kernel and macOS extracts the Kata
# Containers VM kernel (virtio-pci and -mmio both built in, boots a rootfs
# with no initrd). `KERNEL_URL` overrides either. Building our own kernel is
# an open question in the spec (it decides the CVE workflow); tracking these
# is what unblocks everything else meanwhile.
KERNEL_URL="${KERNEL_URL:-}"
[[ "$(uname -s)" == "Linux" && -z "$KERNEL_URL" ]] \
  && KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.12/$ARCH/vmlinux-6.1.128"
KATA_VERSION="${KATA_VERSION:-4.1.0}"
BASE_URL="${BASE_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.12/$ARCH/ubuntu-24.04.squashfs}"

say() { printf '  %s\n' "$*"; }

command -v mke2fs >/dev/null || { echo "mke2fs not found; install e2fsprogs" >&2; exit 1; }
command -v unsquashfs >/dev/null || { echo "unsquashfs not found; install squashfs-tools" >&2; exit 1; }

mkdir -p "$OUT" "$WORK"

# ---- 1. the kernel ---------------------------------------------------------
# extract the plain VM kernel out of Kata's static release bundle. The
# tarball is ~600 MB for a ~40 MB kernel, so it is cached in WORK; the plain
# kernel is the one whose name is bare `vmlinux-<version>` (the gpu/debug/
# dragonball variants carry suffixes).
fetch_kata_kernel() {
  command -v zstd >/dev/null || { echo "zstd not found; install zstd (brew install zstd)" >&2; exit 1; }
  local kata_arch=amd64
  [[ "$ARCH" == "aarch64" ]] && kata_arch=arm64
  local tarball="$WORK/kata-static-$KATA_VERSION-$kata_arch.tar.zst"
  if [[ ! -f "$tarball" ]]; then
    say "fetching the Kata VM kernel bundle ($kata_arch)"
    curl -fsSL "https://github.com/kata-containers/kata-containers/releases/download/$KATA_VERSION/kata-static-$KATA_VERSION-$kata_arch.tar.zst" \
      -o "$tarball.part"
    mv "$tarball.part" "$tarball"
  fi
  local member
  member=$(zstd -dc "$tarball" | tar -tf - | grep -E 'share/kata-containers/vmlinux-[0-9]+\.[0-9.]+-[0-9]+$' | head -1)
  [[ -n "$member" ]] || { echo "no plain vmlinux in the Kata bundle" >&2; exit 1; }
  say "extracting $(basename "$member")"
  zstd -dc "$tarball" | tar -xf - -C "$WORK" "$member"
  install -m 0644 "$WORK/$member" "$OUT/vmlinux"
}

if [[ ! -f "$OUT/vmlinux" ]]; then
  if [[ -n "$KERNEL_URL" ]]; then
    say "fetching the guest kernel"
    curl -fsSL "$KERNEL_URL" -o "$OUT/vmlinux.part"
    mv "$OUT/vmlinux.part" "$OUT/vmlinux"
  else
    fetch_kata_kernel
  fi
fi
# On macOS the kernel MUST speak virtio-pci — without it every VZ device is
# invisible and the boot is a silent hang, which is the single most
# expensive failure to diagnose from outside. Refuse it here, where the fix
# (delete the kernel, rerun, or point KERNEL_URL at a PCI-capable one) is
# printable.
# `grep -c`, not `-q`: -q closes the pipe on the first match, strings dies
# of SIGPIPE, and under `pipefail` a KERNEL THAT PASSES gets refused (141).
if [[ "$(uname -s)" == "Darwin" ]] && [[ "$(strings "$OUT/vmlinux" | grep -c virtio_pci)" == "0" ]]; then
  echo "$OUT/vmlinux has no virtio-pci support and cannot see any VZ device;" >&2
  echo "remove it and re-run (Kata kernel), or set KERNEL_URL to a PCI-capable kernel" >&2
  exit 1
fi
say "kernel: $(du -h "$OUT/vmlinux" | cut -f1)"

# ---- 2. the base filesystem ------------------------------------------------
BASE="$WORK/base.squashfs"
if [[ ! -f "$BASE" ]]; then
  say "fetching the base rootfs"
  curl -fsSL "$BASE_URL" -o "$BASE.part"
  mv "$BASE.part" "$BASE"
fi

TREE="$WORK/tree"
rm -rf "$TREE"
say "extracting the base"
# -no-xattrs: extracting capabilities/ACLs needs privileges we deliberately
# do not have. Nothing in the guest depends on them — PID 1 runs as root
# inside its own VM.
unsquashfs -no-xattrs -quiet -force -dest "$TREE" "$BASE"

# Prepare build tools in the base without lending host paths or
# credentials to runs. The setup executes as root only in a private user/mount
# namespace. Its writable scratch stays on disk and never enters the image.
if [[ -n "${ROOTFS_SETUP:-}" ]]; then
  SETUP="$(realpath "$ROOTFS_SETUP")"
  [[ -f "$SETUP" ]] || { echo "ROOTFS_SETUP is not a file: $SETUP" >&2; exit 1; }
  mkdir -p "$WORK/setup-tmp"
  say "preparing guest tools"
  $SETUP_PRIVILEGE bwrap "${SETUP_AS_ROOT[@]}" --unshare-pid --die-with-parent \
    --bind "$TREE" / --proc /proc --dev /dev \
    --bind "$WORK/setup-tmp" /tmp \
    --ro-bind /etc/resolv.conf /etc/resolv.conf \
    --ro-bind "$SETUP" /run/ducktape-guest-setup \
    --clearenv --setenv PATH /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    --setenv DEBIAN_FRONTEND noninteractive \
    /bin/bash /run/ducktape-guest-setup "$@"
  # A privileged setup writes root-owned files, and `mke2fs -d` below stays
  # rootless — it would fail on the first one it cannot read. The extracted
  # base is already the operator's, so hand the additions over to match it
  # rather than escalate the image build too.
  [[ -z "$SETUP_PRIVILEGE" ]] || $SETUP_PRIVILEGE chown -R "$(id -u):$(id -g)" "$TREE"
fi

# ---- 3. the init -----------------------------------------------------------
MUSL_TARGET="$ARCH-unknown-linux-musl"
# cargo's own answer, not `$HERE/target`: a host whose cargo config names a
# shared `build.target-dir` builds the init there.
TARGET_DIR="$(cd "$HERE" && cargo metadata --no-deps --format-version 1 \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
INIT="$TARGET_DIR/$MUSL_TARGET/release/duck-guest-init"
# ALWAYS, never "only if it is missing". cargo is already incremental, so this
# costs nothing when the source has not moved — while skipping it on an
# existing binary bakes a stale PID 1 into the image and the next boot silently
# runs last week's init.
say "building duck-guest-init (static musl, $MUSL_TARGET)"
# On a non-Linux host this is a cross build with no system musl toolchain;
# rust-lld + the rustup target's bundled musl libc link a static binary with
# nothing but `rustup target add`.
CROSS_FLAGS=""
[[ "$(uname -s)" == "Linux" ]] || CROSS_FLAGS="-C linker=rust-lld -C link-self-contained=yes"
(cd "$HERE" && RUSTFLAGS="${RUSTFLAGS:-} $CROSS_FLAGS" \
  cargo build -p duck-guest-init --release --target "$MUSL_TARGET")
install -m 0755 "$INIT" "$TREE/duck-guest-init"
say "init: $(du -h "$TREE/duck-guest-init" | cut -f1) static"

# ---- 5. mountpoints --------------------------------------------------------
# The per-run devices land here, and so do the tmpfs mounts the init makes for
# a read-only rootfs. They must EXIST in the image: mounting onto a missing
# directory fails, and creating one at boot fails too — the rootfs is read-only.
mkdir -p "$TREE/duck/workspace" "$TREE/agent" "$TREE/opt/duck/bin" \
         "$TREE/proc" "$TREE/sys" "$TREE/dev" \
         "$TREE/tmp" "$TREE/run" "$TREE/var/tmp" "$TREE/root"
chmod 1777 "$TREE/tmp" "$TREE/var/tmp"

# ---- 6. the image ----------------------------------------------------------
IMG="$OUT/rootfs.ext4"
rm -f "$IMG"
# measured tree + 64 MiB of slack: the image is READ-ONLY in every run, so it
# needs no growth room, only enough not to fail mke2fs on rounding.
# `du -sk` and not `-sb`: byte totals are a GNU extension and BSD du (macOS)
# has no -b; kibibytes over-count file bytes toward block usage, which only
# adds slack.
bytes=$(( $(du -sk "$TREE" | cut -f1) * 1024 ))
blocks=$(( (bytes + 64 * 1024 * 1024) / 4096 ))
say "building the rootfs image ($(( bytes / 1024 / 1024 )) MiB)"
mke2fs -q -t ext4 -b 4096 -d "$TREE" "$IMG" "$blocks"

# one runtime per OS: Firecracker over KVM, the vz shim over
# Virtualization.framework.
RUNTIME=firecracker
[[ "$(uname -s)" == "Darwin" ]] && RUNTIME=vz

say "rootfs: $(du -h "$IMG" | cut -f1) -> $IMG"
echo
echo "A node whose workspace holds these under guest/ boots them under:"
echo "  [sandbox]"
echo "  runtime = \"$RUNTIME\""
