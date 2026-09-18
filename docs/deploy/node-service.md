# Running a node as a service

How to keep a `ducktape` node — and the compute / agent / airlock daemons
that serve it — running across reboots and crashes. Everything up to
"macOS (launchd)" is the Linux host: the units are
`ops/node/ducktape-node@.service` and `ops/node/ducktape-service@.service`,
the log rotation is `ops/node/ducktape-node.logrotate`, and every path below
matches what those files set. macOS runs the node as a per-user LaunchAgent
instead; that section names its two files and its commands.

Every unit runs **`ducktape-node-launcher`**, never the node or a daemon
directly (`bin/node-launcher/src/main.rs`). The node unit runs
`ducktape-node-launcher run --workspace DIR --config FILE`: the launcher
starts `<workspace>/current/ducktape node run`, asks it `ducktape release
status` every poll, and follows the network's node releases — it stages a
designated release, qualifies it against the workspace checkpoint, flips
`current` at the activation height and rolls back a release that never comes
up. Whenever it holds no node — at start, and after a flip stopped the old
one — a `run` launcher whose bytes differ from
`<workspace>/current/ducktape-node-launcher` `exec`s that file in its own pid
(`node_update_launcher_exec`), so after a flip the unit's main process runs the
launcher the release shipped; the unit's `ExecStart` copy is never written, and
it is what counts a boot and rolls back a release whose launcher cannot start.
A service unit runs the daemon through the launcher's service role,
`ducktape-node-launcher service --workspace DIR --config FILE -- service run <kind> --enable`
(`supervise_service`): the role starts `<workspace>/current/ducktape` and
restarts the daemon when a release flip moves that link, so the node and its
daemons change build together. A node started as a bare `ducktape node run`
follows no release at all, and a daemon started as a bare `service run` keeps
the binary it started from (`service status` shows the skew as
`build X (this node: Y)`).

The node is supervisor-ready: SIGTERM and SIGINT take the graceful
checkpoint path on a validator and a resident alike (the same one the desktop
shell uses on quit, `QuitSignals` in `bin/node/src/drain_actions.rs`) and end
with one `node_shutdown` line naming the signal, the height and whether the
final checkpoint was `written`, `already_current` or `skipped(<reason>)` — the
next boot's recovery line names at least that height. It raises its own
open-file soft limit to 65536 (`bin/node/src/resource_limits.rs`). The
launcher passes a stop to its child as SIGTERM and gives it 120 s
(`STOP_BUDGET`, `bin/node-launcher/src/node.rs`) before SIGKILL.

**The release key comes from the network.** A release is trusted under the key
the workspace pins at `<workspace>/updates/keys/release.pub`. A workspace with
no pin — every one `node join` writes — pins the node key the network's
governance committed (`ducktape release key set --kind node --pubkey <hex>`,
run by the network's validators) on the launcher's first reading that carries
one, and logs `node_update_release_key_pinned` once. A pin already on disk is
never overwritten: one that differs from the network's key is refused as
`release_key_pinned_differs` (at attempt 1, then every 60th poll, both keys
named) and stays the key followed; `ducktape-node-launcher install
--release-key <hex>`, with the unit stopped, is the explicit way to move it
(an install refuses a workspace a running launcher holds:
`workspace_locked`). Until the network commits
a key, an unpinned launcher refuses each designated release as
`no_release_key`. `ducktape release status` prints both sides:
`release_key node` (the network's) and `pinned` (this workspace's).

## Where the workspace lives

Everything a network keeps on disk sits in its workspace under
**`DUCKTAPE_HOME`** (`crates/workspace-config/src/lib.rs`, `ducktape_home`):
`$DUCKTAPE_HOME` when set, else `~/.ducktape`. The home holds one directory
per network and nothing else — two networks on one host share no file. The
units set `DUCKTAPE_HOME=/var/lib/ducktape` (a systemd `StateDirectory`,
owned by the `ducktape` user), so under them the layout is:

```
/var/lib/ducktape/
  <chain-id>/                one dir per network (`ducktape node list`)
    node.toml                the operator file: listeners, storage_dir, [sandbox]
    network.toml             the network descriptor (validators, reach hints, the genesis pin)
    genesis                  the network's wasm (every component + index guest), pinned by network.toml
    identity.key             THIS NODE'S seat key, 0600 — back it up (see backup-and-keys.md)
    keys/                    this network's user wallets + `active` pointer (`ducktape wallet new <name> -n <chain-id>`)
    guest/                   the kernel + rootfs its runs boot (`OUT=<workspace>/guest ops/build-guest-rootfs.sh`)
    executors/               the pinned agent CLIs its runs exec (`ducktape agent install -n <chain-id>`)
    capabilities/            operator capability specs, `*.toml`
    wireguard.key            the tunnel keypair (regenerable)
    services.toml            service grants (`ducktape service enable`)
    coord.cap                the coordinator admission capability, when issued
    service-link.token       the node↔daemon link secret, 0600
    daemon.log               `node run`'s tee (append-only)
    <kind>.log               `service run <kind>`'s tee (append-only)
    storage/                 consensus state, blobs, mesh-state.json, airlock-creds/
    updates/                 the launcher's: state.json, keys/release.pub (the pinned release key), releases/<sha>/
    current -> updates/releases/<sha>   the release every unit runs: `ducktape` and its `modules/`
/usr/local/lib/ducktape/     the installed program, in the shape of an unpacked node release
  ducktape                   linked from /usr/local/bin/ducktape
  ducktape-node-launcher     linked from /usr/local/bin/ducktape-node-launcher
  modules/                   the founding set (`<id>.component.wasm`, `<id>.index.wasm`, the netstack guest): what `node init` composes a genesis from, and what `launcher install` copies into the first release
```

A node reads its founding set beside its own binary, so under the launcher it
reads `current/modules/`, and a release flip moves the binary and its set
together. The units set no `DUCKTAPE_MODULES_DIR`: a directory named there
would outrank the set each later release brings.

### What grows, and what nothing prunes

`storage/blobstore` (one flat file per applied op payload, named by its
sha256) and `storage/index` (one indexer op row per dispatch per module) keep
**every** op payload forever — there is no retention window, no GC pass and no
pruning knob today, so both only ever climb. A `files` upload puts its chunk
bytes through both, so a network moving a few GB of files a month costs each
node roughly that much again in `blobstore` and again in `index`, plus one
`blobstore` file per op (100k+ files in that one directory after a few months
of real use). Watch the slope, per node:

```sh
curl -s 127.0.0.1:8844/metrics | grep ducktape_store_        # bytes + files per store
curl -s 127.0.0.1:8844/v1/status | jq .operations.storage.stores
```

Both are node-local and derived — nothing here is consensus state — so the
recovery for a full disk is still an operator decision, not one this node
makes for you.

Every operator verb you run against that tree needs the same view of it:

```sh
alias dt='sudo -u ducktape env DUCKTAPE_HOME=/var/lib/ducktape /usr/local/bin/ducktape'
```

## Install

`ops/node/install.sh --workspace <name> (--init | --join <invite>)` runs
steps 1-7 below end to end (`--dry-run` prints the commands without touching
the host); the steps are spelled out here for anyone auditing or adapting them.
`<name>` is a chain id or a unique prefix of one; the script resolves it to
the full chain id once the network is founded or joined. `--archive <file>`
takes the program from a node release archive (`ducktape-<sha7>-linux-<arch>.tar.zst`,
which carries `ducktape`, `ducktape-node-launcher`, `modules/` and
`release.json`) instead of step 1's build: it unpacks it with `zstd`, installs
both binaries and `release.json` into `/usr/local/lib/ducktape`, and step 3
copies the archive's `modules/`.

```sh
# 1. Build ducktape and its launcher (make install-node puts both in
#    ~/.cargo/bin and the founding set in ~/.cargo/bin/modules beside them),
#    install them as one program directory and link them onto PATH.
make install-node
sudo install -d -m 0755 /usr/local/lib/ducktape
sudo install -m 0755 ~/.cargo/bin/ducktape ~/.cargo/bin/ducktape-node-launcher /usr/local/lib/ducktape/
sudo ln -sfn /usr/local/lib/ducktape/ducktape /usr/local/bin/ducktape
sudo ln -sfn /usr/local/lib/ducktape/ducktape-node-launcher /usr/local/bin/ducktape-node-launcher

# 2. A dedicated user. The kvm group is for the service daemons: compute and
#    agent open /dev/kvm per run; `node run` itself never does.
sudo useradd --system --home-dir /var/lib/ducktape --shell /usr/sbin/nologin ducktape
sudo usermod -aG kvm ducktape
sudo install -d -o ducktape -g ducktape -m 0700 /var/lib/ducktape

# 3. The founding set, beside the binary: what `node init --modules` composes
#    the genesis from, and what the first release carries.
sudo install -d -m 0755 /usr/local/lib/ducktape/modules
sudo cp ~/.cargo/bin/modules/*.wasm /usr/local/lib/ducktape/modules/
sudo chmod -R a+rX /usr/local/lib/ducktape/modules

# 4. Units and log rotation.
sudo cp ops/node/ducktape-node@.service ops/node/ducktape-service@.service /etc/systemd/system/
sudo install -m 0644 ops/node/ducktape-node.logrotate /etc/logrotate.d/ducktape-node
sudo systemctl daemon-reload

# 5. Found or join the network AS the service user, so the files land where
#    the unit will look for them. A member (an identity the founder admitted
#    before genesis) joins with the founder's `<workspace>/genesis`; a
#    resident fetches it off the mesh at first boot.
dt node init --name mynet --modules /usr/local/lib/ducktape/modules   # founder
dt node join '<invite blob>'                                    # ...or a resident
dt node join '<invite blob>' --genesis /path/to/founders/genesis # ...or a member
dt node list                              # the chain id the instance names

# 6. Seed the first release under the launcher — ONCE: after this the
#    launcher owns `current`. No --release-key: the launcher pins the key the
#    network committed on its first read. Before it writes anything, the
#    install refuses a directory with no node.toml (`node_config_missing`)
#    and a binary with no `modules/` beside it (`founding_set_missing`).
#    Then name the workspace the service units run over.
W='/var/lib/ducktape/mynet#d0cdf950'
sudo -u ducktape env DUCKTAPE_HOME=/var/lib/ducktape ducktape-node-launcher install \
  --workspace "$W" --config "$W/node.toml" --from /usr/local/lib/ducktape/ducktape
sudo install -d -m 0755 /etc/ducktape
echo "DUCKTAPE_WORKSPACE=\"$W\"" | sudo tee /etc/ducktape/workspace.env

# 7. Enable and start (next section).
```

`node join` ends on the same instruction for a hand-run node: the two
`ducktape-node-launcher` commands above, with the real paths of the
workspace it wrote and of the binary that wrote it.

`node init`/`node join` probe the host and write the `[sandbox]` table when
`/dev/kvm` opens; a host that gained KVM later runs `dt node sandbox` once.
The table names HOW a run is isolated, never where its images are: those are
the workspace's own, built by `OUT=/var/lib/ducktape/<chain-id>/guest
ops/build-guest-rootfs.sh` (as the service user, or chowned to it), and
`dt node sandbox` names the exact invocation for a workspace missing them.
Firecracker, `mke2fs` AND `debugfs` must be on the service's `PATH`
— `/usr/local/bin`, `/usr/sbin` and `/sbin` are searched
(`crates/services/sandbox/src/host_tools.rs`); the compute/agent daemon
refuses to boot without them (`sandbox.rs`, `required_tools`) — install
`e2fsprogs`.

## Enable and start

The instance name is the workspace DIRECTORY under `/var/lib/ducktape`: the
full chain id (`dt node list`). A chain id carries `#`, which a unit name
cannot, so escape it; the unit hands `%I`, the unescaped instance, to the
launcher as `--workspace /var/lib/ducktape/%I`:

```sh
sudo systemctl enable --now "ducktape-node@$(systemd-escape 'mynet#d0cdf950')"

dt node status                            # height + root hash, once it serves
dt release status                         # the designated release, the network's key, this pin
```

A joiner whose invite the network refused exits 77 and so does its launcher,
which the unit does not restart (`RestartPreventExitStatus=77`): every restart
would ask the same question and hear the same no. `systemctl status` shows the
unit failed and the journal carries the refusal; join again with a fresh
invite, then start the unit.

Service daemons: the instance is the kind (`compute`, `agent`, `airlock`),
and the workspace is `DUCKTAPE_WORKSPACE` from `/etc/ducktape/workspace.env`.
The launcher's service role waits until the node has published its mesh
identity before it starts the daemon, so co-starting both at boot converges on
its own (once serving, a daemon rides out a node restart — see below).
Enabling the unit is the consent: it runs `service run <kind> --enable`, which
mints the daemon's grant without asking.

```sh
sudo systemctl enable --now ducktape-service@compute
dt service status
```

`/etc/ducktape/node.env` may carry `DUCKTAPE_NODE_ARGS` (extra `node run`
flags), `DUCKTAPE_SERVICE_ARGS` (extra `service run` flags — never a
workspace selector: the launcher appends `--config` itself), `RUST_LOG`, and a
`TMPDIR` on a disk with room for run images (they are as large as a run's
inputs, and the unit's private `/tmp` is whatever the host's `/tmp` is).

To pin a daemon to one node's lifecycle:

```sh
sudo systemctl edit ducktape-service@compute
# [Unit]
# After=ducktape-node@mynet\x23d0cdf950.service
# BindsTo=ducktape-node@mynet\x23d0cdf950.service
```

## Logs

Two sinks, one filter: stderr (the journal) and the tee file in the
workspace. They never disagree about what was recorded. The journal also
carries the launcher's own `ducktape::update` lines — every release it
stages, qualifies, flips or refuses, and the key it pins.

```sh
U='ducktape-node@mynet\x23d0cdf950'       # the escaped instance, as enabled above
journalctl -fu "$U"
tail -f /var/lib/ducktape/<chain-id>/daemon.log
tail -f /var/lib/ducktape/<chain-id>/compute.log

# turn one plane up on the LIVE node — never restart to look at a wedged state.
# the route mutates the process, so it takes a credential: the verb signs with
# the active wallet key, and on a server with no wallet the node's own operator
# credential does (an uncredentialed curl is refused 401).
ducktape node log-filter 'info,ducktape::join=debug' -n <chain-id>
curl -XPOST 127.0.0.1:8844/v1/log-filter -d 'info,ducktape::join=debug' \
  -H "x-ducktape-admin-token: $(cat /var/lib/ducktape/<chain-id>/admin.token)"
```

The tee files are opened append-only once and never reopened, which is why
the logrotate drop-in uses `copytruncate` (weekly, or at 256 MB, eight kept).

A wedged node (no more progress, no crash) can dump every async task it is
parked on. The unit's main process is the launcher, so the signal goes to its
`node run` child:
`kill -USR1 "$(pgrep -P "$(systemctl show -p MainPID --value "$U")" -f ' node run ')"`
writes `/var/lib/ducktape/<chain-id>/tasks.txt` (overwritten each
time) and logs one `task_dump_written` line to `daemon.log`. Linux
x86_64/aarch64 only; elsewhere the signal does nothing.

## Restart, stop, upgrade

```sh
sudo systemctl restart "$U"      # the launcher stops its node (SIGTERM → final checkpoint), then boots it again
sudo systemctl stop "$U"         # stays down until started
```

A node's binary moves by a NODE RELEASE, not by a file copy: the network
designates it (`ducktape release schedule`, `docs/roll-a-node-release.md`) and
every launcher stages, qualifies and flips it at the activation height, then
restarts the service daemons on it. Replacing `/usr/local/lib/ducktape/ducktape`
changes nothing that runs — every unit runs `<workspace>/current/ducktape`.
The launcher itself is the installed file: replace
`/usr/local/lib/ducktape/ducktape-node-launcher` and restart the units.

A running daemon survives a node restart, so do not restart it for one: its
link task re-dials forever (`bin/node/src/compute/link.rs`,
`bin/node/src/agent/link.rs`; the agent link re-reads `service-link.token`
on every attach, since a node restart mints a fresh one), and its hello
heartbeat keeps signaling — `warn` `hello_failed` at attempt 1, then every
30th, carrying `attempts` (`bin/node/src/services.rs`, `heartbeat`). Restart
`ducktape-service@<kind>` only for a new grant. Before stopping a validator,
read the next section:
on a three-validator network the restart halts the chain for its duration.

`curl 127.0.0.1:8844/v1/status | jq .version` prints the build the node
runs (`<cargo version>+<git short sha>[-<dirty digest>]`), which is how two
hosts confirm they run the same binary.

`dt node peers` (and `GET /v1/peers`) carries that same stamp per peer as
`build=`, and a node warns once per `(peer, stamp)` with
`reason = "build_stamp_mismatch"` when a peer's stamp differs from its own.
Nothing refuses, disconnects or gates admission on a stamp — two builds whose
consensus logic has drifted still finalize together, and this only makes the
drift nameable. **Know the column's reach before you trust it.** A node learns
a peer's stamp only by POLLING that peer on the state-sync detection lane, and
only a RESIDENT polls: the column is filled on a resident, for the source it
happens to be polling, and reads `build=unknown` everywhere else — on every
peer of a validator, and on a resident for every peer it does not poll. So
`build=unknown` means "never asked", never "agrees with us", and on an
all-validator network (the three-seat shape below) nothing fills the column at
all and the mismatch warn cannot fire. Reading `/v1/status | jq .version` on
each host is the check that always works.

## Validator count: three seats tolerate nothing

Consensus is BFT: `f = (n - 1) / 3` faults tolerated, `quorum = n - f`. The
arithmetic is commonware's (`commonware_utils::faults`, baked into
`Finalization::verify` — see `verify_finalization` in
`crates/kernel/consensus/src/lib.rs`); the node reports the same number on
`/v1/status` and `/metrics` (`fn quorum`, `crates/noded/src/metrics.rs`).

| validators `n` | tolerated faults `f` | quorum |
| --- | --- | --- |
| 3 | **0** | 3 |
| 4 | 1 | 3 |
| 5 | 1 | 4 |
| 7 | 2 | 5 |

**At n = 3 the fault tolerance is zero.** Every seat must vote every block, so
one validator that is down, unreachable or wedged HALTS block production.
Rebooting one of three halts the chain for the reboot. A sleeping host is a
down validator — and the mesh does not even notice until a socket's
read/write deadline expires (`MESH_IO_TIMEOUT`,
`bin/node/src/constants.rs`), so a closed laptop lid is a halt plus a
detection delay. n = 4 is the first count with any slack at all.

This is accepted rather than fixed — no code warns about it and nothing refuses
to run at n = 3. If you want a network that survives one host rebooting, seat
five validators on always-on hosts and keep the sometimes-on machines as
residents.

### What the halt looks like

- Writes fail after ten seconds with **`timed out awaiting finalization — re-query
  on the next block`** (the budget is `SUBMIT_HOLD` in
  `bin/node/src/constants.rs`, the sentence is in
  `bin/node/src/validator/run/drain.rs`). The desktop app does **not** show that
  sentence — it rewrites any "timed out" to **`The node did not answer in time.
  Retry in a moment.`** (`user_error`), so on the app the halt looks like a
  slow node.
- Reads keep answering from committed state, so every node still looks alive:
  `dt node status` prints a height, and that height simply stops advancing.
- `curl 127.0.0.1:8844/v1/status | jq .operations.consensus` shows
  `reachable_validators` below `quorum`; the same pair is on `/metrics` as
  `ducktape_consensus_reachable_validators` and `ducktape_consensus_quorum`.
- The same object carries `block_beat_stalled_seconds` — how long this node has
  gone without sealing a height, 0 while the chain beats — and `/metrics` has it
  as `ducktape_block_beat_stalled_seconds`. That is the number to alert on: it
  rises continuously rather than only when a log line happens to fire, and the
  node's own `block_beat_stalled` event escalates from `warn` to `error` once it
  passes a minute.
- `dt node peers` names the seat that stopped talking (`connected=no`, or no
  row at all).

### Recovery is manual

Nothing you can run on the surviving nodes repairs the chain. **The only fix is
to bring the missing validator back**, and under these units that means starting its node
again (a node that crashed on a supervised host is already being restarted
by its launcher, backing off up to about a minute while it keeps dying at
boot; a hand-run node has no supervisor at all):

```sh
dt node status                                   # height stopped advancing?
dt node member status                            # in-set=<bool> validators=<n>
dt node peers                                    # which seat stopped talking

# on the missing host:
sudo systemctl start "$U"
journalctl -fu "$U"                              # watch it re-mesh and finalize

dt node status                                   # the height moves again
```

Voting the dead seat out is **not** a recovery step: `ducktape node member
remove <pubkey>` (and `ducktape node member leave`, which is the same path
aimed at self) opens a governance proposal that has to be proposed, voted and
executed *through the halted engine*. The ballot itself is fine — validator-mode
governance needs `total / 2 + 1` votes (`crates/modules/system/governance`), so
2 of 3 live seats carry it. It is the transaction that cannot land, not the vote
count. `ducktape node member promote` and `ducktape node resident accept` are the same
ceremony. Every membership change needs a live chain, so grow the set BEFORE
you need to.

If the seat is gone for good — the host is dead and `identity.key` was not
backed up — no verb helps, and there is no key-rotation verb to reach for
either: `ducktape node member` is `promote | remove | leave | status`, so the
key IS the seat. See `backup-and-keys.md`.

## macOS (launchd)

There is no systemd on a Mac, so the node runs as a **per-user LaunchAgent**:
as the logged-in user, out of that user's `~/.ducktape`, installable without
root. The agent runs `ducktape-node-launcher run` over the workspace, exactly
as the unit does. `ops/node/dev.ducktape.node.plist` is the agent, and it is a
template — a plist cannot expand `~` or resolve a workspace selector — which
`ops/node/install-macos.sh` renders and loads:

```sh
ducktape node init --name mynet        # found (or join) as yourself, first
ops/node/install-macos.sh --dry-run --workspace mynet   # print the rendered plist
ops/node/install-macos.sh --workspace mynet             # write it and bootstrap it
```

The script resolves the binary with `command -v ducktape` (`--binary` overrides
it) and the launcher beside it, resolves the selector to the workspace
directory (`ducktape node list`), seeds the first release from that binary
with `ducktape-node-launcher install` when the workspace has none yet, writes
`~/Library/LaunchAgents/dev.ducktape.node.plist`, and runs
`launchctl bootstrap gui/$(id -u) <plist>`. It is idempotent: a re-run
re-renders and re-loads, which is also how you change the workspace, the log
filter (`--rust-log`) or `DUCKTAPE_HOME` (`--home`); a workspace already under
the launcher keeps the release it runs. A second network on the same Mac needs
`--label <label>`, because the label is the agent's identity in the user's
launchd domain. `--uninstall` boots the agent out and removes the plist,
leaving the workspace alone.

```sh
launchctl print gui/$(id -u)/dev.ducktape.node    # state and last exit status
launchctl kickstart -k gui/$(id -u)/dev.ducktape.node   # restart it
launchctl bootout gui/$(id -u)/dev.ducktape.node        # stop it until re-bootstrapped
ops/node/install-macos.sh --uninstall                   # ...and forget it
```

What the plist carries, and why:

- `RunAtLoad` + `KeepAlive` + `ThrottleInterval 3` — the unit's
  `Restart=always` / `RestartSec=3`.
- `ExitTimeOut 150` — launchd's SIGTERM-to-SIGKILL gap, the unit's
  `TimeoutStopSec`: longer than the 120 s the launcher gives its node to take
  the graceful checkpoint path.
- `SoftResourceLimits`/`HardResourceLimits` `NumberOfFiles` 10240 / 65536. A
  GUI-launched process inherits a soft limit of **256** (`launchctl limit
  maxfiles` says `256 unlimited`), which the module stores blow past into a
  bare `EMFILE`; the node raises its own soft limit toward 65536 and is clamped
  to the lower of the hard limit and `kern.maxfilesperproc`
  (`crates/kernel/node/src/resource_limits.rs`), so the hard limit here is what
  that raise may reach and the soft limit is the floor if it refuses.
- `ProcessType Interactive` — an unset `ProcessType` lets launchd throttle the
  job's CPU and I/O, which a validator cannot bear.
- `EnvironmentVariables` for `DUCKTAPE_HOME`, `RUST_LOG`, and a `PATH` naming
  the Homebrew prefixes, since launchd hands a job a minimal one.

Logs: `~/Library/Logs/ducktape/node.err.log` (the tracing stream) and
`node.out.log`, plus the workspace's own `daemon.log` as on Linux. **Nothing
rotates the two Library files** — launchd has no logrotate — so truncate them
yourself if they grow; `daemon.log` is the copy to keep.

Before the compute plane will run on that Mac, `ops/macos-preflight.sh` has to
pass (the vz shim, its entitlement, the guest images). The rest of this
document — ports, the halt at three validators, the log-filter route, backup —
reads the same on both platforms.

## Listen ports

Defaults from `crates/workspace-config/src/node_toml.rs`, written into
`node.toml` by `init`/`join` and overridable with the plumbing flags
(`--listen`, `--http`, `--rpc`, `--wireguard-listen`, `--invite-listen`):

| Plane | Default | Proto | Inbound rule? |
| --- | --- | --- | --- |
| p2p control mesh (`listen`) | `[::]:8846` | TCP | **Yes** for a node others dial directly (a founder, a `Direct`-hinted member). A member that advertises `"overlay"` is dialed over the WireGuard tunnel instead. |
| WireGuard tunnel plane (`wireguard_listen`) | `0.0.0.0:51820` | UDP | **Yes** for an inviter / a node without a coordinator; the plane hole-punches through a coordinator otherwise. Bind the concrete IP on a LAN or VPS without a coordinator — an unspecified bind advertises an endpoint-less record and joiner↔joiner tunnels stay dark. |
| invite intro (`invite_listen`) | WireGuard port + 1 → `0.0.0.0:51821` | UDP | **Yes** on any node that mints invites (a joiner rings this doorbell first). |
| node HTTP API (`http_listen`) | `0.0.0.0:8844` | TCP | **Yes** for a remote desktop app or CLI. Reads are open to any peer; every mutating `/v1` route requires a per-request user signature or the workspace's operator token from a loopback peer, and `/v1/admin/*` follows `DUCKTAPE_ADMIN` (`crates/noded/src/admin.rs`). Every co-located process dials this plane over loopback whatever it is bound to. |
| operator rpc (`rpc_listen`) | `127.0.0.1:8845` | TCP | No — loopback only. |
| browser gateway (`gateway_listen`) | `127.0.0.1:0` | TCP | No — port 0, printed at boot, re-read per session. |

Outbound: UDP 3478 + TCP 443 to the coordinator
(`relay.ducktape.industries` by default) for rendezvous and the
first-contact relay fallback; HTTPS to `auth.ducktape.industries` from the
CLI for passkey/wallet ceremonies (not from the node).
