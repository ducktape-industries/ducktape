# Operator scripts

Repo-side helpers for building, installing and maintaining ducktape. Most
scripts back a `make` target; see the repository `Makefile`.

## Desktop app

- `app/` — `install.sh` (`make install-app`) installs the desktop app at the
  immutable revision `app/APP_REV` names, from its own repository
  ([ducktape-app](https://github.com/ducktape-industries/ducktape-app));
  `install-test.sh` holds that contract.
- `macos-icon.swift` — rasterizes the app's SVG icon into the `.icns` sizes.

## Sandbox (microVM) hosts

- `build-guest-rootfs.sh` — builds one workspace's kernel and rootfs
  (`OUT=<workspace>/guest`) for Firecracker (Linux) or vz (macOS). Linux
  installs the pinned Rust and the `wasm-tools` CLI through
  `guest-rust-tools.sh` by default; `ROOTFS_SETUP` selects a custom setup.
- `firecracker/` — `boot-bench.sh` and `snapshot-bench.sh`, the cold-boot and
  snapshot-restore timing lanes for the microVM sandbox.

## Airlock enclave image

- `airlock-gateway/install-rcodesign.sh` — the pinned `rcodesign` release
  (SHA-256 checked) into `<prefix>/bin`; what the gateway's
  `POST /sign/macos-bundle` signs with, and what `cargo test -p airlock`
  needs on `PATH` (`make rcodesign`).
- `airlock-gateway/stage-image.sh` (`make airlock-gateway-image`) — the
  enclave image root: the release `airlock-gateway`, `rcodesign`, and the
  entitlements plist at the binary's default paths.

## Worktrees

- `worktree-clean.sh` — removes task worktrees whose branch is fully merged
  into `origin/dev`; dry-run by default, `--yes` to act. It REFUSES a worktree
  that is dirty, carries a commit not in `dev`, or has live processes under
  it. `worktree-clean-test.py` covers those refusals.
