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

- Without it, it applies `k8s/tunnel.yaml` and `k8s/node.yaml`.
  `node.yaml` is the deployment `node`, one pod running the node on `home`,
  and the Service `node-http` on the node's HTTP port, `8844`. `tunnel.yaml`
  is the deployment `tunnel`: cloudflared, which carries `node-http`, and the
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

- The node's pod is the only writer of `home`: the deployment replaces it,
  never runs two. Reach the node with
  `kubectl -n NETWORK exec deploy/node -c node -- ducktape <verb>`.
- Cloudflare spreads a tunnel's traffic over every connector it has, so the
  tunnel deployment starts a new cloudflared, waits until its `/ready`
  answers, and only then stops the old one. Restarting the node leaves the
  tunnel up; the public address fails only while the node is down.
- The node's pod runs at PriorityClass `system-node-critical`, the tunnel's
  at `system-cluster-critical`. The kubelet never evicts a pod of either
  class when the machine runs short. It gives the node's processes OOM score
  -997 whatever they request, so the kernel kills an agent's process before
  the node's. The tunnel's score follows from its memory request, as for any
  pod that requests some. Both request the CPU and memory they were measured
  to use and set no limit, so nothing kills the node for passing one.

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
- A pod created with a `notify` (a chat channel, and optionally a thread in
  it) gets one post there when it ends, mentioning its `requester`: the run
  container's exit code, or why fetching its inputs or the pod failed. A pod
  without one gets none. Nothing else posts: `pods/NAME/blob` and
  `pods/NAME/file` answer with their result, so they post nothing.
- Namespace `agents` holds the ResourceQuota `agents` (`requests.storage`
  500Gi), which bounds what its claims ask for. A pod states cpu and memory
  only if it wants requests or limits; without them it contends for the
  whole machine. The PriorityClass `agents` ranks its pods below every
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

## Forge and GitHub

- dev and main of ducktape, programs and app live on duckpond's forge
  (repositories `core`, `programs`, `app`) and on GitHub. Each repo's
  `.github/workflows/forge-sync.yml` moves forge's branch to a GitHub push.
- `k8s/forge-mirror/mirror.py` is forge-mirror, which carries the other
  direction. It follows duckpond's forge writes over `/v1/changes/forge`. A
  block that moves a followed forge branch starts one job for that branch, and
  so does a start. A job pushes forge's head to GitHub's branch of the same
  name with `--force-with-lease` on the GitHub head it just read, so forge
  always wins; a push that drops GitHub commits logs them as `reset`. A push
  that loses a race with another GitHub push runs again. A job that fails
  otherwise runs again at each later block that writes forge or the clock
  program (`FORGE_MIRROR_CLOCK`, `scheduler`), and logs its first attempt and
  every tenth with an `attempts` field. `k8s/forge-mirror/mirror-test.py`
  covers it.
- forge-mirror pushes over HTTPS as the GitHub App "ducktape forge-mirror",
  with an installation token it mints from the App's key. The Secret
  `forge-mirror/github-app` holds `app_id`, `installation_id` and
  `private_key`, nothing else.
- `k8s/forge-mirror/deploy` builds the forge-mirror image around `mirror.py`
  and the pinned git-remote-duck, pushes it to the cluster node's registry, and
  applies `forge-mirror.yaml`: namespace `forge-mirror`, its state claim, a
  policy closing every inbound port, and Deployment `forge-mirror`. Create the
  Secret apart from it:
  `kubectl -n forge-mirror create secret generic github-app --from-literal=app_id=APP_ID --from-literal=installation_id=INSTALLATION_ID --from-file=private_key=APP.pem`.
- `k8s/forge-mirror/protect APP_ID [--apply]` puts one ruleset on dev and
  main of each repository that blocks update, force push and deletion for
  everyone but the App, and deletes the classic protection it replaces.
  Without `--apply` it prints both.

## Worktrees

- `worktree-clean.sh` — removes task worktrees whose branch is fully merged
  into `origin/dev`; dry-run by default, `--yes` to act. It REFUSES a worktree
  that is dirty, carries a commit not in `dev`, or has live processes under
  it. `worktree-clean-test.py` covers those refusals.
