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

## A network's node on Kubernetes

- `k8s/deploy NETWORK COMMIT [--found]` builds `k8s/Containerfile`
  around the Linux ducktape that release `bin-COMMIT` carries, pushes it to
  the registry on the cluster node (`DEPLOY_NODE`, default
  `fabrics@fabrics-1`, at its `127.0.0.1:5000`), and applies
  `k8s/network.yaml`: the namespace `NETWORK` and the claim `home`, whose
  volume is kept when the claim is deleted.
- With `--found` it then applies `k8s/found.yaml`, a pod `found` that holds
  `home` at `/data` and runs nothing. Found the network into it, then
  delete it:

  ```sh
  kubectl -n NETWORK exec found -- ducktape identity
  kubectl cp founding NETWORK/found:/data/founding
  kubectl -n NETWORK exec found -- ducktape init /data/founding/founding.toml
  kubectl -n NETWORK delete pod found
  ```

- Without it, it applies `k8s/node.yaml`: one pod running the node on
  `home` and cloudflared, which carries the node's HTTP port, and the
  WebSocket on it that peers dial, through a Cloudflare tunnel. The founding
  file's `address` is `wss://` and the tunnel's hostname. The tunnel's token
  is the Secret `tunnel`:

  ```sh
  cloudflared tunnel create NETWORK
  cloudflared tunnel route dns NETWORK HOSTNAME
  kubectl -n NETWORK create secret generic tunnel --from-literal=token="$(cloudflared tunnel token NETWORK)"
  ```

- The pod is the only writer of `home`: the deployment replaces it, never
  runs two. Reach the node with
  `kubectl -n NETWORK exec deploy/node -c node -- ducktape <verb>`.

## Worktrees

- `worktree-clean.sh` — removes task worktrees whose branch is fully merged
  into `origin/dev`; dry-run by default, `--yes` to act. It REFUSES a worktree
  that is dirty, carries a commit not in `dev`, or has live processes under
  it. `worktree-clean-test.py` covers those refusals.
