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
- `server.py hardcap` prints the cpu and memory `deploy` gives the quota `hardcap`: the
  nodes' allocatable minus the requests of every Running or Pending pod outside `agents`
  (its pod-level request where it states one, else its containers' summed), read with
  `PODS_KUBECTL` (`kubectl` when unset). It refuses a cap that lowers `hardcap` below what its
  `status.used` says agents' pods already hold (a raise still applies), naming the pods holding it, so `deploy`
  stops before it builds.
- The requester of what `POST /v1/pods` and `POST /v1/manifests` make is the account egress
  says sent the request (egress `Sender`, asked once per create), or a fleet call's asker. A
  body naming a `requester` is refused 400; a create egress could not be asked about is refused
  502 and makes nothing; a request egress names no account for makes it with no requester.
- A call fleet relays for a machine (`ducktape-program` is `ducktape-industries/fleet`) names its run's asker in
  `fleet-asker`, which fleet stamps on every call it relays. The service makes that account the
  requester of what the call makes, and refuses such a call with no account there. Such a call reaches only its own run's pod: a pod it makes carries its
  `fleet-run` and `fleet-attempt` as labels, and it may read, wait for, exec in, put the outputs
  of and delete only a pod labelled with both; anything else, a build runner included, is 403.
- Only the service writes `ducktape.industries/*` labels and annotations, which say whose object
  it is. A `/v1/manifests` manifest or a `/v1/k8s/` POST that names one is refused 403. A
  `/v1/k8s/` PUT must name exactly those the object holds, read from the same path first, and a
  PATCH of any type is judged by its dry run against that read. Both cover the object, its pod
  template and its claim templates. The write is then sent held to the `resourceVersion` of the
  object as the requester check below read it (for a subresource, the object it belongs to), so a
  write that lands in between makes Kubernetes refuse it 409.
- A change or a delete is the object's requester's alone: `DELETE /v1/pods/NAME`, `DELETE
  /v1/manifests` and a `/v1/k8s/` PUT, PATCH or DELETE of an object, or a POST to one's subresource
  (a pod's eviction or binding, a service account's token; a subresource's write, and a
  body Kubernetes forwards unread such as `proxy`, judged by the object it belongs to, which the
  service reads itself), and a `/v1/k8s/` call of any method to a pod's or service's `exec`,
  `attach`, `portforward` or `proxy` (judged by that pod or service, a proxy's `NAME:PORT` or
  `SCHEME:NAME:PORT` split as Kubernetes splits it and any other form refused 400, then sent on
  as it came, its body and query unread) go through only when the account egress says sent the request (a call fleet relays: its run's asker) is the object's `requester` label,
  and anyone else is refused 403 naming whose it is. The program that made a pod deletes it as its
  own account, which is that pod's requester. An object with no requester label (made before pods
  stamped one) is changed or deleted by no call: the operator removes it with kubectl. The service
  reads the object, then sends the delete held to the uid it read, so an object made again under
  the same name in between is not deleted. A `DELETE /v1/pods/NAME` of a pod that hasn't ended
  first marks it `ducktape.industries/deleted-by-requester` with that uid and the time it asks, so its end notice says
  it was deleted by its requester. A `/v1/k8s/` PUT or PATCH of an object that isn't there
  is refused 404, naming the POSTs that make one (a `/v1/k8s/` POST to its collection, or `POST
  /v1/manifests`), which stamp its requester. `POST /v1/manifests` keeps that rule for an object that is
  already there: it reads every object its manifests name in one `kubectl get`, and applies
  over an existing one only for that object's requester (anyone else, and any call over an
  object with no requester label, is refused 403 before anything applies), with the manifest
  held to the uid it read. So no apply moves an object's requester label to another account,
  and with it the right to delete it. A `/v1/k8s/` DELETE of a collection deletes only the caller's
  own: its `labelSelector` gains `ducktape.industries/requester=<caller>`. A `/v1/k8s/` POST
  stamps the sender as requester too, as `/v1/pods` and `/v1/manifests` do.
- `POST /v1/builds` makes one build from `{repo, commit, base, gates | command, env, notify,
  limit}`: a bare pod in namespace `builds` named `<chain>-<request>` as any pod is, so its name,
  the run's id, is in the create's answer, and a lost answer is read again by that name
  (`GET /v1/builds/NAME`). Its init container `fetch` mirrors `repo` and tools `main` into its
  node's `/srv/build-jobs/repos` under a lock; then `run`, which mounts that mirror read-only,
  checks `commit` out with `origin/<base>` beside it and runs tools' `gates.sh` with `GATES` set to
  `gates` (full, check or light), or the command. A gates build keeps cargo's registry and git in
  the node's `/srv/build-jobs/cargo`. A command build mounts nothing of the node: its mirror and
  cargo live in its own `/work`. It ends within `limit` minutes, 1 to 40 (40
  when unnamed, the gates' own bound from J-493, as `activeDeadlineSeconds`). `GET /v1/builds/NAME/logs` reads its log, `DELETE
  /v1/builds/NAME` deletes it for its requester alone, and `notify` posts its end as a pod's.
  An `env` naming a setting the pod makes itself is refused 400.
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
- A build.sh call sent to a pod by name (the warm pod, not `builds` or a build runner) goes with
  `PINNED=1` added to its settings, unless it names that itself. build.sh's `movable` then keeps
  the run, so `builds`' reroute never moves it to another machine.
- A merge queue's lane starts by itself, once per selected Preview. With a train whose tip
  contexts name this service's lane, only its last formed member is selected; otherwise it
  is the queue front. With a key, a thread finds the account the key acts as (identity `OfKey`) and
  follows `ducktape changes forge`. Each time the stream opens it reads forge `Repos` a page at
  a time and settles every repository it lists (so any repository with a queue); then it
  settles a repository once per block that writes its queue (`e/`), statuses (`s/`), Previews
  (`m/`), required checks (`q/`), train configuration (`y/`) or formation (`z/`). A settle
  reads forge `Required`, and for each branch whose contexts hold
  `<the service's account>/build.sh#lane` its `Train`. When that lane is a tip context, it
  selects the last formed member's Preview commit and change number; no formed members means
  no lane. Otherwise it reads the `Queue` front and that change's `Preview`. It reads only
  the selected commit's `Statuses`; a repository with such a branch also has the ConfigMap `runs`
  read once. When the branch's reporter has that lane Pending or nothing there, and every other
  required context Passed (a selected member whose review is Pending waits for it), it first keeps
  `rerun-<sha256(repo, commit)>` in the ConfigMap `runs`, then calls build.sh
  `JOB=forge:<repo>:<n> NOTIFY=forge:<repo>:<n> [FIRST_LIMIT=<minutes>] <repo> <commit> lane`
  as a `builds` call goes (with no request behind it), records the run under the key and bumps
  it to the front of that machine's check lane (`build.sh bump <run>`), since it holds back every
  change behind it. A
  lane that doesn't start is recorded with why, posted in the change's room once per distinct
  reason, and asked again after one block time, then two, four and so on, while that Preview is
  still selected; it stops as soon as the lane target moves. A restart never starts a second
  lane for a Preview it kept (it takes up the retries of the ones that didn't start), a key whose
  Preview is no longer selected is dropped and its run stopped (`build.sh stop <run>`,
  followed to wherever build.sh moved it; a run that already ended counts as stopped, and only a
  stop refused for another reason logs `pods_rerun_unbuilt`), and the lane's report reaches forge-reach
  as any lane's does.

## What it needs

- **A roll and the calls in flight.** The Deployment's pods container has a readiness probe on
  its port, so the rollout stops the old pod only once the new one listens, and a five-second
  `preStop` sleep, so a call the Service routed to the old pod just before it left the
  endpoints still reaches it. At SIGTERM the service stops listening, hangs up its idle
  connections and reattachable waits, cancels and joins its background workers, and answers each
  whole call and exec stream still open before it exits. An exec whose caller disconnected is
  also drained. Shutdown logs how many whole calls and which pods' execs are still open, then
  logs every background worker it is waiting on (worker name, thread identity and stack
  locations), each subprocess it reaps, and each joined worker before saying that background
  work has stopped and the drain has finished. Runner reads and live follows both belong to
  their background worker and are cancelled together; foreground execs remain separate.
  `pods_posted` carries the submit receipt's outcome and reason, distinguishing an applied
  submission from an `already_exists` refusal without claiming a second message was created.
  `terminationGracePeriodSeconds` is the preStop plus
  egress's wait for a whole answer (`DEFAULT_WAIT`, 64 blocks, times the network's block time,
  which `deploy` reads from `NODE_URL`'s `/v1/status`), since egress gives up on a whole answer
  then anyway. A stream has no such bound (egress's wait runs from its last event); Kubernetes
  still ends a command that outlives the Deployment's grace period. A wait (a pod's or a build runner's) answers with
  `ducktape-reattach: request`, so a node that serves egress (core `README.md`, the service
  request section) sends the same request again to the new pod, which waits on the same pod
  and ends with its real end; a wait's events before its end are alive beats and at most one
  `note` that its pod can't be placed yet, so the new one starts afresh whatever
  `ducktape-heard` says, and notes a pod still unplaced once more. An exec stream carries no such mark;
  the stopping service keeps its connection through the command's final answer.

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
- **Its node's status.** It reads the network's block time from `PODS_NODE`'s `/v1/status` as
  it starts, and every retry pause starts there; it stops, saying why, when that node
  doesn't answer.
- **Its node inside the cluster.** The service sends its own calls (`PODS_DUCKTAPE`: blob
  puts, files commits, chat posts, follows) to the Service `node-http` in namespace
  `duckpond`, `http://node-http.duckpond.svc:8844`, never through the public host, whose
  Cloudflare tunnel refuses a request body over 100 MB. Agent pods' input fetches read the
  same node (`PODS_NODE`): namespace `agents`' policy `isolated` lets them reach only
  addresses outside the private ranges, and its policy `to-duckpond-node` adds TCP 8844 on
  duckpond's node pods.
- **Optional: build pods.** `PODS_BUILD_IMAGE` (a `localhost:5000/toolchain` image),
  `PODS_BUILD_CPU` and `PODS_BUILD_MEMORY` (each build pod's fixed request, and its memory
  limit; duckpond's 7 and 28Gi fit four to a lat VM's 32 cpu and 121.8Gi allocatable), and
  `PODS_BUILD_NAMESPACE` (`builds` when unset). Unset, `/v1/builds` answers 503. `pods.yaml`
  writes namespace `builds`: its Role `pods-builds` lets the service create, read, watch, patch and delete pods and read their
  logs; the policy `builds-pods` holds every pod there to what the service writes (uid 10001,
  non-root, no privilege or added capability, `hostUsers: false`, hostPath only
  `/srv/build-jobs/cargo` and `/srv/build-jobs/repos`, the toolchain image, class `builds`,
  `builds.ducktape/pods=true` and the `ducktape.industries/builds` toleration). Its policy
  `isolated` admits no traffic in and lets its pods out only to their own namespace, DNS and
  public addresses; `to-duckpond-node` adds TCP 8844 on duckpond's node pods. A node carries
  `builds.ducktape/pods=true` only once its SSH runner is out, so the two never share a VM. Build
  notices keep their open runs in the ConfigMap `build-runs`.
- **Runner-only placement.** With `PODS_WARM_POD` unset or empty, `builds` gives each eligible runner
  its next turn in configuration order, skipping unreachable runners and full slots. Capacity
  comes from `build.sh ahead` and the runner's ping (`busy`, `heavy_runs`), not predicted finish
  times. When every reachable runner is full, the next runner holds the queued call. This path
  does not move a new call to another clone just because that clone already holds its commit.
  `reroute` reads queues on every runner and moves queued calls in batches to runners with
  shorter waits; it needs no warm pod. Runs already started and pinned runs stay where they are.
- **Optional: the warm pod for `builds`.** `PODS_WARM_POD`, a files path whose one line names
  the warm pod `builds` falls back to. Unset or empty, `builds` uses runner-only placement;
  duckpond's is empty.
- **The lane's first bound.** `PODS_LANE_FIRST_LIMIT`, whole minutes, which each lane the service
  starts states as `FIRST_LIMIT=`: build.sh bounds a run by twice the longest green run of its
  kind on that machine, and a machine with none yet (a new warm pod) refuses a call that states
  no bound. duckpond's is 40: twice the longest green lane measured, 1183 s of gates (programs,
  desktop run 20261022-080840-1474062; the warm pod's longest was 1146 s). Unset, the service's
  lanes state none.
- **Optional: the build runners.** `PODS_RUNNERS`, `<name>=<keys path>` words in tie order
  (`desktop=/desktop` when unset), each keys path a mounted Secret (files `id_ed25519`,
  `id_ed25519.pub`, `known_hosts`, `user`, and `hosts` or `forward`, optionally `kinds`):
  duckpond's are `desktop-runner` at `/desktop` and, for every other runner, `runner-<name>` at
  `/runners/<name>` (`zk-dev`, eddy's dev box, and the Latitude build VMs `lat-N`). A
  runner with a `forward` file is reached through `ducktape forward` (from `PODS_DUCKTAPE`,
  signed with the service's key from `blob-key`), so the image's CLI must carry that verb and
  the serving node's `forward.toml` must list the service's key among that service's
  `signers`. Without its Secret, a runner's pod answers 503.

## Stand one up for another network

1. Build and apply: `ops/k8s/pods/deploy NODE_URL FORGE DUCKTAPE_COMMIT`, where `NODE_URL`
   is a node `deploy` reads the network's block time from and `FORGE` is `duck://<chain id>/forge`.
   It builds the image, pushes it to the cluster node's registry and applies
   `pods.yaml`, with `hardcap` computed from the live cluster. The NetworkPolicies
   `node-only` and `to-duckpond-node`, `PODS_DUCKTAPE`'s and `PODS_NODE`'s node, the Service's fixed `clusterIP` and the deploy's `DEPLOY_NODE` and
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
