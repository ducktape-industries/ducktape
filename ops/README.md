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

## Forge and GitHub in step

- dev and main of ducktape, programs and app exist on GitHub and on
  duckpond's forge (repositories `core`, `programs`, `app`). Each repo's
  `.github/workflows/forge-sync.yml` moves forge's branch to a GitHub push;
  `forge-sync/follower.py` moves GitHub's branch to a forge push.
- The follower runs on orthorymain as the systemd user unit
  `forge-sync.service`, so the forge-to-GitHub half works only while
  orthorymain is up. It pushes over SSH with orthory's own key, so its pushes
  act with orthory's GitHub rights: as an admin it passes dev's required
  `unit` check, and the branch rules of dev in the three repos and of app's
  main let orthory alone force-push.
- The follower follows duckpond's writes to forge and to chat
  (`/v1/changes/forge`, `/v1/changes/chat` at
  https://duckpond.ducktape.industries). A block that moves a followed forge
  branch starts one job for that branch; a post in chat channel `sync` starts
  one for every branch, and so does a start. When the tunnel closes an idle
  follow, the follower follows again and syncs what moved meanwhile.
- A job reads GitHub's head. GitHub behind is pushed forge's head; GitHub
  equal or ahead is left; diverged, forge wins: GitHub's branch is pushed
  forge's head with a lease on the head just read, and the follower posts one
  line in #sync naming the old head, the new one and every dropped commit. A
  job whose push loses a race with a GitHub push runs again. A job that fails
  otherwise posts its reason in #sync once and runs again at the next forge
  push or #sync post.
- `forge-sync.yml` changes nothing when forge already holds the push or has
  moved past it; when the two have diverged it posts in #sync, which starts
  the follower's jobs. A push by either side leaves the pair equal, so the
  other side's run does nothing.
- `forge-sync/install.sh COMMIT` installs `follower.py`, the ducktape CLI
  release `bin-COMMIT` carries and git-remote-duck under
  `~/.local/share/forge-sync`, and the unit, and starts it. Its state is
  `~/.local/state/forge-sync`: the git mirrors, the journal of resets not yet
  posted, and `keys/key` and `keys/password`, account 22's sealed
  "forge-sync follower" key that signs its #sync posts (mode 600).
  `forge-sync/follower-test.py` covers the follower.

## Worktrees

- `worktree-clean.sh` — removes task worktrees whose branch is fully merged
  into `origin/dev`; dry-run by default, `--yes` to act. It REFUSES a worktree
  that is dirty, carries a commit not in `dev`, or has live processes under
  it. `worktree-clean-test.py` covers those refusals.
