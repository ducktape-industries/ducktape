# The pods service

`server.py` is the pods service: a network's agents use it to run work on a Kubernetes
cluster through the network's egress. It makes pods with fetched inputs and execs into
them, either whole or streamed. It also serves logs and a wait that streams until a pod
ends, applies and deletes manifests, passes Kubernetes API calls through inside one
namespace, puts a pod's outputs as blobs or files, reads a file out of a pod, packs a pod's commit for a forge push and reads forge's report of it,
posts when work ends, and reports usage. The build runners `PODS_RUNNERS` names (`desktop`
unless it names others) are machines reserved as pods of their names, reached by ssh, straight
or through a member node's halo forward.

- The contract agents call: `/shared/jobs/J-016-deploys-and-pods/pods-contract.md` in the
  network's files (endpoints, bodies, answers, refusals), and the routes in
  `../../README.md`, section "Agents' computing on Kubernetes".
- `server-test.py` covers the service over real HTTP against a recording kubectl, ducktape
  and kubelet: `python3 ops/k8s/pods/server-test.py` (also part of `make test`).
- `server.py fetch` is each pod's init container, which fetches its inputs.
- A run a notified pod arms (`ducktape-run armed <channel> <run> <words>` in its log) and
  never ends (`ducktape-run done <run>`) is posted `run <run> lost with pod <pod>: <why>` in
  its channel, once, when the pod ends, when it is deleted, or when the service finds it gone
  as its watch starts. The service keeps each open run in the ConfigMap `runs` in namespace
  `pods` as it reads the line, so a rotated log or a restart of the service loses none.
- A run whose log also holds `ducktape-run argv <run> <base64>` (its settings and words,
  NUL-separated) is called again instead, once: on the first build runner that takes it, with
  `AGAIN=1` added, and its channel is told `run <run> is now run <machine>:<new> …: wait on that
  one`, never that it failed. A run no runner takes waits in the ConfigMap (`pending-…`) and is
  called again by the next builds call, which can reach the warm pod too. A run whose words carry
  `AGAIN=1` is posted lost as above, and a run marked as being called again when the service
  stopped is posted with a word to read `build.sh busy` before calling it again.

## What it needs

- **A Kubernetes namespace and ServiceAccount.** `pods.yaml` makes namespace `pods`, which
  holds the service, and namespace `agents`, where agents' work runs. It also makes the
  ServiceAccount `pods` with a Role in `agents` (pods, exec, logs, the workload kinds), a
  Role in `pods` for the ConfigMap `runs` (below), and
  a ClusterRole for `nodes/stats` (usage), the PriorityClass `agents`, the admission
  policies, the quota `hardcap`, the LimitRange `defaults` and the NetworkPolicies. The
  policy `node-only` admits callers only from the network's node pod (namespace
  `duckpond`, label `app: node`), so the `ducktape-*` headers the service reads are the
  node's.
- **Its own chain account.** The service signs blob puts, files commits and chat posts as
  an account of the network. Its key file and password are the Secret `blob-key` in
  namespace `pods` (files `key` and `password`), mounted at `/keys`. Without that Secret
  it runs, but every call that has to sign answers 503.
- **An egress binding.** The network's egress program has to know the service, and a node
  has to front it:
  1. An account registers the service, `egress` `Register { service: "pods" }`, and assigns
     it to the nodes that serve it, `Assign { service: "pods", nodes: [<node key>] }`. The
     account that registers it owns it.
  2. Each of those nodes names it in its `egress.toml`, as
     `[services.pods] base = "http://<the Service's address>:8080"`, then runs
     `ducktape egress reload`. The node then sends `Serve` by itself. On duckpond that
     entry is a change to forge repo `infra`'s `egress/fabrics-1.toml`, which reconcile
     applies once it merges (`../../README.md`, section "duckpond's desired
     infrastructure"); nobody edits the node's file by hand.
- **The API server's OpenAPI.** Before it passes a body on (a manifest, a pod's resources, a
  `/v1/k8s/` write, patch or subresource), it reads `/openapi/v3` (granted to every account by
  the default `system:discovery` role) to find each field typed as a Quantity in the kind
  Kubernetes will decode the body as (the last top-level key that case-folds to `kind`, and
  for a merge patch the kind of the object it makes; a strategic or apply patch is its
  endpoint's kind), and sends
  Kubernetes its own reading of each in Kubernetes' canonical form, so Kubernetes never parses an
  agent's text. Each group-version's schema is kept by the hash the index names.
- **Its node's status.** It reads the network's block time from `NODE_URL`'s `/v1/status` as
  it starts, and every retry pause starts there; it stops, saying why, when that node
  doesn't answer.
- **Optional: the warm pod for `builds`.** `PODS_WARM_POD`, a files path whose one line names
  the warm pod `builds` falls back to. Unset, `builds` answers 503 when no runner is free.
- **Optional: the build runners.** `PODS_RUNNERS`, `<name>=<keys path>` words in tie order
  (`desktop=/desktop` when unset), each keys path a mounted Secret (files `id_ed25519`,
  `id_ed25519.pub`, `known_hosts`, `user`, and `hosts` or `forward`, optionally `kinds`):
  duckpond's are `desktop-runner` at `/desktop` and `runner-zk-dev` at `/runners/zk-dev`. A
  runner with a `forward` file is reached through `ducktape forward` (from `PODS_DUCKTAPE`,
  signed with the service's key from `blob-key`), so the image's CLI must carry that verb and
  the serving node's `forward.toml` must list the service's key among that service's
  `signers`. Without its Secret, a runner's pod answers 503.

## Stand one up for another network

1. Build and apply: `ops/k8s/pods/deploy NODE_URL FORGE DUCKTAPE_COMMIT`, where `NODE_URL`
   is a node the pods' inputs are read from and `FORGE` is `duck://<chain id>/forge`.
   It builds the image, pushes it to the cluster node's registry and applies
   `pods.yaml`, with `hardcap` computed from the live cluster. The NetworkPolicy
   `node-only`, the Service's fixed `clusterIP` and the deploy's `DEPLOY_NODE` and
   `DEPLOY_CONTEXT` assume fabrics-1 and duckpond's node. Change those to match the new
   network's node and cluster.
2. Give it an account: create a key and an account for it on the network (`ducktape wallet
   new`, then identity `Create`), then
   `kubectl -n pods create secret generic blob-key --from-file=key=KEY --from-file=password=PASSWORD`
   and `kubectl -n pods rollout restart deployment/pods`.
3. Bind it in egress: `Register` and `Assign` as above, then the node's `egress.toml`
   entry and `ducktape egress reload` (on duckpond, the `infra` change above).
4. On duckpond a new image after that rolls through forge repo `infra`: a merge
   of its `pods.toml` makes reconcile patch the Deployment's image and prove it
   (`../../README.md`, section "duckpond's desired infrastructure").
5. Check it from an agent: `network_call` egress `Request` with `GET /v1/usage` answers
   200, and a pod made with `POST /v1/pods` runs and posts its end as a chat notice for its requester in its `notify`
   channel.
