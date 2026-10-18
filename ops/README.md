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

## Agents' computing on Kubernetes

- `k8s/pods/deploy NODE_URL FORGE COMMIT` builds `k8s/pods/Containerfile` around
  the Linux ducktape that release `bin-COMMIT` carries, kubectl and
  git-remote-duck, pushes it to the cluster node's registry, and applies
  `k8s/pods/pods.yaml`: the service `pods` in namespace `pods` at
  `http://10.43.80.80:8080`, and namespace `agents`, where it runs what agents
  ask for; a pod's inputs come from the node at `NODE_URL` and its forge
  `FORGE` (`duck://NETWORK/forge`). A network reaches it as the egress service `pods`
  (`[services.pods] base = "http://10.43.80.80:8080"` in the node's
  `egress.toml`); a NetworkPolicy admits only the duckpond node pod, so the
  `ducktape-chain`, `ducktape-program` and `ducktape-request` headers it reads
  are the node's.
- `k8s/pods/server.py` is the service, and `server.py fetch` the init
  container that fetches a pod's inputs. Its routes, under `/v1`: `pods`
  (create, list), `pods/NAME` (status, delete), `pods/NAME/logs`,
  `pods/NAME/exec`, `pods/NAME/blob`, `pods/NAME/file`, `manifests` (apply,
  delete), `manifests/KIND[/NAME]` and `usage`. `server-test.py` covers them.
- Namespace `agents` holds the ResourceQuota `agents` (`limits.cpu` 48,
  `limits.memory` 96Gi, `requests.storage` 500Gi), so every pod states cpu
  and memory limits; the PriorityClass `agents` ranks its pods below every
  other pod on the cluster. Its NetworkPolicy `isolated` admits traffic only
  from inside the namespace and lets pods out to public addresses and DNS.
- The service signs blob puts, files commits and chat posts as the program
  account `pods`, whose key and password are the Secret `blob-key` (files
  `key` and `password`) in namespace `pods`:

  ```sh
  kubectl -n pods create secret generic blob-key --from-file=key=KEY --from-file=password=PASSWORD
  ```

- `k8s/toolchain/deploy` builds `k8s/toolchain/Containerfile`, the image
  agents build ducktape, programs and the app with, pushes it to the
  cluster node's registry and prints its reference.

## Worktrees

- `worktree-clean.sh` — removes task worktrees whose branch is fully merged
  into `origin/dev`; dry-run by default, `--yes` to act. It REFUSES a worktree
  that is dirty, carries a commit not in `dev`, or has live processes under
  it. `worktree-clean-test.py` covers those refusals.
