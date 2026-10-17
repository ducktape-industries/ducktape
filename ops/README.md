# Operator scripts

Repo-side helpers for building, installing and maintaining ducktape. Most
scripts back a `make` target; see the repository `Makefile`.

## Desktop app

- `app/` — `install.sh` (`make install-app`) installs the desktop app at the
  immutable revision `app/APP_REV` names, from its own repository
  ([ducktape-app](https://github.com/ducktape-industries/ducktape-app));
  `install-test.sh` holds that contract.
- `macos-icon.swift` — rasterizes the app's SVG icon into the `.icns` sizes.

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
  kubectl -n NETWORK exec found -- ducktape init /data/founding/founding.toml --halo node.example.org:9100
  kubectl -n NETWORK delete pod found
  ```

- Without it, it applies `k8s/node.yaml`: one pod running the node on
  `home` and cloudflared, which carries the node's HTTP port, and the
  WebSocket on it that peers dial, through a Cloudflare tunnel. The founding
  file's `address` is `wss://` and the tunnel's hostname. The node binds halo
  at `0.0.0.0:9100` and registers the host:port given to `init --halo`.
  A node whose registered halo address takes no inbound UDP still takes part by dialing out.
  The tunnel's token is the Secret `tunnel`:

  ```sh
  cloudflared tunnel create NETWORK
  cloudflared tunnel route dns NETWORK HOSTNAME
  kubectl -n NETWORK create secret generic tunnel --from-literal=token="$(cloudflared tunnel token NETWORK)"
  ```

- The pod is the only writer of `home`: the deployment replaces it, never
  runs two. Reach the node with
  `kubectl -n NETWORK exec deploy/node -c node -- ducktape <verb>`.

## Moving a running network onto the access role

- `migrate/access.sh FOUNDING_DIR LEAD_BLOCKS [DEPLOY_COMMAND...]` moves a
  network founded before the access role onto it in place, through the
  validators' vote, while its node still runs the binary it was founded
  with. `FOUNDING_DIR` is the founding the programs repository publishes
  for the release (`founding.toml` and every file it names). A batch of
  program changes lands `LEAD_BLOCKS` blocks, plus one per ballot in the
  batch, after the tip it starts from; it must outlast the vote, so a
  network of several validators gives it room.
  In order, it:
  - puts every program and view whose code changes as `program` blobs, signed
    by `DUCKTAPE_PUT`'s wallet, which needs an account;
  - admits `access`, then binds the access role to it, and binds a role to
    `forge`, `files` and `harness` each, so the vote reaches their one-time
    `Migrate` op (`ducktape vote bind`);
  - swaps every program but `programs` and `valset`, and every view, to its
    new code at one block, and checks the programs program holds each change
    (a vote is enacted even when the op it carries is refused);
  - runs `Migrate` by vote on forge, files, harness, then identity (harness
    reads the old answer lists identity drops), and unbinds the roles bound
    for the move;
  - swaps the programs program, then runs its `Migrate`, which copies the
    kernel's role table into its own (its new code refuses every change
    while that table is empty);
  - swaps `valset`, which has no `Motion::Bind` once swapped;
  - verifies each program's code, that the role table equals both the
    kernel's and the roles `founding.toml` declares, and prints the access
    lists the move wrote;
  - runs `DEPLOY_COMMAND`, or prints the deploy step for the new binary
    (`k8s/deploy NETWORK COMMIT`).
- Each step checks the state first, so the script runs again safely. A vote
  that only counts stops it with exit 3: every other validator casts the
  same ballot, from the op file kept in `MIGRATE_DIR` (default
  `./access-migration`), and the script runs again.
- `DUCKTAPE` is the command that runs the `ducktape` CLI of the binary the
  network still runs (the one with `vote bind`) with the validator's
  workspace, reading an op on stdin (for a pod:
  `kubectl -n NETWORK exec -i deploy/node -c node -- ducktape`);
  `FOUNDING_REMOTE` is where `DUCKTAPE_PUT` sees the founding's files.
  It needs `bash` and `python3` where it runs.

## Worktrees

- `worktree-clean.sh` — removes task worktrees whose branch is fully merged
  into `origin/dev`; dry-run by default, `--yes` to act. It REFUSES a worktree
  that is dirty, carries a commit not in `dev`, or has live processes under
  it. `worktree-clean-test.py` covers those refusals.
