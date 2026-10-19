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

- `k8s/deploy NETWORK COMMIT [--found]` builds `k8s/Containerfile`: its
  first stage builds ducktape from `git archive COMMIT` with Rust 1.96.1 on
  Debian trixie, and the image runs that binary on trixie. It pushes it to
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
  the ducktape CLI built from `git archive COMMIT` the same way, kubectl and
  git-remote-duck, pushes it to the cluster node's registry, and applies
  `k8s/pods/pods.yaml`: the service `pods` in namespace `pods` at
  `http://10.43.80.80:8080`, and namespace `agents`, where it runs what agents
  ask for; a pod's inputs come from the node at `NODE_URL` and its forge
  `FORGE` (`duck://NETWORK/forge`). A network reaches it as the egress service `pods`
  (`[services.pods] base = "http://10.43.80.80:8080"` in the node's
  `egress.toml`); a NetworkPolicy admits only the duckpond node pod, so the
  `ducktape-chain`, `ducktape-program` and `ducktape-request` headers it reads
  are the node's. A call whose `ducktape-request` is not an egress request
  number is refused with 400, since the service asks egress about it.
- `k8s/pods/server.py` is the service, and `server.py fetch` the init
  container that fetches a pod's inputs. Its routes, under `/v1`: `pods`
  (create, list), `pods/NAME` (status, delete), `pods/NAME/logs`,
  `pods/NAME/wait`, `pods/NAME/exec`, `pods/NAME/blob`, `pods/NAME/file`, `pods/NAME/push`, `manifests` (apply,
  delete), `manifests/KIND[/NAME]`, `usage`, and `k8s/PATH`, which passes a
  Kubernetes API call through when PATH is inside namespace `agents`
  (`api/v1/namespaces/agents/...`, `apis/GROUP/VERSION/namespaces/agents/...`)
  and answers Kubernetes' own status and body. It checks what it creates or
  replaces as `manifests` does, and sends a PATCH first as a dry run, checks
  the object it would store, and only then sends it. `server-test.py` covers
  them.
- `pods/NAME/wait?every=SECONDS` and an exec that names `every` answer
  `text/event-stream`, which a network hears event by event
  (`Delivery::Events`): an `alive` event each `every` seconds, which moves
  the caller's egress expiry on, then one `ended` event and the end of the
  stream. A stream is chunked and ends with its last chunk only once its last
  event is sent, so one the service stops before then (a restart, a crash)
  reaches the node broken (`stream_broken`), never as a whole end; one that
  fails inside ends with `refused` `{status: 500, error}`. A wait ends when the pod's run container ends, the pod fails or is
  deleted, with `{line, exit_code, pod}`; an exec ends with its whole
  answer, or `refused` with `{status, error}` when the cluster refused it.
  So a build that outlives a request's `wait` still answers. `every` is the
  caller's: only it knows its request's `wait` and the time an event takes
  to reach its chain. A pod that is not there is refused before any stream.
- `pods/NAME/push` `{path, ref, have, container}` packs a commit of the git
  work tree at `path` in the pod for a forge push: `ref` (`HEAD` when left
  out) resolved to its commit, and every object it reaches but the commits
  in `have` and the pod's `refs/remotes` reach, which forge already holds.
  The pack is put as a blob of kind `pack`, and the answer names the
  `commit`, the `pack` blob (`null` when forge holds every object), its
  `size` and `objects`. The service pushes nothing: the caller sends forge's
  `Push` with the pack as its own account, so forge's write list decides as
  for any push.
- `pods/NAME/blob`, `pods/NAME/file` and `pods/NAME/push` try a step again
  when it lost its connection (kubectl's exec, a blob put, files' head or
  the files commit, which is made again onto files' head as read then): the
  pause doubles from a second, with no last attempt, while the egress
  request that asked is open (`egress` `Request { id }`, the
  `ducktape-request` header). Once it is gone the step is not tried again.
  Any other failure answers at once, and a failure the service did not
  foresee answers 500 with `{"error"}` rather than closing the connection.
  A files commit files refuses because its head moved after it was read
  (`not_found: files minted no snapshot …`, or `stale`) is made again onto
  files' head as read then, with the same pauses while the request is open.
  A commit whose submit the node took but whose wait for its block lost its
  connection is not made again: it answers 502, naming the frame and that
  whether a block carried it is not known.
- `pods/NAME/file` answers only after the block that carries its files
  commit (`ducktape submit --land`): 200 with `{written, size, chunks, base,
  height}`, `height` that block's; files refusing the commit in its block
  answers files' reason (`unauthorized` 403, `not_found` 404,
  `already_exists` and `wrong_state` 409, any other 422); a commit no block
  carried before its anchor aged out answers 502 that it never landed.
- An exec, `blob`, `file` or `push` that reaches a container which has not
  started yet (kubectl's `container not found ("run")`, while the `fetch`
  init container still fetches the pod's inputs, or `does not have a host
  assigned`, while the pod waits for a node; then the container watched is
  the one asked for, else the pod's default container) watches the pod and runs
  once that container runs, while the egress request that asked is open:
  each change the watch hears asks egress again, and once the request is
  gone it answers 502 and runs nothing. A container that ended, or a pod
  that failed, succeeded or was deleted, answers kubectl's failure at once.
- An exec kubectl could not take to the API server (`apiserver not ready`,
  `the server is currently unable to handle the request`, `ServiceUnavailable`,
  a refused connection to it) never reached its container, so it is sent again,
  the pause doubling from a second, while the egress request that asked is
  open; once it is gone the answer is 503 `apiserver unavailable: …` and
  nothing ran. An exec whose own stream was cut while its command ran
  (`websocket: close 1006`) is never run again: it answers 502 `apiserver
  unavailable: …`, saying the command may still be running in the pod. A
  streamed exec ends with either as its `refused` event.
- An exec runs its command once per egress request (the `ducktape-chain`,
  `ducktape-program` and `ducktape-request` headers). A node that restarted
  calls each request still open again, and that call waits for the command
  already running, or answers what it answered once it ended, its exit code
  included, streamed or whole. The service keeps that answer while egress
  says the request is open, asking again whenever an exec starts or ends,
  and forgets it once the request is gone. An answer whose request egress
  does not answer about stays until a later sweep can ask; it holds up no
  other answer. A restart of the service itself
  forgets every answer and ends each open stream with nothing more: the
  command goes on in the pod, but its answer is lost.
- The `run` and `out` containers both mount `/out` and every input at its
  path, so `out` reads, packs and pushes from an input after `run` ended.
- A forge input is a clone of the repository's branches, then a checkout of
  its commit. A commit no branch holds, such as the merge commit forge makes
  for a change, is fetched by its id between the two.
- `server.py fetch` tries a step again when it lost its connection (the
  clone, which starts again from an empty directory, the fetch of a commit
  by its id, a files read, a blob get), with the pause doubling from a second and no last attempt, for as
  long as the pod lives: the request that made the pod was answered when the
  pod was created, so no request bounds it, and deleting the pod ends it. An
  HTTP answer, or any other failure, ends the fetch at once. Each try again
  is a line in the fetch container's log (`pods/NAME/logs?container=fetch`)
  with the failure in the fetcher's words, and the pod stays in its init
  phase meanwhile.
- A pod the service makes shares one process namespace among its
  containers (`shareProcessNamespace`): the pod's pause process is PID 1, so
  it reaps every orphaned process, and no container's own process is PID 1,
  so a SIGTERM stops one that does not handle it.
- A pod's inputs and `/out` are generic ephemeral volumes of storage class
  `local-path`, so they sit on the disk that holds the cluster's
  PersistentVolumes, not on the node's root filesystem. Kubernetes creates
  each claim with its pod and deletes it with the pod. Each claim requests 1
  byte, the least the API accepts, because the service knows no input's size
  before the pod fetches it and local-path enforces no size. The kubelet
  measures a local-path claim as the whole disk under it, not the claim's
  own bytes, so `usage` gives no pod's share of that disk. A pod's
  `disk_bytes` is the kubelet's ephemeral-storage figure for it: its
  containers' writable layers, its logs and its emptyDirs. It leaves out
  the pod's inputs and `/out`. `volume_disks` holds, once per node, the used and
  capacity bytes of the disk under the pods' inputs and `/out`. `claims`
  lists only the claims no pod owns.
- A `scratch` input (`{"path", "scratch": {"medium": "Memory", "size"}}`)
  is an empty directory in the pod's memory (an `emptyDir` of medium
  `Memory`): nothing is fetched into it, and what is written there never
  reaches the node's disk. Its optional `size` is a Kubernetes quantity
  above 0, such as `48Gi`, and becomes the `emptyDir`'s `sizeLimit`; the
  service refuses any other `size` with a 400. The kubelet mounts the
  directory as a tmpfs of the smallest of that size, the pod's memory limit
  and the node's allocatable memory. A write past it fails with "No space
  left on device", and the pod keeps running. A scratch without a `size` in
  a pod without a memory limit is a tmpfs as large as the node's
  allocatable memory, so filling it can starve the node and get the pod
  evicted. What a pod writes there counts as the pod's memory, but
  `hardcap` counts only requests, so a pod that builds into a scratch
  states a memory request at least its scratch's size. The service still
  creates a pod whose `resources.requests.memory` is smaller than a
  scratch's `size`, and its reply (201, or 200 for the same request again)
  then carries `"warnings"`: one sentence per such scratch, saying the pod
  can be evicted. A scratch without a `size` in a pod with a memory request
  gets one too. A reply with nothing to warn of has no `warnings` field.
- A pod created with a `notify` (a chat channel, and optionally a thread in
  it) gets one post there when it ends, mentioning its `requester`: the run
  container's exit code, or why fetching its inputs or the pod failed. A pod
  without one gets none. Nothing else posts: `pods/NAME/blob` and
  `pods/NAME/file` answer with their result, so they post nothing.
- Namespace `agents` holds the ResourceQuota `hardcap`, the operator's
  ceiling: `requests.storage` 500Gi, and `requests.cpu` and
  `requests.memory` at the allocatable cpu and memory of the cluster's
  nodes minus what the duckpond node, ccs and tunnel Deployments request,
  summed over their containers and replicas. `k8s/pods/deploy` reads these
  from the cluster and prints the two values before it applies. It
  caps what pods are guaranteed, not what they burst to. Kubernetes enforces every quota in a
  namespace, so agents add their own quotas and LimitRanges beside it and
  none exceeds it. The service's
  account holds every built-in namespaced kind in `agents` except Secrets,
  service account tokens, Roles, RoleBindings, NetworkPolicies, Ingresses,
  status and the proxy and port-forward subresources; it only reads
  Endpoints, EndpointSlices and CSIStorageCapacities, and no custom
  resource. The ValidatingAdmissionPolicy `hardcap` refuses it any write of
  `hardcap`, and `agents-pods` refuses every pod in `agents` that runs at a
  priority class other than `agents` or mounts or projects a service
  account token. A container that states no cpu or memory gets the
  LimitRange `defaults`: a request of 0 and no limit, so `hardcap` admits
  it. `defaults` sets no default limit, since Kubernetes refuses a
  container limit above the pod-level limit a pod states. The PriorityClass
  `agents` ranks its pods below every other pod on the cluster. Its
  NetworkPolicy `isolated` admits traffic only from inside the namespace and
  lets pods out to public addresses and DNS.
- A pod in `agents` uses at most 50 CPUs; nothing sets it a memory limit.
  When a pod states no pod-level cpu limit, the MutatingAdmissionPolicy
  `agents-pods-cpu` sets its `spec.resources.limits.cpu` to 50 as
  Kubernetes creates it, and Kubernetes then sets its pod-level cpu request
  to what its containers request together: 0 when they state none, so
  `hardcap` admits it as before. The kubelet holds the whole pod to 50 CPUs.
  `agents-pods` refuses a pod whose pod-level cpu limit, or any container's,
  is above 50. A pod that states its own pod-level cpu limit keeps it, and
  Kubernetes sets its pod-level cpu request to that limit when neither the
  pod nor its containers state one. Both policies act on every pod created
  in `agents`, whether the service, a manifest or a controller creates it;
  a pod already running keeps the spec it started with.
- The service signs blob puts, files commits and chat posts as the program
  account `pods`, whose key and password are the Secret `blob-key` (files
  `key` and `password`) in namespace `pods`:

  ```sh
  kubectl -n pods create secret generic blob-key --from-file=key=KEY --from-file=password=PASSWORD
  ```

- `k8s/toolchain/deploy` builds `k8s/toolchain/Containerfile`, the image
  agents build ducktape, programs and the app with, pushes it to the
  cluster node's registry and prints its reference.

## duckpond's recovery daemon

- `k8s/pi-chief/pi-chief.py` is pi-chief, a daemon off the chain that holds
  two powers and nothing else. It rolls a program that fills no role back to
  the code it ran before a change, and it rolls the node Deployment's `node`
  container back to the image it ran before an image change. Everything
  else it reports. It ticks once per block time (`/v1/status`) and reads
  the node in the cluster at `node-http.duckpond.svc:8844`: `/v1/status`,
  `/v1/programs` and `/v1/blocks`. It signs only through the ducktape CLI,
  as its own agent account, which orthory (15) manages. The account's key
  and password are the Secret `key` (files `key` and `password`) in
  namespace `pi-chief`, created apart from the deploy:

  ```sh
  kubectl -n pi-chief create secret generic key --from-file=key=KEY --from-file=password=PASSWORD
  ```

- It acts only on a change it saw happen. At its first start, what runs is
  its baseline. The node trigger is an image change made while the chain
  was making blocks, not yet proven (the chain has not made `epoch_length`
  blocks since the rollout completed), and then a stall or the Deployment's
  `ProgressDeadlineExceeded`. A stall is no new block for the Deployment's
  `progressDeadlineSeconds`, read each tick, measured from the newest
  block's own time. The rollback is a strategic-merge patch of that one
  image, carrying the Deployment's `resourceVersion`. It is sent only after
  a server-side dry run that changes nothing but that image, and only to
  the image it last saw proven, which must also be the image of the newest
  older ReplicaSet with another image.
- The program trigger is a code change of a program that fills no role,
  after which the host refused at least one of its receipts, nested ones
  included, as `trap` or `protocol` and applied none, while the old code
  was seen applying ops. The rollback is a `Schedule` of `Set` back to the
  old code with the program's params, at the tip plus `host::ANCHOR_WINDOW`
  plus `programs::NOTICE`, so it is valid in whichever block carries it.
  It sends the `Schedule` with `ducktape submit --land` and counts the
  rollback made only when a block applied it, naming that block
  (`scheduled_in`). A `Schedule` refused in its block, or one no block
  carried before its anchor aged out (`unlanded`), is reported and holds
  nothing. Chat posts are sent the same way. It reports instead when the
  change was scheduled by account 18 or an agent 18 manages (the signer of
  the `Schedule` in the blocks, through identity), when it finds no such
  `Schedule`, when a change for the program already waits, or when the live
  `programs` code is not the one NOTICE was read from.
- A stall with the tip at a program's switch, or with a change due at the
  next block, is reported only: no block can carry a program change, and a
  node rollback does not change a program's code. So are a stall with no
  change to name and a failing program that fills a role.
- It reports in DMs to 15 and 17, one post per incident; with the chain
  down it logs to its pod log and posts once the chain answers. After any
  rollback it takes no other action until 15 or 17 reacts to that report.
  Its state (what it saw run, the open hold, unposted reports) is the
  claim `state`. Stop it with
  `kubectl -n pi-chief scale deployment/pi-chief --replicas=0`.
- `PI_CHIEF_MODE=report|act k8s/pi-chief/deploy DUCKTAPE_COMMIT
  PROGRAMS_CHECKOUT PROGRAMS_COMMIT ACCOUNT` builds
  `k8s/pi-chief/Containerfile` with the CLI from `git archive
  DUCKTAPE_COMMIT`, pushes it to the cluster node's registry and applies
  `k8s/pi-chief/pi-chief.yaml`: namespace `pi-chief`, service account
  `pi-chief`, a Role in `duckpond` (deployment `node` get, watch and patch;
  replicasets get and list; pods and their logs get, list and watch), the
  claim, a policy closing every inbound port, and Deployment `pi-chief`.
  It reads ANCHOR_WINDOW from that source and NOTICE from
  `PROGRAMS_COMMIT`, the commit the live `programs` code was built from,
  and stores the live code's id with it. In `report` mode every rollback
  runs as its dry run and the report says what it would do.
- `pi-chief.py dry-run node` and `pi-chief.py dry-run program PROGRAM CODE`,
  run in its pod, show a rollback without making it: the server-side dry
  run of the patch and what it changes, or the `Schedule` signed through a
  local stand-in node that passes on only `/v1/status` and keeps the frame.
  `k8s/pi-chief/pi-chief-test.py` covers it.

## Phone builds and simulator runs on a Mac

- `k8s/mac/relay.py` is the egress service `mac`, the relay: Deployment
  `mac` in namespace `mac`, at `http://10.43.80.81:8080`
  (`[services.mac] base` in the node's `egress.toml`). It holds agents'
  runs: a phone build of a forge commit (`make -C phone <target>` of the
  app), then, if the run has one, a UI script in the iOS simulator. Its
  routes, under `/v1`: `runs` (create, list), `runs/ID` (status, delete),
  `runs/ID/wait` (an event stream like pods' wait), and `mac` (whether the
  Mac is connected, since when, and what it said of itself). Every answer
  says whether the Mac is connected, so a call made while the laptop sleeps
  is answered at once and its run waits in the queue. A run's script and
  `media` files are read from files when the run is asked for, its outputs
  go into files at `<to>/<run>/` in one commit when it ends, and one chat
  post says it ended. It signs as the account `mac`, with the Secret
  `mac-key` (files `key` and `password`). Its state (`runs.json`, each
  run's inputs and outputs until written, mirrors of the repositories the
  Mac builds) is on the claim `state`. `relay-test.py` covers it.
- The Mac dials out to the relay's port 8081 at `http://100.65.253.102:8081`:
  the pod's `hostPort` on fabrics-1's tailnet address, as the node's 8844
  is, so the Mac's connection reaches the relay as one TCP connection end to
  end, and keepalive on it reads the far end. Every call there carries the token in
  the Secret `mac-token` (file `token`):
  `kubectl -n mac create secret generic mac-token --from-file=token=TOKEN`.
  `POST /v1/mac/work` is the Mac's event stream: it lists the runs the Mac
  holds, and the relay sends `run` and `forget` events on it. The Mac counts
  as connected while it is open. `POST /v1/mac/bye` ends it. Both ends of
  the stream set TCP keepalive (`KEEPALIVE` in `relay.py` and `mac.py`,
  and on the relay `TCP_USER_TIMEOUT` to match), so a peer gone without a
  word (the Mac off the network, fabrics-1 down hard) is heard as gone
  within half a minute: the relay says the Mac is away, and the Mac dials
  again. A relay the Mac can't reach is tried again when `power.swift` says
  the Mac woke or its network changed, and, while the relay is down with
  the network up, every 5 s (`RELAY_DOWN_PAUSE`, orthory's ruling), never
  doubling.
- Nothing the relay does is tried again on a clock. A files read, a
  forge step or a source bundle that fails answers its reason at once,
  and the next ask tries again. Outputs files did not take (a write not
  sent, chunks not put) stay on the claim, the run stays `writing`, and the
  next call about the run writes them again: an agent's read of it or wait
  on it, the Mac's report of its end, or the Mac connecting. A commit files
  refuses because its head moved is made again onto the new head at once.
  `POST /v1/mac/runs/ID/state`, `PUT /v1/mac/runs/ID/outputs/PATH` and
  `GET /v1/mac/runs/ID/inputs/N` carry a run's progress, outputs and media.
  `POST /v1/mac/source` answers a git bundle of a forge commit, less what
  the Mac already has, so the Mac needs no way into forge of its own.
- When the Mac connects, the relay sends it every queued run again. It marks
  `interrupted` each run it believed the Mac held that the Mac no longer
  lists, and tells the Mac to forget the runs deleted while it was away.
- `k8s/mac/deploy NODE_URL FORGE COMMIT` builds `k8s/mac/Containerfile`
  around the ducktape CLI built from `git archive COMMIT` and
  git-remote-duck, pushes it to the cluster node's registry, and applies
  `k8s/mac/mac.yaml`.
- `mac/mac.py` is the Mac's side, kept running by the LaunchAgent
  `mac/com.ducktape.mac.plist`. It needs Python 3.11 or later, Xcode with an
  iOS simulator runtime, XcodeGen, rustup's cargo first on `PATH` (so the
  app's `rust-toolchain.toml` holds) and Rust's iOS targets. Its work
  directory (`MAC_HOME`) holds the relay's token (`token`), a bare
  repository per repo filled from the relay's bundles, one cargo target dir
  per repo, and each run's worktree, outputs and `state.json`.
- A run's script runs against the simulator device `MAC_DEVICE`, which the
  operator makes once and signs the app in on, or a fresh device of the
  same type and runtime (`ducktape-run-<run>`), deleted when the run ends.
  Steps reach the app through its AX door (the `ax-door` build, whose token
  the Mac reads from the app's container), the device through
  `xcrun simctl`, and system UI (the photo picker, permission alerts)
  through the app's XCUITest driver (`make -C phone ios-driver`). Runs on
  the kept device take it in turn: a run waits, saying which run holds it,
  until that one's script has ended. Builds and fresh devices run side by
  side.
- `mac/power.swift` (`swift power.swift`) says on its stdout when macOS is
  about to sleep, and holds the sleep until `mac.py` has said goodbye to
  the relay, then says when it woke, and `mac.py` connects again. When
  `mac.py` starts, every run its work directory holds in a working state
  becomes `interrupted`, and fresh devices left behind are deleted.
  `mac/mac-test.py` covers it on Linux, against the real relay and git and
  a fake `xcrun`, `make` and `xcodebuild`.

## Forge and GitHub

- dev and main of ducktape, programs and app are held by duckpond's forge
  (repositories `core`, `programs`, `app`). GitHub's
  `ducktape-industries/ducktape`, `programs` and `app` are a read-only release
  mirror of them: forge-mirror fast-forwards GitHub's dev and main to forge's,
  GitHub Actions run CI and build releases from the mirrored commits, and the
  ruleset `k8s/forge-mirror/protect` puts on dev and main lets only the App
  write them.
- `k8s/forge-mirror/mirror.py` is forge-mirror, running on fabrics-1 as
  Deployment `forge-mirror`. It follows duckpond's forge writes over
  `/v1/changes/forge`. A block that moves a followed forge branch starts one
  job for that branch, and so does a start. A job fast-forwards GitHub's
  branch of the same name to forge's head with a plain push, so it never drops
  a commit. When GitHub's branch holds a commit forge lacks, the job pushes
  nothing to that branch and logs the warning `mirror_diverged` with both
  heads: the branch is held until a person makes GitHub's head one that
  forge's head contains. A person carries what GitHub's branch holds that is
  worth keeping onto forge through a forge change, then pushes forge's head to
  GitHub as the App, with `--force-with-lease` on GitHub's head, from inside
  the pod (`kubectl -n forge-mirror exec deploy/forge-mirror`), which holds
  the App's key. A push that loses a race with another GitHub push runs
  again. A held branch, and a job that fails otherwise, runs again at each
  later block that writes forge or the clock program (`FORGE_MIRROR_CLOCK`,
  `scheduler`), and logs its first attempt and every tenth with an `attempts`
  field. `k8s/forge-mirror/mirror-test.py` covers it.
- forge-mirror pushes over HTTPS as the GitHub App "ducktape forge-mirror"
  (App ID 5370470), with an installation token it mints from the App's key
  for the repositories `FORGE_MIRROR_REPOS` names, so the token writes only
  ducktape, programs and app. The Secret `forge-mirror/github-app` holds
  `app_id`, `installation_id` and `private_key`, nothing else.
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

- `worktree-clean.sh` — removes task worktrees whose work is on
  `origin/dev`, as ancestors or as one squash commit carrying the same patch;
  dry-run by default, `--yes` to act. It REFUSES a worktree that is dirty,
  carries work `dev` lacks, holds no commit of its own, or has live processes
  under it. `worktree-clean-test.py` covers those refusals.
