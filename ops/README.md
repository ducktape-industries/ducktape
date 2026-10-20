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
  volume is kept when the claim is deleted. Every `kubectl` call names the context
  `CONTEXT`, the current context unless set. It applies `k8s/storage.yaml`,
  the StorageClass `local-path-retain` every network's claim uses, only when
  the cluster lacks it.
  [`../docs/found-a-network.md`](../docs/found-a-network.md) walks a
  founding through these steps.
- With `--found` it then applies `k8s/found.yaml`, a pod `found` that holds
  `home` at `/data` and runs nothing, and with `EGRESS=FILE` copies FILE
  into it as `/data/egress.toml`. Found the network into it, then delete it:

  ```sh
  kubectl -n NETWORK exec found -- ducktape identity
  kubectl cp founding NETWORK/found:/data/founding
  kubectl -n NETWORK exec found -- ducktape init /data/founding/founding.toml --halo node.example.org:9100
  kubectl -n NETWORK delete pod found
  ```

- Without it, it applies `k8s/node.yaml`, and `k8s/tunnel.yaml` when the
  namespace holds the Secret `tunnel`.
  `node.yaml` is the deployment `node`, one pod running the node on `home`,
  and the Service `node-http` on the node's HTTP port, `8844`. The node gets
  each key of the Secret `egress`, when there is one, as an environment
  variable and as a file under `/secrets/egress/`, for `egress.toml`'s
  secrets. Its pod resolves names only through public resolvers
  (`dnsPolicy: None`), so `egress.toml` names a cluster Service by its
  ClusterIP, never by its DNS name.
- With `HOST=IP`, `deploy` binds the node's HTTP port at `IP:HTTP_PORT`
  (TCP) and its halo port at `IP:HALO_PORT` (UDP) on the machine that runs
  the pod, 8844 and 9100 unless set: a network reached only on a host
  address, such as a Tailscale IP, needs no tunnel. duckpond deploys with
  `HOST=100.65.253.102`.
- `tunnel.yaml`
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

## A network's first accounts

- `found/seed.sh` turns a freshly founded network into one with two agents:
  the account `founder` creates the agent `chief`, adds chief's own key to
  it and lists who chief answers; chief's key then registers and assigns
  the egress service, sets the model, writes chief's configuration, sets
  its script, and creates `tech` the same way, chief managing it. It sends
  each op as JSON with `ducktape op … --land` and has chief's workspace sign
  chief's consent with `ducktape wallet consent`. What each step printed
  stays in the step folder, and a rerun skips the steps that landed.
  `../docs/found-a-network.md` step 10 gives the call.

## An agent outside the network

- `bridge/bridge.py` is a chat bridge for one account that a process of
  its own signs for: it follows attribution's writes
  (`ducktape changes attribution`), reads each change addressed to the
  account with attribution's `Change { seq }` and each chat message with
  `MessageById`, starts the handler afresh for each message with
  `{"change", "message"}` on its stdin, and sends every line the handler
  prints as a chat op with `ducktape op chat … --land`. The handler runs
  without `DUCKTAPE_PASSWORD`. `STATE` keeps the last seq handled; on start
  it pages `ChangesTo` for what it missed. An op the network rejects is
  reported on stderr and its change counts as handled. It ends, stopping
  its `ducktape changes`, when the node closes the stream, the handler
  fails or `ducktape` can't reach the node, so whatever runs it starts it
  again. `../docs/found-a-network.md` step 14 gives the
  call, and `bridge/bridge-test.py` covers it against a stand-in
  `ducktape`.

## Agents' computing on Kubernetes

`k8s/pods/README.md` says what the pods service needs (its namespaces and
ServiceAccount, its own chain account, the egress binding) and how to stand
one up for another network.

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
- The reserved pod `desktop` is orthory's desktop build runner, a rootless
  podman container that runs the warm pod's `build.sh`. The service
  reaches it by ssh with the Secret `desktop-runner` in namespace `pods`
  (files `id_ed25519`, `known_hosts`, `hosts`, `user`; `hosts` lists, split by any whitespace, the
  addresses to try in order, and the one that last answered is tried first),
  whose key may only run the desktop's `gate` command. `pods/desktop` answers
  the runner's `ping` as a pod that runs (503 while it is down or
  unreachable); `pods/desktop/exec` runs only `build.sh`: the command
  `["sh", "/var/tmp/warm/build.sh", ...]`, or a script of plain words that
  calls it (KEY=VALUE words first, no expansion, operator or comment outside
  quotes), and refuses anything else with 400. An exec is sent to the next
  host only when ssh never connected; a session that broke once it ran
  answers 502 and is not run again, while reads try the next host. `blob` and `file` read through
  `gate get` (paths under `/out/` and `/var/tmp/warm/`), `push` through
  `gate resolve` and `gate pack` (which run `RESOLVING` and `PACKING`
  verbatim: changing those scripts means telling the operator, who keeps the
  gate's copy), `logs` through `gate logs`, and `wait` follows the runner's
  log from then on and ends when that stream does. A streamed exec and a wait keep the ssh
  connection alive at their own `every` (`ServerAliveInterval`). It is never
  deleted. With no Secret mounted every call to it answers 503. An exec whose
  words name `NOTIFY=<channel>` starts one follower of the runner's log, and so
  does the service's own start when the Secret is mounted (kept
  for good, and joined again a block's time after it breaks, from the time the
  last stream that delivered a line began, or at once when the next such exec
  comes; a join that delivers no line keeps that window and doubles the
  pause). Its first window reaches back to when the runner started (its
  ping's `started`, podman's StartedAt: date, time with up to nine fraction
  digits, offset, zone), so a run that ended while the service was down, rolled
  or restarted still posts its end; before any post, chat is asked for its message id
  (`MessageById`, the line's hash), and one chat already holds is not
  offered again. Before that first window is followed, one read of the log
  over it finds the newest line chat holds, asking from the newest line
  back, and every line up to it counts as posted with no ask of its own
  (`pods_caught_up already_posted= asked=`; the follower posts in log order,
  each line ended before the next, so a held line means every older one was
  settled; a log it can't read, or a chat ask that fails during the walk,
  logs `pods_uncaught` with why, marks nothing, and each line is asked as
  it comes). A line refused for any reason (an archived room, a room or post
  chat refused) is final: it is not offered again, not even after a restart. Of the rest, each stream's end line (`pods_unfollowed`) counts them
  as `already_posted`, with no line of its own for each. While the ping names no start, or one not in
  that shape, it reads from then on, logs `pods_unresumed` with why, and asks again on the next join. Each `ducktape-notify`
  line is a chat notice in its channel (subject `desktop`, the line as its
  detail, grouped by the run it names as `desktop:<run>`), for the warm pod's
  requester (the pod `PODS_WARM_POD` names; for no one while none is named,
  or while that pod can't be read, which logs `pods_unconcerned` with why),
  posted as `pods` where the channel exists, is not archived and is open to anyone
  (a desktop run has no requester to vouch for a members-only room).
- The reserved pod `builds` routes a build.sh call to the machine where it
  waits least: `desktop` when its `ping` says it is up, its `busy` reads (a
  list of `build.sh busy` lines, `busy_exit` 0) and
  no quiet run or stray lock holder is among them, and its `build.sh ahead
  <the call's words>` (`ahead <lane> <n>`: the lane the call runs in and how
  many runs in flight there it would wait for) is 0, or no more than the
  warm pod's (the warm pod the files path `PODS_WARM_POD` names, duckpond:
  `/shared/tools/warm-pod`), else the warm pod. A machine that can't say
  its wait leaves the run to the other, and a desktop that can't say its
  wait, or that its ping finds busy, keeps the run when the warm pod can't
  be named (an empty or unread warm-pod file, as while the warm pod is
  recreated) or can't say its own wait (a name of a pod that is gone):
  only a desktop that is down then leaves a call nowhere to run (503, or
  the file's 502). A desktop run whose session was cut while no warm pod
  is named answers 502 with both reasons, naming the run when it started
  (`pods_unrouted`). A call naming a commit id (7 to 40 hex digits, an
  apply's base too) asks the picked machine's `build.sh holds <repo>
  <commit>` (its clone, no fetch); when that machine lacks it and the other
  holds it, the run goes to the other (`pods_routed reason=holds_commit`), so
  a commit made on one machine (an apply not pushed yet) builds where it
  is; when neither holds it the run goes where it waits least and fetches
  it from forge, and a commit forge lacks too ends saying to push it
  first. `reroute` (build.sh's
  words `reroute` alone) moves the warm pod's queued runs to the desktop
  while the desktop would start them sooner: the warm pod's `build.sh
  movable` (the queued runs another machine may take, in lane order: the
  read-only verbs, and an `sh` or `shot` whose call set `MOVABLE=1`), the
  desktop's `build.sh lanes`, one desktop `build.sh take` of every run it
  moves (after following the desktop's log when a moved run names NOTIFY,
  so its end posts), and one warm `build.sh moved` ending each there and
  naming its replacement, asked once more for the runs its answer leaves
  out. A run that started meanwhile stays, and its desktop copy ends with
  `build.sh yield`, posting no end. A handful of execs, however many runs,
  and one line per run moved or kept with why: `unknown` names both runs
  and why when the warm pod never says, `unstopped` a copy yield did not
  end, a copy that had already ended is named so (both runs then post an
  end), and
  a take cut mid-call answers 502 naming the runs it may have started. A builds call that finds the desktop's lane free reroutes
  once it is sent, one reroute at a time (`pods_rerouted`, or
  `pods_reroute_failed` with its sentence). A call whose request egress says is gone runs nothing (502), and one egress can't be asked about runs and logs `pods_request_unchecked`. builds keeps each run it starts under the call's words (settings included), so a later call of the same words (a retry whose answer never came back) gets that run while the run's machine's `build.sh claim` says `same` (queued or running, the same call and JOB, and a branch it names still at the run's commit), following `moved` to the run that stands for it, and also while that machine can't be asked or forge can't say (`pods_run_claimed`; a `WAIT=1` call then waits on it); otherwise it starts its own (`pods_run_unclaimed`). As exec starts and ends wake the sweeper, one `build.sh ended` per machine holding such runs drops each that ended and follows each moved, keeping those of a machine it can't ask, and dropping those of a pod that is gone. Its exec takes build.sh's words as
  `desktop` does; the answer's `run <id>` reads `run <machine>:<id>` and its
  stderr starts `builds: on <machine>`. A call to any other pod that
  answers 404 names the warm pod `PODS_WARM_POD` names now, when that is
  another pod (`if pod X was the warm pod, it has been replaced: the warm
  pod is now Y`), so a caller holding a replaced warm pod's name learns
  the new one. `status`, `log`, `stop`, `wait` and
  `requeue` name their run as `<machine>:<run>` and go to that machine, or
  by its bare id (as build.sh's `read it with:` line and NOTIFY name it),
  and go to the first machine, the desktop then the warm pod, whose
  `build.sh status` of it starts with its `job` line (a bare `status` answers
  that read itself): 404 only when every machine answered `no run`, else
  503 naming each machine that could not be asked or answered otherwise;
  calls that act on one machine (`busy`, `bump`, `prune`, `jobs`,
  `heavy-runs`, `disk-targets`, `queue-out`, `queue-in`, `self-update`,
  `worktree`, `measure`) are refused with 400, since they name a machine. A run the
  desktop never started (unreachable, down) goes to the warm pod. A held run
  (`WAIT=1`, or a streamed exec) whose desktop session was cut runs again on
  the warm pod when it only reads (`gates`, `lint`, `check`, `lane`,
  `lane-test`, `test`, `show`, `grep`, `schema-diff`), and any other is
  parked: 502 naming its run (`desktop:<id>`, when the run had started) and
  `build.sh status` of it to read once the desktop answers, before calling
  it again, and the event `pods_parked`. Routing logs `pods_routed` with a
  token (`desktop_unready`, `desktop_busy`, `desktop_lost`,
  `desktop_unasked`, `warm_unasked`, `shorter_wait`) and the
  sentence in `detail`. `GET pods/builds` is refused: it is no pod.

  ```sh
  kubectl -n pods create secret generic desktop-runner --from-file=id_ed25519=KEY \
    --from-file=id_ed25519.pub=KEY.pub --from-file=known_hosts=KNOWN_HOSTS --from-file=hosts=HOSTS --from-file=user=USER
  ```

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
  `commit`, its `parents`, `commits` (how many commits the pack carries:
  the commit and every one under it that forge does not hold), the `pack`
  blob (`null` when forge holds every object), its `size` and `objects`.
  The service pushes nothing: the caller sends forge's
  `Push` with the pack as its own account, so forge's write list decides as
  for any push.
- `pods/NAME/blob` `{path | paths, kind, container}` puts a file the pod
  wrote as a blob of `kind` and answers its `blob` id and `size`; with
  `paths` it puts every one of them in one bundle and answers `blobs`, a
  `{path, blob, size}` for each in the order asked, so a round of programs
  is one call. Both answer the bundle (`bundle`, `put_at`, `held_since`).
- Every retry pause starts at the network's block time, `block_time_ms` of
  `/v1/status` on the node at `NODE_URL`, which the service reads once as it
  starts (it stops, saying why, when that read fails) and hands each fetch
  container as `PODS_BLOCK_MS`. A `Fetcher` made with no `block_ms` (the
  tools repo's warm scripts call it so) reads it from its own node.
- `pods/NAME/blob`, `pods/NAME/file` and `pods/NAME/push` try a step again
  when it lost its connection (kubectl's exec, a blob put, files' head or
  the files commit, which is made again onto files' head as read then): the
  pause doubles from the network's block time, with no last attempt, while the egress
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
  the pause doubling from the network's block time, while the egress request that asked is
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
  by its id, a files read, a blob get), with the pause doubling from the network's block time and no last attempt, for as
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
  above 0, such as `48Gi`, whose exponent fits the int32 Kubernetes reads it
  into, and becomes the `emptyDir`'s `sizeLimit`; the service refuses any
  other `size` with a 400. The service reads a quantity as Kubernetes holds
  it: rounded up to a whole nano, and at most 2^63-1. The kubelet mounts the
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
  gets one too. A pod-level `resources.limits.memory` bounds the pod's
  memory, scratch included, so a full scratch then fails one write or gets
  one process OOM-killed instead of the pod evicted. Because the kubelet
  sizes each memory scratch to that limit at most, a scratch without a
  `size`, or scratches whose sizes add up to the limit or more, leave the
  pod's processes no memory once full; the reply warns of each, saying the
  processes are OOM-killed. A reply with nothing to warn of has no
  `warnings` field.
- A pod created with a `notify` (a chat channel, and optionally a thread in
  it) gets one chat notice there when it ends (chat `Notify`: kind `pod`,
  group and subject the pod, status passed or failed by the run container's
  exit), for its `requester` alone: the run container's exit code, or why
  fetching its inputs or the pod failed. While its run container runs, each
  line it prints that reads `ducktape-notify <channel> <text>` is a notice
  for the requester in that channel too, with the pod as its subject and
  `<text>` as its detail, grouped by the run the text names (`<pod>:<run>`
  for `<run> ended: …` and `run <run> lost with pod …`, else the pod), so
  nothing the service posts can pass for a person or a program; a line is
  posted once, under an id from the pod and the sha256 of the channel and
  the text. Before the service follows a running pod's log (on the pod's
  start, or the service's own), one read of that log finds the newest line
  chat holds, asking from the newest line back, and every line up to it
  counts as posted with no ask of its own (`pods_caught_up pod=
  already_posted= asked=`, as for the desktop; a log it can't read, or a
  chat ask that fails during the walk, logs `pods_uncaught` with why, and
  each line is asked as it comes). A pod that ends before the service saw it run, or while the
  service was down, has its log read before its end post. Every post, the
  end post too, goes only where its requester may post, as chat decides
  (the channel is not archived, and is open, or the requester owns it or
  is a member; asked of chat before each post): a pod with no requester
  posts nothing. A pod without a `notify` gets none of either. A post the
  node doesn't answer (its room query or its submit fails with a 5xx or a
  dropped connection, or the frame's landing isn't known) is made again,
  the pause doubling from the network's block time with no last attempt
  (`pods_retried` on the first try and each power of two), until the node
  answers; the message id makes a repeat a no-op. A definite answer
  (no channel, archived, members only, not a member, rejected) ends it at
  once. Nothing else posts: `pods/NAME/blob` and
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
- `k8s/git-remote-duck DEST` is the one pin of git-remote-duck that every
  image takes (toolchain, pods, mac, forge-mirror): a release name, the
  network blob that holds its x86_64 Linux binary and that binary's sha256.
  It reads the blob from duckpond's public node (`/v1/blob/get`, or the node
  `DUCK_NODE` names), checks the sha256 and writes the binary to `DEST`. The
  same bytes, with a `SHA256SUMS`, are in files at
  `/shared/releases/git-remote-duck/<commit>/`. A new release is built from
  qa's `feat/duck-push` at that commit, put as a blob (pods `/blob`) and
  copied there, and this file names it.

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

## duckpond's alarms

- `k8s/watch/watch.py` is watch, a read-only daemon off the chain. It
  samples every 10 s and posts to chat channel `alerts` once when a check
  turns bad and once when it is good again, never while a check stays as
  it was. Its checks:
  - the block rate over the last 40 s, from the height in the node's
    `/v1/status` at `node-http.duckpond.svc:8844`: bad under 0.7 blocks/s;
    a sample the node does not answer starts the window again;
  - the node's `/v1/status`, and the public one at
    `duckpond.ducktape.industries` while the node's answers;
  - harness's `Budget`: bad while any account or window has an Anthropic
    status other than `allowed`;
  - ccs, when `WATCH_CCS` names it: bad when `/ccs/status` gets no answer
    or answers 500 or more;
  - the Kubernetes API's `/readyz` and, while it is ready, the `Available`
    condition of Deployments `duckpond/node`, `duckpond/ccs`,
    `duckpond/tunnel`, `pods/pods` and `pi-chief/pi-chief` (not the
    registry: a registry that fails shows as a roll that cannot pull);
  - the node volume's free bytes as `df` counts them (`f_bavail`), in two
    rows: bad under 10 GB and under 6 GB, the node-volume plan's heads-up
    and fallback lines; fabrics-1's root disk, which holds the kubelet's
    nodefs, bad when its free share falls under the kubelet's
    `evictionHard` `nodefs.available`; and `rlm-home`, the kubelet's image
    filesystem, bad at its `imageGCHighThresholdPercent` as the kubelet
    counts usage. Each is read with `statvfs` on an empty host directory
    on that filesystem, mounted read-only, so the pod sees no file there.
- A check it cannot read changes nothing, and logs `check_unread` once as
  it turns unread: with the node down, the rate and the public URL are
  unread. It saves what it decided to its claim `state` before it posts,
  and posts with `ducktape op chat …` as its own agent account, which
  orthory (15) manages, without waiting for a block: every CLI call ends
  within one sample period or counts as unread, so a chain that stalls
  while its RPC answers never holds up a sample. Each post is read back by
  its message id in the confirmed state before it is sent, so a post that
  landed, or landed before the pod stopped, is never sent again; one that
  no block carries is sent again each sample under the same id. An alert
  that no block carries by the next sample, or that cannot be sent, is
  also handed once, as its text on stdin (after the first chat call in a
  flush fails, the rest of that flush is paged without calling chat), to
  the
  command `WATCH_PAGER` names (again only after that command fails, with
  `page_failed` logged at the first failed page and every tenth); with
  none named it logs `page_unsent`. An alert that cannot be posted logs
  `alert_unposted` at its first attempt and every tenth. The account's key
  and password are the Secret `key` in namespace `watch`:

  ```sh
  kubectl -n watch create secret generic key --from-file=key=KEY --from-file=password=PASSWORD
  ```

- `WATCH_ROOT_DISK=DIR WATCH_NODE_DISK=DIR WATCH_HOME_DISK=DIR
  [WATCH_CCS=URL] [WATCH_PAGER=COMMAND] k8s/watch/deploy DUCKTAPE_COMMIT`
  checks that every
  Deployment `k8s/watch/watch.yaml` names runs, reads
  `imageGCHighThresholdPercent` and `evictionHard` `nodefs.available` (a
  whole percent) from the kubelet's `configz`, builds
  `k8s/watch/Containerfile` with the CLI from `git archive
  DUCKTAPE_COMMIT`, pushes it to the cluster node's registry and applies
  `k8s/watch/watch.yaml`: namespace `watch`, service account `watch`, a
  Role in `duckpond`, `pods` and `pi-chief` that may get only
  those Deployments, the claim, a policy closing every inbound port, and
  Deployment `watch`. Each DIR is an empty directory made beforehand on
  that filesystem. ccs's NetworkPolicy `duckpond/ccs-node-only` admits only
  the node, so `WATCH_CCS` is set once it also admits namespace `watch`.
- `watch.py once`, run in its pod, prints one sample's readings and posts
  nothing. `k8s/watch/watch-test.py` covers it. Stop it with
  `kubectl -n watch scale deployment/watch --replicas=0`.

## The registry's garbage collection

- `k8s/registry-gc/gc.sh` is one run of the CronJob `registry-gc` in
  namespace `registry`, every 5 minutes, the kubelet's image-GC period. It
  reads `rlm-home`'s usage with `statvfs` on the registry's claim, which
  lives there, and counts it as the kubelet does. Under the kubelet's
  `imageGCHighThresholdPercent` it stops. At or over it, it runs
  `registry garbage-collect --dry-run`, and stops when no blob is eligible
  for deletion. Otherwise it adds `REGISTRY_STORAGE_MAINTENANCE_READONLY`
  (`{"enabled":true}`) to Deployment `registry` and waits for the rollout,
  so a push is refused while it collects. Then it runs
  `registry garbage-collect` and removes the variable again, which also
  restarts the registry and drops its cached blob metadata. It never passes
  `--delete-untagged`: it deletes only blobs that no manifest in the store
  references, so every image the store still holds, tagged or not, stays
  whole.
- One run acts at a time: each run first takes an `flock` on the claim's
  directory, and a run that finds it held logs `another_run` and stops, so
  a run by hand never meets a scheduled one. Once read-only is on, any
  failure or a stop still removes the variable, after a running collection
  ends. A run that finds the variable still set removes it before anything
  else. A failed run is not retried; the next period runs again.
- Each step logs one `registry-gc: event=<name>` line: `another_run`,
  `read_only_left_on`, `under_line`, `nothing_to_collect`, `collecting`
  (with the dry run's blob count and bytes), `read_only_on`,
  `read_only_off` and `collected` (with the blobs deleted and the bytes
  the store shrank by while read-only). `line_unread` and `dry_run_unread`
  end a run whose line or dry run does not read.
  `k8s/registry-gc/gc-test.py` covers it.
- `k8s/registry-gc/deploy` reads the registry pod's image digest, the
  cluster's version and node architecture and `imageGCHighThresholdPercent`
  from the kubelet's `configz`. It builds `k8s/registry-gc/Containerfile`,
  the registry's own image with the matching kubectl and `gc.sh`, pushes it
  to the cluster node's registry, and applies `k8s/registry-gc/registry-gc.yaml`
  after a server-side dry run: service account `registry-gc`, a Role that
  may get, list, watch and patch only Deployment `registry`, and the
  CronJob, which mounts the claim `registry`. It changes neither the
  Deployment nor the claim.
- Run it once by hand, at the line it uses, with
  `kubectl -n registry create job --from=cronjob/registry-gc registry-gc-by-hand`.
  Stop it with
  `kubectl -n registry patch cronjob registry-gc -p '{"spec":{"suspend":true}}'`.

## duckpond's desired infrastructure

- Forge repo `infra` holds what fabrics-1 should run, one file per duty; a
  merge into its `main` (protected, so only merges move it) is the
  decision. `k8s/reconcile/reconcile.py` is reconcile, a daemon off the
  chain that applies it. Its duties are `egress/fabrics-1.toml`, the
  node's own `egress.toml`, and `pods.toml`, the pods service's image.
- On duckpond a new egress service goes into `infra` first, as a change to
  `egress/fabrics-1.toml`, and reconcile applies it once it merges. Nobody
  edits the node's `/data/egress.toml` or runs `ducktape egress reload` by
  hand: a hand edit leaves `infra` behind what the node serves, and the next
  `main` reconcile applies undoes it.
- Each block time it reads `infra`'s `main` through forge. For every open
  change into `main` whose head changes that file, it posts a commit status
  `reconcile/egress` and a review comment, once per head: refused when the
  file is not UTF-8, does not parse or is not shaped as `egress.toml` is,
  when a service carries `secrets`, which the node never takes through
  `egress set` (a service that needs a secret sits behind a proxy in the
  cluster that holds it), else the node's own dry run (`ducktape egress
  set --dry`), refused when a base does not answer from the node.
- On a new `main` it plans the file the same way and, when the node would
  change, sends it once (`RECONCILE_MODE=act`) with `ducktape egress set`,
  signed by its account's key, which the node's workspace file
  `egress-grants` lists. It plans it again at once: a base that no longer
  answers puts back the file it applied before. It posts each outcome to
  chat channel `infra`: applied, refused, rolled back, and proven when the
  egress program's row for the node lists what the file serves. It never
  sends a refused file again; a new `main` is a new try. A node that does
  not answer (the CLI says no `refused: <reason>:`) is no refusal: the
  `main` stays unseen and the next tick plans it again. A set whose answer
  was lost is sent again, the same file twice, before it posts applied, and
  rolls back to the file it replaced if a base no longer answers. With
  `RECONCILE_MODE=report` it does all of this except the set that is not
  dry: it posts what it would apply and leaves the node as it is. A start
  in another mode than its state was saved in follows `main` again, so a
  switch to `act` applies the `main` that report mode only reported. Its state
  is the claim `state`, saved before it posts; each post is read back by
  its id before it is sent. A post that cannot be posted logs
  `report_unposted`, and a tick that cannot read forge, the node or
  Kubernetes logs `unread`, each at its first attempt and every tenth; a
  good read logs `read` once.
- `pods.toml` holds one line, `image = "localhost:5000/pods:<tag>@sha256:<digest>"`,
  a digest already pushed (`k8s/pods/deploy` builds and pushes it). An open
  change to it gets status `reconcile/pods`: refused when the image is not
  that shape, when infra's egress file serves no `pods` (the proof below
  could not run), or when a server-side dry-run patch changes more than the
  `pods` container's image and its `PODS_OUT_IMAGE`. A new `main` that
  changes the image is patched once (strategic merge, with the
  Deployment's `resourceVersion`); reconcile then follows the rollout on
  the Deployment's watch stream. It is proven when the rollout completes,
  the node's dry `egress set` says `pods` answered from the node, and the
  running pod's `imageID` is the declared digest; the post in `infra`
  mentions `RECONCILE_PROBER` (17), who runs the functional probe (exec, a
  tmpfs scratch, a notify post) with the pod tools, since nothing off the
  chain reaches the pods service. A failed proof, or a rollout past
  `progressDeadlineSeconds`, patches back the image it replaced. A digest
  the registry lacks fails to pull while the old pod serves, so it costs
  that deadline and rolls back by itself. A tick waits on the rollout, so
  egress waits behind a roll. The proof's dry `egress set` is refused while
  infra's egress file carries secrets, so a roll proves only once it
  carries none.
- reconcile writes no `PAUSED`: the infra merge is the only gate for when
  to roll. It reads no node condition, which would need a ClusterRole. A
  roll under MemoryPressure still keeps the service: `deploy/pods` is
  RollingUpdate with one replica (25% / 25%: one surge pod, none
  unavailable), so the surge pod waits Pending on the memory-pressure
  NoSchedule taint while the old pod serves, and the Deployment's
  `progressDeadlineSeconds` rolls it back.
- `RECONCILE_MODE=report|act k8s/reconcile/deploy DUCKTAPE_COMMIT` checks
  that Secret `reconcile/key` exists, builds `k8s/reconcile/Containerfile`
  with the CLI from `git archive DUCKTAPE_COMMIT`, pushes it to the
  cluster node's registry and applies `k8s/reconcile/reconcile.yaml`:
  namespace `reconcile`, service account `reconcile` with a Role in
  `pods` (get, watch and patch deployment `pods`; list pods), the
  claim, a policy closing every inbound port, and Deployment `reconcile`. Its account must be on `infra`'s write
  list to post statuses. `k8s/reconcile/reconcile-test.py` covers it.
  Switch the mode by running `deploy` again with the other `RECONCILE_MODE`.
  Stop it with `kubectl -n reconcile scale deployment/reconcile
  --replicas=0`.

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
- With `MAC_LABELS` (mac.yaml: `mac macos-aarch64`), the relay is also an
  actions runner for the Mac, as account `mac`: it registers those labels,
  follows `ducktape changes actions`, and while the Mac is connected and
  holds no run claims the oldest queued check whose `runs-on` they cover
  (passing over one another runner took, or whose job's `runners` does not
  name `mac`: that refusal is final for the check, so the relay asks once,
  logs it once, and forgets the id when actions no longer queues it).
  The check becomes a run `check-<id>`: the Mac runs the job's steps with
  `sh -c` in a worktree of its commit, with `OUT` (what it keeps) and
  `NEEDS` (what the jobs it needs produced, at `NEEDS/<job>/<path>`, read
  from actions `Query::Needs` and `Query::Outputs` and handed as inputs).
  When it ends, `log` goes into files at
  `/shared/actions/<repo>/<commit>/<workflow>/<job>-<attempt>/log`, every
  other output is put as a blob of kind `output` in one bundle and recorded
  with actions `Outputs`, and the check is reported: passed, failed when a
  step exited non-zero, errored otherwise. A check that ends elsewhere
  (superseded) is forgotten on the Mac; deleting its run errs it. The Mac
  holds no key and the relay signs nothing but what `mac` already did, so
  any commit builds; signing is the separate signer's (J-074). The relay
  reads actions once per height `ducktape changes` prints (its first
  word), and follows again at once when the follow ends after hearing
  something, else after `FOLLOW_PAUSE` (5 s, as the Mac's
  `RELAY_DOWN_PAUSE`, chief's ruling; `mac-test.py` holds the two equal),
  saying why it ended. A check whose report did not
  send keeps its run `writing`, holds the Mac, and is reported again by the
  next pull; deleting an unended check's run reports it errored first,
  whatever the Mac had said of it, and a delete lets the Mac take the next
  check. A
  run's view carries `check`, and the Mac's run event `job`, only for a
  check's run.
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
  Steps reach the app through its inspector (`/inspect/*` on the `ax-door`
  build's door, whose token the Mac reads from the app's container): find,
  wait, press, type and scroll by the view's own ids, a press or typing
  wheeling a clipped node into view first; the screen itself through
  `changed`, which compares the simulator's frames with the last
  screenshot until one differs and the next frame repeats it, or its
  `deadline_ms` passes; the device through
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

## Signing on a Mac

- `signer/signer.py` is the signer: an actions runner of its own account
  (`signer-eddy` on eddy's Mac) whose labels are signing capabilities,
  `sign:<identity>` (`sign:dev.ducktape.app`). Build and sign are separate
  (eddy, #engineering 1230/1232): the keys stay in that Mac's keychain with
  the signer's own tools, and the signer never runs a check's commands or
  its commit's code. It registers its labels, follows `ducktape changes
  actions` as the mac relay does (once per height it prints, again at once
  after a follow that heard something, else after `FOLLOW_PAUSE`), and
  claims the oldest queued check its labels cover, passing over one whose
  job's `runners` does not name it: that refusal is final for the check,
  so it asks once, logs it once, and forgets the id when actions no longer
  queues it.
- A signing check's `steps` is one operation's name from the config's
  `tools`: `developer-id`, `testflight` or `release-manifest`. Before any
  tool runs it checks, from the network alone, that the check's commit is
  reachable from a branch the config allows for that repo and operation
  (forge `Compare { from: commit, into: refs/heads/<branch> }` with
  `ahead` 0; `release-manifest` takes the branches of the channel the job's
  `CHANNEL` names: stable from `main`, nightly from `main` or `dev`, chief's
  ruling), that every job it needs (`Query::Needs`) was met by a passed
  check of the same run and commit reported by a builder the config
  trusts (`mac`, account 44, and `actions-pods`, 38) or by the signer's
  own account, whose `Outputs` that builder recorded, and that each blob it fetches hashes to its id and
  holds the size its row says. Anything else errs the check, saying which,
  and signs nothing.
- The tool runs as `<tool…> <inputs> <out>`: the needed outputs at
  `inputs/<job>/<path>`, and what it writes in `out` is put as blobs of
  kind `output` in one bundle, recorded with `Outputs`, beside
  `signed.json` (the signer's account and labels, its own sha256, the
  operation, repo, commit, branch, check, and each input's and output's
  sha256; never a tool's command line), and the check is reported passed. A tool that exits non-zero
  fails it, saying only its exit code: what a tool prints may hold a
  secret, so it stays in `logs/<check>.log` in the signer's home, never on
  the network. The tool sees `SIGNER_REPO`, `SIGNER_COMMIT`,
  `SIGNER_BRANCH`, `SIGNER_CHECK`, `SIGNER_INPUTS` (a JSON file naming each
  input's job, check, path, blob id and sha256) and the job's `env` as
  `SIGNER_ENV_<NAME>`, never the signer's duckpond key. A tool that writes
  `publish.json` in `out` asks the signer to publish with its own key
  before it reports, within what the config's `publish` allows for that
  repo and operation (`files`, a files root, and `tags`, a tag-name
  prefix; an operation with neither publishes nothing): `files` (each
  `{path, from}`, an absolute files path under the root and a file under
  the check's work dir, committed in one files commit, made again on a
  head that moved) and `tag` (`{name, message, time}`: an annotated tag
  on the check's commit in the check's repo, its name under the prefix and
  one forge takes as a ref, tagged by the signer's account at that unix
  time, pushed as a one-object pack with forge `Push` and read back with
  `RefNames`, since forge applies a push whose ref it refuses and says so
  only in its report). Everything is checked, and the tag computed and
  read, before anything is written, so a refused publish writes nothing.
  The same tag already there is done; another one errs the check with
  both oids: keep the tag that is there, or let the repo delete it,
  delete it and rerun. A tool's tag name, message and time come from the
  check (its commit, its committer time, its inputs), never the clock, so
  a rerun after a lost receipt makes the same tag. `publish.json` is not
  an output; what was published (each file's path and sha256, the tag's
  name and oid) is in `signed.json`'s `published`. An Outputs or Report that did not send
  is kept in `pending.json` and sent before anything else is claimed; its
  first failure is logged with its reason, and its sending with the
  attempts it took. The check it is about to claim is in `claimed.json`
  until its report is owed: anything that breaks while it holds a check
  errs that check and clears its work. A claim whose receipt was lost
  keeps the file and stops the pull. At start and before each claim, a
  check left there is read back with `Query::Check`: one still running
  with the signer's `account` as claimant (a reboot mid-tool, a claim that
  landed after all) is reported errored, and anything else is let go. It pulls once each time a
  follow starts, so a check queued while the Mac slept is claimed when it
  wakes. A follow that keeps failing, and pulls that cannot read actions,
  are each logged once, and their recovery with the attempts it took. `signer-test.py` holds `FOLLOW_PAUSE` equal
  to the Mac's `RELAY_DOWN_PAUSE`.
- Its home (`SIGNER_HOME`, by default `~/Library/Application
  Support/ducktape-signer`) holds `keys/key` and `keys/password` (the
  account's duckpond key), `config.json` and `signer.log`. A home with no
  `config.json` ends the signer, saying so, and the LaunchAgent does not
  start it again; `launchctl kickstart` it once the config is in place. A
  `config.json` that is there but does not read (caught mid-save) fails
  the signer, so launchd starts it again and it reads the file again. `SIGNER_DUCKTAPE`
  is the CLI it runs (the plist: `ducktape --workspace <home>/workspace
  --node <node>`):

  ```json
  {"labels": ["sign:dev.ducktape.app"],
   "builders": [44, 38],
   "repos": {"app": {"developer-id": ["main", "dev"], "testflight": ["main", "dev"],
                     "release-manifest": {"stable": ["main"], "nightly": ["main", "dev"]}}},
   "tools": {"developer-id": ["/path/to/developer-id"], "testflight": ["/path/to/testflight"],
             "release-manifest": ["/path/to/release-manifest"]},
   "publish": {"app": {"release-manifest": {"files": "/shared/releases/app/", "tags": "release/"}}}}
  ```

  Every answer the signer checks, blobs included, comes from the node in
  `SIGNER_DUCKTAPE`, and the CLI checks it against nothing else: that node
  URL is what the signer trusts, so its holder sets it to duckpond's own
  node and nothing in between. At start the signer asks identity `OfKey`
  for the public key `keys/key` holds in the clear, and is that account:
  `signed.json` names it first, and the read-back of a lost claim errs
  only checks it claimed. A key identity names no account for ends the
  signer, saying so, and the LaunchAgent does not start it again; a node
  it cannot reach yet is asked again after `FOLLOW_PAUSE`, logged once. The
  LaunchAgent `signer/com.ducktape.signer.plist` keeps it running.
  `signer/signer-test.py` covers it against a fake network and a real tool
  process.

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
