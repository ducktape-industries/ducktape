---
name: qa
description: Verify a running Ducktape node and cluster — the node's /v1 surface, module transactions, real-socket cluster e2e, native GPUI app suites, and the live huddle lane. App suites run with cargo test -p ducktape-app; device and pixel checks require platform support.
---

# Node QA

`ducktape-app` (`app/`) hosts dynamically loaded WASM views in native GPUI.
Its `#[cfg(test)]` suites run like any other crate and belong in every QA pass.
Native test contexts do not prove desktop rendering: font/layout probes need
the real platform text system, and live device/media behavior needs the huddle
lane. Do not report unsupported native pixel capture as a passing screenshot.

## What to run

Node and module semantics — deterministic, in-process:

```bash
cargo test -p simnode                        # the deterministic /v1 twin's suites
cargo test -p node-bin --test cluster_e2e    # real 4-node cluster over localhost TCP
cargo test -p ducktape-app                   # the desktop app's own suites
make test                                    # full local gate: wasm drift + workspace + sim
```

`cargo test -p ducktape-app` is part of every QA pass; the node lanes above do
not cover it.

### The huddle: three lanes, and only the last one is the whole thing

A huddle is the one feature whose failure mode is BETWEEN two people, so its
coverage is layered and the top layer has to be run by hand:

```bash
cargo test -p node-bin --test huddle_media_e2e   # two real nodes, real overlay,
                                                 # the late-joiner deadlock and its cure
ops/huddle-lane.sh                               # stands a two-node network up and
                                                 # prints one command per side
```

`ops/huddle-lane.sh` is the live lane: two real nodes, one user key per side,
and `app/src/tests/huddle_live.rs` run once per side (it is `#[ignore]`d, so it
only runs when asked). Each side joins the huddle through the app's own
`join_huddle`, waits for the other, publishes this box's camera and microphone,
and asserts the other side's beacon, picture AND voice all arrive. A headless
box can borrow the devices — one camera for the pair, one sound card per side:

```bash
sudo modprobe v4l2loopback devices=1 exclusive_caps=1 max_openers=8
sudo chmod a+rw /dev/video0
ffmpeg -re -f lavfi -i testsrc=size=640x480:rate=30 -pix_fmt yuyv422 -f v4l2 /dev/video0 &

sudo modprobe snd-aloop index=0,1 enable=1,1 pcm_substreams=4 id=lanea,laneb
sudo chmod -R a+rw /dev/snd
ffmpeg -y -f lavfi -i "sine=frequency=500:duration=600:sample_rate=48000" -ac 2 \
       -c:a pcm_s16le /tmp/tone.wav
for card in lanea laneb; do while true; do aplay -D hw:$card,1,0 /tmp/tone.wav; done & done
```

An aloop card loops device 1's playback into device 0's capture — which is what
`default` records from — so that tone IS the side's microphone, and `ALSA_CARD`
picks which card a process calls `default`. Both `snd-aloop` and the v4l2 core
live in `linux-modules-extra-$(uname -r)`.

`DUCKTAPE_HUDDLE_SOURCE=screen` (with a real `DISPLAY` — `Xvfb :99 -screen 0
1280x800x24` is one) publishes that side's DESKTOP instead of its camera, and
the far side names what it got by its size: a camera is 640×480, a desktop is
that root window halved onto the tile budget (1280×800 → 640×400).

Measured on zk-dev 2026-08-22: both sides had the other's picture and 40+
audible mixed frames about one second after joining, camera one way and a
shared desktop the other. Stop the `ffmpeg` producer and both sides fail on the
picture; stop the `aplay` loops and both fail on the voice with beacon and
picture still true. That falsifiability is what makes the passing run mean
anything.

The lane also prints how THIS side's own picture arrived — frames, mean gap,
worst gap. Stutter is a distribution, not a rate, and only the worst gap can
see a hole. Same box, same run: a 30 fps camera lands at 33.3 ms mean / 45.7 ms
worst, and a 10 fps screen share at 100.0 / 117.0. A mean well above the
source's own interval is the capture loop paying for its work AFTER the wait
instead of inside it.

**The signing helper must be on PATH.** `join_huddle` signs through the
`ducktape` CLI, so a lane process started with a stripped environment dies with
"The signing helper failed to run" before it ever reaches the huddle. Put
`target/debug` on `PATH` when you run a side under `sudo`/`env`/`ip netns exec`.

#### Off loopback: the same lane across a ROUTED path

Loopback hides the two things a real second machine brings — routing and a
1500-byte MTU. Network namespaces bring both back on one box, which is as close
as it gets without a second machine:

```bash
for ns in dtA dtB dtR; do sudo ip netns add $ns; done
sudo ip link add vethA type veth peer name rA; sudo ip link add vethB type veth peer name rB
sudo ip link set vethA netns dtA; sudo ip link set rA netns dtR
sudo ip link set vethB netns dtB; sudo ip link set rB netns dtR
sudo ip -n dtA addr add 10.10.1.2/24 dev vethA; sudo ip -n dtA link set vethA up; sudo ip -n dtA link set lo up
sudo ip -n dtB addr add 10.10.2.2/24 dev vethB; sudo ip -n dtB link set vethB up; sudo ip -n dtB link set lo up
sudo ip -n dtR addr add 10.10.1.1/24 dev rA; sudo ip -n dtR link set rA up
sudo ip -n dtR addr add 10.10.2.1/24 dev rB; sudo ip -n dtR link set rB up
sudo ip netns exec dtR sysctl -w net.ipv4.ip_forward=1
sudo ip -n dtA route add default via 10.10.1.1; sudo ip -n dtB route add default via 10.10.2.1
```

Then give each node a config whose `listen`/`wireguard_listen` is its OWN
namespace address (`peer_addrs` naming both), run each with `ip netns exec
dt{A,B} …`, and run each lane side in the matching namespace. `/dev` is not
namespaced, so the borrowed camera and sound cards still work.

Measured on zk-dev 2026-08-23, two subnets and a router namespace between them:
both sides had the other's 640×480 picture and audible frames, and the capture
pace was unchanged (33.6 ms mean, 41.7 / 45.6 ms worst). That is JPEG frames
fragmenting across a 1500-byte MTU with WireGuard overhead — which loopback's
65536-byte MTU never exercises.

### On macOS: raise the fd limit first

```bash
ulimit -n 4096      # macOS defaults the SOFT limit to 256; the hard limit is unlimited
```

Without it, `cargo test -p ducktape-app` fails on macOS with
`Too many open files` inside qmdb init, from the tests that boot a simnode in
process. A node at rest holds ~340 fds and **317 of them are path-backed**
(qmdb journal blobs) — fixed at boot, not scaling with peers — so 256 is not
close to enough for even one in-process node.

The shipped binary is unaffected: `resource_limits::raise_open_file_limit()`
runs in `bin/node`'s `main()` and lifts the soft limit toward 65,536. A test
harness never goes through that `main`, which is why only the test lane sees
it. Measured 2026-07-28 on macmini-duke (macOS 26.5.2, arm64): `cargo check
-p ducktape-app --tests` is clean and 85/86 tests pass — the one failure is
exactly this limit.

`bin/simnode` boots a deterministic node in-process for any crate's `#[test]`.
For the embedding harness (`simnode::boot`) and the chat wire facts, see the
`sim-lane` skill.

### On macOS: the first exec of a fresh build pays Gatekeeper

The FIRST exec of any freshly built binary is scanned whole-file by
syspolicyd before it runs. Measured 2026-08-06 on macmini-duke: 3.2 s wall at
0% process CPU for the ~1 GB debug `ducktape`, 0.012 s once cached — and the
cache keys on the binary's hash, so **every rebuild pays it again on first
run**. Signing does not help: arm64 binaries are already ad-hoc signed by the
linker, and a Developer ID identity doesn't survive a rebuild's new hash
either (notarization is for quarantined downloads, not local builds).

Two remedies, use both:
- The app pre-warms its signer CLI at launch-window open (`hub_state()`
  spawns `ducktape --version` fire-and-forget), so the scan finishes while
  the password is being typed.
- On a dev Mac, grant the terminal the **Developer Tools** exception
  (System Settings → Privacy & Security → Developer Tools; surface the pane
  with `sudo spctl developer-mode enable-terminal`). Locally built products
  of an exempted terminal skip the first-run scan entirely.

A Linux box structurally cannot reproduce this class — when a Mac feels
seconds slower than the rig on a first action after a rebuild, time the CLI
twice on the Mac before suspecting the app.

## Live node inspection

A running daemon (`cargo run -p noded-bin -- --modules <dir>`, or a workspace
node seeded by `make demo-seed`) serves the full `/v1` surface at
`http://127.0.0.1:8844` by default. Its genesis composes every tenant from
`<dir>/<id>.component.wasm` and converges every `<dir>/<id>.index.wasm`;
without `--modules` it reads the founding set the build staged beside the
binary (`target/<profile>/modules`, or `$DUCKTAPE_MODULES_DIR`) and refuses
to boot, naming the first file it could not find, if that set is incomplete.
Query it directly, or drive its module surface with the
`ops/agent-system` operator CLI (raw query/submit, agent list/pause/resume).
Do not expose capability-bearing URL paths, keys, passwords, or recovery
phrases in reports.

## Native frame evidence

The app's `frame_probe` test helpers use native GPUI scenes and the platform
text system. Separate idle, scrolling, tab switching and typing measurements;
an aggregate CPU number cannot prove that an individual interaction is smooth.
Do not substitute the test platform's no-op text backend for font or layout
measurements. Pixel captures and device behavior require platform support.

## Process safety

Never use `pkill -f` — a pattern match will cheerfully kill an editor, a grep,
or an unrelated node. Identify a process by executable, process cwd, and the
workspace's `--config` before signalling it, or use the node's own graceful
`/v1/admin/shutdown`. Every `/v1/admin/*` route needs a credential — WHICH one
is decided by the node's `DUCKTAPE_ADMIN` exposure, and by nothing else. Read
the node's env before reaching for a token; owning an account does not change
the answer.

**`loopback` — the default, and what an unset `DUCKTAPE_ADMIN` gives you.** The
OPERATOR credential, on an on-box caller. Loopback presence alone is not
authority (a service daemon is a loopback peer too), so present the secret the
node minted 0600 into its own workspace:

```
curl -XPOST localhost:$PORT/v1/admin/shutdown \
  -H "x-ducktape-admin-token: $(cat "$WORKSPACE/admin.token")"
```

**`public` — only when the operator set `DUCKTAPE_ADMIN=public`.** The surface
is reachable off-box, so the OWNER proof-of-possession is the gate for every
peer, loopback included. The owner is the Identity ACCOUNT the node's wallet
key is on (resolved through `OfKey`; no node is ever bound to an account), and
any member key of that account may sign. The operator token is NOT accepted
and NOT a fallback there; mint a per-request PoP with a member key instead:

```
ducktape user sign-admin --key "$DUCK/<chain-id>/keys/<wallet>.key" \
  --method POST --path /v1/admin/shutdown --node-key "$NODE_KEY"
# one json line {"key","ts","sig"} -> x-ducktape-admin-key / -ts / -sig
```

A `public` node whose wallet key is on no account yet falls back to the
operator token until that account exists — so on a fresh network both recipes
work, and after `ducktape account create --name <you>` (a user-signed frame
from that wallet) only the PoP does.

The refusals tell the two apart: a token presented to an owned `public` node is
`401 owner_signature_invalid` (wrong credential TYPE), never `403
operator_token_mismatch` (right type, wrong secret). `DUCKTAPE_ADMIN=off`
removes the routes entirely — 404. The token is still minted there, because it
is no longer the admin namespace's alone (below).

**The DATA plane wants a credential too, in two strengths.** Every MUTATING
`/v1` route takes EITHER a per-request signature or that same operator
credential, in the same `x-ducktape-admin-token` header. Reads stay open.

- MODULE-BOUND — `/v1/submit`, the duckfs writes, `/v1/files/object/{path}`
  PUT/DELETE, `POST /v1/fs/workspaces` and its commit — take ANY key's
  signature: the key becomes the op's origin and the module authorizes it.
- NODE-LEVEL — `/v1/invite`, `/v1/log-filter`, `/v1/term/sessions`,
  `DELETE /v1/fs/workspaces/{id}` — take the operator credential or a signature
  by the node's own operator key (its active wallet key at boot). A signature by
  any other key is `403 not_operator`.

So a QA `curl` that writes carries
`-H "x-ducktape-admin-token: $(cat "$WORKSPACE/admin.token")"`, a `401
signature_missing` on a route that used to work means exactly that header is
missing, and a `403 not_operator` means the right shape with the wrong key. The
forge's `git-receive-pack` wants the same credential through git's
`http.extraHeader`, or a `git push --signed` certificate.

Never paste either credential (or a token file's contents) into a report. For
merged-worktree cleanup, dry-run
`ops/worktree-clean.sh` and then use `--yes`; it finds live processes by cwd and
never uses `pkill -f`.
