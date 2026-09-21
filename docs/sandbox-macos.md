# The sandbox on macOS (vz)

One stack decision, fixed: **Linux → Firecracker over KVM, macOS → the vz
shim over Virtualization.framework** (the same substrate Apple's
`apple/containerization` builds on). Everything above the hypervisor is
shared: the same guest kernel + `duck-guest-init` rootfs, the same ext4
workspace/asset/manifest block devices, the same vsock frame protocol, the
same `<uds>_<port>` socket convention, the same Firecracker-schema config
JSON. The shim (`bin/duck-vz-shim`, a few hundred lines of Swift) is the only
macOS-specific component.

## Prerequisites — one pass

```sh
ops/macos-preflight.sh
```

`make dev` runs this for you with `--prompt`: on a Mac it lists what is
missing and asks once — "install these now? [Y/n]" — then runs the accepted
steps (brew packages, the musl target, the shim build+sign). Declining just
leaves a node that refuses provider runs.

It checks everything the compute plane (airlock + provider runs) needs on a
Mac and prints the exact install command for each missing piece: Apple
silicon/`kern.hv_support`, Xcode command line tools,
`brew install e2fsprogs squashfs zstd` (e2fsprogs is keg-only — the node searches
the standard Homebrew prefixes itself, do not add it to PATH),
the `aarch64-unknown-linux-musl` target (`rust-toolchain.toml` lists it, so
`rustup toolchain install` from the checkout is what installs it), and the
signed shim. Exit 0 means the host half of the node's boot probe will pass;
the guest images are each workspace's own, built below.

## Bring-up on a Mac

```sh
# 1. the shim (builds + ad-hoc codesigns the virtualization entitlement)
INSTALL=~/bin bin/duck-vz-shim/build.sh        # ~/bin must be on PATH

# 2. the workspace's guest artifacts (aarch64 kernel + rootfs; cross-builds
#    the init with rust-lld, no musl toolchain needed). A guest is per
#    workspace: W is the node's workspace (`--workspace`, default
#    `$DUCKTAPE_HOME`).
OUT=$W/guest ops/build-guest-rootfs.sh

# 3. the smoke: one microVM end to end — boot, stdio, exit code, workspace
#    read-back. This is the first thing to run and the thing to bisect with;
#    `vm_smoke: OK` is the proof.
cargo run -p sandbox-host --example vm_smoke -- \
    --kernel $W/guest/vmlinux \
    --rootfs $W/guest/rootfs.ext4

# 4. the node: the table says HOW, the images are read from $W/guest
#   [sandbox]
#   runtime = "vz"
```

The boot probe fails loudly (daemon boot, not first run) when any of these is
missing: the shim on PATH, its `com.apple.security.virtualization`
entitlement (re-run `build.sh` — it codesigns), `kern.hv_support`, `mke2fs` /
`debugfs`, or the images.

## What differs from Linux, deliberately

- **No tap, no nftables.** A macOS guest gets no network device at all; runs
  reach the host over the vsock tunnel allowlist only — the same as Linux,
  where the single production `VmConfig` also sets no tap.
- **The kernel is the Kata Containers VM kernel, not the Firecracker CI
  kernel.** VZ attaches virtio over PCI; the Firecracker CI kernel is
  virtio-MMIO only and boots into a silent black hole (no console, no disks).
  `build-guest-rootfs.sh` therefore extracts the Kata kernel on macOS
  (virtio-pci + -mmio, no-initrd boot) and refuses any macOS kernel without
  `virtio_pci` in it.
- **The kernel command line** (`firecracker_api::boot_args`): `console=hvc0`
  (virtio console, not 16550), explicit `root=/dev/vda ro` (appending it is a
  Firecracker behavior), and no `pci=off` (that flag boots a guest that finds
  no disks under VZ).
- **The guest is told how to die.** A guest reboot really REBOOTS under VZ
  (there is no `guestDidStop`), so an init that exits Firecracker-style would
  boot-loop forever. The vz cmdline carries `DUCK_HALT=poweroff` and the init
  powers off via PSCI, which is what stops the VM; `panic=` stays default on
  vz so a panicking guest parks for the host's timeout instead of
  boot-looping.
- **`vsock.listen_ports`** rides the config JSON for vz only:
  Virtualization.framework wants each guest-outbound port declared, while
  Firecracker forwards any port by convention and rejects unknown config
  fields.
- **The shim's fds are non-blocking.** VZ hands over `O_NONBLOCK` fds, so
  `EAGAIN` on read is not EOF; the shim bridges them accordingly.
- **`build.sh` uses bare `swiftc`** and walks the installed SDKs newest-first
  until one builds: a command-line-tools install can be internally skewed
  (SPM aborting, the newest SDK unparseable by its own compiler).
- **Executors must be Linux aarch64 ELF binaries**, and they do not come from
  the host `PATH` — a Mac's own `claude`/`codex` is Mach-O and the guest cannot
  exec it at all. The pinned linux/arm64 build lives in
  `<workspace>/executors`, and the node derives
  a read-only image from that directory and mounts it at `/opt/duck/bin` for
  each run. The rootfs carries no CLI, so installing one needs no image
  rebuild.
