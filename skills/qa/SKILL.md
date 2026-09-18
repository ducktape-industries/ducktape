---
name: qa
description: Verify a running Ducktape node and cluster — the node's /v1 surface, module transactions, and the real-socket cluster e2e suites.
---

# Node QA

## What to run

Node and module semantics — deterministic, in-process:

```bash
cargo test -p simnode                        # the deterministic /v1 twin's suites
cargo test -p node-bin --test cluster_e2e    # real 4-node cluster over localhost TCP
make test                                    # full local gate: no-embedded-wasm lint + workspace + sim
```

### Reading an e2e failure on a loaded box

The node e2e suites spawn real processes and wait on real markers, so on a box
where other sessions are compiling they fail for reasons that have nothing to
do with the code. Three rules, in the order they bite:

**Run the test binary at NORMAL priority.** `nice -n 19` belongs on cargo
BUILDS. These suites race fixed deadlines — `wait_marker(…, Duration::from_secs(30))` —
so a deprioritised node misses them by construction and every case goes red at
once. Build niced, then exec the test binary un-niced.

**A `timed out without printing "<marker>"` panic is LOAD, not a bug.** Quote
the marker before believing anything: it names the phase that ran out of clock.
`genesis root_hash=` is first boot, where the node installs every founding
module's wasm and which is by far the heaviest phase; `recovered root_hash=` is
restart recovery. Confirm by reading the node's own log tail in the panic — a
node that is still emitting lines at the deadline (`module installed`,
`Merkle structure lags behind journal`, `recovering orphaned leaf`) was SLOW,
not wedged. Never widen a deadline to make one pass: the deadline is what makes
stopped progress visible at all.

**Pin what the run reads, and guard it.** A build stages into the set named for
its own checkout (`modules%<path>`), so a peer's `noded` build no longer
restages yours — but YOUR next `cargo build` does, and that is enough to shift
artifacts under an iteration. `workspace_config::modules_dir()` honours
`$DUCKTAPE_MODULES_DIR` before anything else and `founding_set()` resolves
through it, so a private snapshot holds one set still for the whole run:

```bash
touch crates/noded/build.rs && cargo check -p noded    # restage
cp -a "$CARGO_TARGET_DIR/debug/modules$(pwd | tr / %)/." target/pin-modules/
export DUCKTAPE_MODULES_DIR=$PWD/target/pin-modules
```

That variable ALSO redirects `sim_modules_dir()`, and the production set has no
`kv`, so snapshot both directories or neither — otherwise a suite that boots a
simnode fails with `kv.component.wasm: no such founding entry`. The node binary cannot be
pinned at all (`CARGO_BIN_EXE_ducktape` is baked in at compile time), so digest
it around each iteration and DISCARD any iteration it changed under — a pass on
shifted artifacts is worth no more than a failure. Digest a directory by hashing
the hashes (`find . -type f | sort | xargs md5sum | awk '{print $1}' | md5sum`);
comparing raw `md5sum` output across two directories always differs, because it
embeds the path.

**Keep the evidence.** `DUCKTAPE_E2E_KEEP=1` disarms the cluster tempdir's Drop
so a failed run's storage, journal and logs survive the unwind. Kept roots are
named `ducktape-e2e-keep-…`, which the harness's own sweep skips, so they
outlive every later run and are yours to delete.

### On macOS: raise the fd limit first

```bash
ulimit -n 4096      # macOS defaults the SOFT limit to 256; the hard limit is unlimited
```

Without it, any suite that boots a simnode in process fails on macOS with
`Too many open files` inside qmdb init. A node at rest holds ~340 fds and
**317 of them are path-backed** (qmdb journal blobs) — fixed at boot, not
scaling with peers — so 256 is not close to enough for even one in-process
node.

The shipped binary is unaffected: `resource_limits::raise_open_file_limit()`
runs in `bin/node`'s `main()` and lifts the soft limit toward 65,536. A test
harness never goes through that `main`, which is why only the test lane sees
it.

`bin/simnode` boots a deterministic node in-process for any crate's `#[test]`.
For the embedding harness (`simnode::boot`) and the chat wire facts, see the
`sim-lane` skill.

## Live node inspection

A running daemon (`cargo run -p noded-bin -- --modules <dir>`, or a workspace
node seeded by `make demo-seed`) serves the full `/v1` surface at
`http://127.0.0.1:8844` by default. Its genesis composes every tenant from
`<dir>/<id>.component.wasm` and converges every `<dir>/<id>.index.wasm`;
without `--modules` it reads the founding set its own build staged beside the
binary (`target/<profile>/modules%<checkout path>`, or
`$DUCKTAPE_MODULES_DIR`) and refuses
to boot, naming the first file it could not find, if that set is incomplete.
Query it directly, or drive its module surface with the
`ops/agent-system` operator CLI (raw query/submit, agent list/pause/resume).
Do not expose capability-bearing URL paths, keys, passwords, or recovery
phrases in reports.

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
credential, in the same `x-ducktape-admin-token` header. Reads stay open,
except the ws `logs` topic: the log ring is the operator's, so the `/v1/ws`
upgrade carries the operator credential (on-box) or a request signature by the
operator key over `GET /v1/ws` — otherwise subscribing to `logs` answers an
error frame `forbidden`.

- USER OPERATIONS — `/v1/submit/frame` verifies the operation's signed frame;
  Files clients send module queries through `/v1/query` and writes through this
  generic frame lane. No product-specific Files HTTP endpoints are registered.
- ACTING KEY — the blob upload, `POST /v1/fs/workspaces` and its commit accept
  a request signature; the workspace adapter carries that acting identity.
- NODE-LEVEL — `/v1/submit`, `/v1/submit/raw/{target}`, `/v1/invite`, `/v1/log-filter`,
  `/v1/gateway/operator`, `DELETE /v1/fs/workspaces/{id}` — take the operator credential or a signature
  by the node's own operator key (its active wallet key at boot). A signature by
  any other key is `403 not_operator`.
- SERVICE LINK — `POST /v1/services/hello` is a local service daemon's: it
  takes `x-ducktape-service-link` with `$WORKSPACE/service-link.token` (or the
  operator credential); anything else is `401 service_link_missing`. A ws
  `run_output` frame is honored only after `compute_attach` with that same token.
- SIGNED CALLER — `/v1/gateway/proxy` wants the head's `user_pop`, signed by a
  key on an Identity account; none is `401 caller_proof_missing`.

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
