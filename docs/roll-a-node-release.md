# Roll a node release

Move a live network onto a new `ducktape` binary without founding it again.
The binary is published to the network's own duckfs, the network decides at
which block to run it, and every node's `ducktape-node-launcher` stages,
qualifies and flips on its own. Nothing is copied onto a host by hand.

This is for the node binary. A module's bytes move through the code registry
instead (`modules.update`), and the desktop app through its own channel.

## Order: a release that moves the module WIT ships AFTER the swap

A node binary carries the host half of the module WIT world
(`ducktape:module/host`); the components the network runs carry the other
half, and only the code registry moves those. A binary whose world moved
cannot link the components the network is running — and nothing about
publishing or designating it says so. Every launcher stages it, arms it at the
activation height, STOPS its node, and only then hears the qualify refuse:

```
checkpoint_unrestorable: restore compose: forge component loads: Module(component
imports instance `ducktape:module/host@0.1.0`, but a matching implementation was
not found in the linker: instance export `git-object-read` has the wrong type…)
node_update_refused      reason=checkpoint_unrestorable
```

Every node comes back on the release it was already running, so the network
keeps producing — but each one paid a stop for an answer that could never be
yes, and the refused designation stays the network's designation until another
one replaces it or the network withdraws it (see "Withdraw a refused release"
below).

`release schedule` enforces this before it proposes anything: it fetches the
archive it is about to designate off the network's own duckfs and asks its
executable to link the components the registry says this network is running
(`ducktape node qualify --compose-only`, which reads the roster over the rpc
and the component bytes out of the blob files — no lock, no node stopped). A
binary that cannot link them is refused locally, with the sentence above and
no ballot; `--skip-preflight-i-know-the-wit-moved` proposes anyway.

So a release that moves the module WIT ships AFTER the module swap that
matches it, never before: swap each affected module first
(`ducktape module update <id> <component.wasm> --after <blocks>`), wait for the
activation height to pass (`ducktape module status` names each module's active
code), and designate the binary after that. A release that leaves the WIT alone
has no order to keep.

## Before anything: the node must follow the channel

A workspace pins the release key it trusts at
`<workspace>/updates/keys/release.pub`. The NETWORK names that key: a
validator commits it through governance, on the carrier a designation rides —

```
ducktape release key set --kind node --pubkey <hex> --config <workspace>/node.toml
```

(every validator runs the same line until the ballot passes, as with
`release schedule`) — and every launcher whose workspace pins nothing pins it
on its first `release status` reading that carries it, logs
`node_update_release_key_pinned` once, and follows the channel from that poll
on. No member is touched, and nothing restarts. `ducktape release status`
prints the network's key as `release_key node` and the workspace's as
`pinned`.

Until the network commits a key, an unpinned launcher supervises the node and
nothing more: each designated release is refused `no_release_key`, with `this
workspace pins no release key, so it follows no node channel`, once per
release.

A pin already on disk is NEVER overwritten by the network's word. One that
differs from the committed key is refused `release_key_pinned_differs` (at
attempt 1, then every 60th poll, both keys named) and stays the key followed.

`ops/refound-net.sh` pins the workspace wallet's public key on both nodes as
it founds them, and its report says so:

```
  release key pinned bb584d3b…dcc3bea9 (both nodes)
```

An explicit pin — or the move of a differing one — is made in place, no
re-found, with the workspace's launcher stopped: `install` writes the
workspace like `run` does, and refuses one a running launcher holds
(`workspace_locked`). A stopped launcher stops its node first.

```
U="ducktape-node@$(systemd-escape '<chain id>')"
sudo systemctl stop "$U"
PUB=$(<workspace>/current/ducktape user key status --key <workspace>/keys/operator.key | awk '{print $NF}')
ducktape-node-launcher install --workspace <workspace> --config <workspace>/node.toml \
    --from <workspace>/current/ducktape --release-key "$PUB"
sudo systemctl start "$U"
```

`--from` names the release the workspace is already running, so nothing is
copied and `current` does not move; `state.json` starts over at idle on that
release, and the pin is the one new file. The launcher reads it when it
starts.

## 1. Archive the release

A node archive is a `.tar.zst` carrying `ducktape`, `ducktape-node-launcher`
and the founding set as `modules/`, all at its root. Symlinks, hard links,
absolute paths and `..` are refused by the launcher rather than unpacked.

```
cargo build --release -p node-bin -p node-launcher
ops/release/archive.sh --kind node --from target/release \
    --sequence 1 --display "0.1.0+97b4ef7bc"
```

`--sequence` and `--display` write `release.json` at the archive root, so a
host that runs `ducktape-node-launcher install --from` over the extracted
`ducktape` pins that sequence and reads the channel publishing it as up to
date instead of downloading it again; pass the same two values to
`publish.sh`, which refuses an archive whose `release.json` says otherwise
(`release_identity_mismatch`).

`--from` is the profile directory that build wrote: it holds both binaries and
the founding set the same build staged beside them under the name of the
checkout it ran in, so run the script from that checkout. It packs that
set under `modules/`, the one name `workspace_config::modules_dir()` resolves
beside an executable. That is what lets a host with nothing but this archive
run `ducktape node init` — a binary carries no wasm, so a release without the
set founds nothing. The script refuses a set that holds no components, holds
no `netstack.component.wasm`, or carries no `.staged-by` record for the binary beside it to match.

The printed file name is for the eye. The published name is derived from the
archive's own sha256 by `app_update::layout`.

## 2. Publish it

```
RELEASE_WALLET_PASSWORD=… ops/release/publish.sh --kind node \
    --node http://127.0.0.1:<http port> \
    --key <workspace>/keys/operator.key \
    --sequence 1 --display "0.1.0+97b4ef7bc" \
    --archive linux-x86_64=ducktape-linux-x86_64.tar.zst \
    --verified-sha <the sha256 archive.sh printed for a second build>
```

Publish refuses a node archive whose sha256 no `--verified-sha` names
(`archive_not_reproduced`): build and archive the same commit a second time,
in another checkout with its own target directory, and pass the sha256 that
second `archive.sh` printed.

It composes the manifest, signs it with that wallet, and lands the archive,
the manifest and the signature under `/shared/releases` — archives first, so
no reader ever sees a manifest naming an archive that is not there yet.
`--sequence` is the monotonic downgrade guard: strictly above the last
published. The key here must be the one the nodes pin.

## 3. Designate it

Publishing puts the bytes on the network. WHEN to run them is a separate
governance decision, and every member co-signing it passes the same numbers:

```
ducktape release schedule --sha <the archive's sha256> --lead <blocks> --config <workspace>/node.toml
```

The member who proposes passes `--lead`: the activation height is that many
blocks past the committed height the verb reads AFTER its preflight — which
fetches and links the archive, and takes as long as that takes — and the verb
prints the height it designated. Every other member co-signs with that
`--at <height>`.

Either way, a height that leads the proposal by less than one launcher poll
(`LAUNCHER_POLL_MS`, 2000 ms) of blocks at the network's beat is refused as
`activation_lead_too_short`, with both heights and the minimum, and nothing is
proposed. That is the floor, not a margin: each launcher stages the release in
the poll that first sees it, so leave room for the archive's download on the
slowest validator's link.

`release status` is what the launcher reads and what an operator watches:

```
ducktape release status --json --config <workspace>/node.toml
{"base":"http://127.0.0.1:28800","height":6454,
 "designation":{"activation_height":6468,"sha256":"619b38a3…"}, …}
```

## 4. Watch it land

The launcher logs to its stderr — under the unit, the journal
(`journalctl -u "ducktape-node@$(systemd-escape '<chain id>')"`) — under
`ducktape::update`, and its events are the whole story in order:

```
node_update_offered      the network designates a release this node is not running
node_update_downloading  reading it off the network        release=… size=27463016
node_update_staged       staged, waiting for its height    display="0.1.0+97b4ef7bc"
node_update_arming       armed at the committed height; stopping the node to qualify it
node_update_qualified    the staged binary reopened the workspace checkpoint at the committed root
node_update_flipped      from=<old sha> to=<new sha>
node_update_launcher_exec release=<new sha> from=<short> to=<short>  only when the release ships another launcher
node_update_exec         starting the node
```

After a flip the supervisor runs the launcher the release shipped: with the
old node stopped, it `exec`s `<workspace>/current/ducktape-node-launcher` in
its own pid when those bytes differ from its own, and that image starts the
node. A supervisor whose image never logs `node_update_launcher_exec` cannot
take this step, and restarting it runs the same image again: on such a node,
install the release's launcher over the path the unit runs
(`install -m 0755 <workspace>/current/ducktape-node-launcher <ExecStart path>`)
and restart the unit once; every later flip moves the launcher by itself.

Staging is early and the flip is late: the bytes land while the designation is
still in the future, and the node is stopped only once, to qualify the binary
against its own checkpoint. A release that cannot reopen the checkpoint does
not flip — the node starts the one it was already running.

What is staged is the release the network DESIGNATES, looked up in the
channel's manifest (`/shared/releases/node.json` — only the latest publish has
a path, so a designation is published when that manifest names its sha for the
node's platform). One it does not name is refused
`designated_release_unpublished` rather than waited on; publishing it lets the
next attempt stage it. A designation that moves while a release is staged
discards the staged one and stages the new one.

A refusal line carries `class`. `class=transient` is a read that did not
complete — `fs cat` failing (`download_failed`), an archive served short of
its size (`short_read`), a manifest not naming the designation yet: the
launcher asks again after a backoff (the next poll, doubling to 32 polls) and
says so at attempt 1 and every 60th, with `attempts`. `class=definite` is the
release's own answer — a bad signature, an archive longer than the manifest
says or of another hash, a qualify that refused, no key: said once, and not
asked again until the launcher restarts.

Afterwards `/v1/status` carries the new `version`, and the node spends its
recovery window reporting `phase: "recovering"` before it resumes producing.

### Withdraw a refused release

A launcher that refused a release definitely does not ask again until it
restarts, but the designation stays the network's: a restarted launcher or a
node joining later fetches it and refuses it again. Take it back with the same
ceremony, every member passing the same sha:

```
ducktape release withdraw --sha <the archive's sha256> --config <workspace>/node.toml
```

It fetches and runs nothing (there is no preflight), and it refuses a sha the
network does not designate. A withdrawal names one release: once it passes,
`release status` answers with the latest designation of any OTHER release
that no withdrawal has taken back, or with none.

### Take a release back

A release that flipped and came up healthy, then misbehaves, is taken back by
withdrawing it and designating the release before it again — two ceremonies,
in this order, every member passing the same shas:

```
ducktape release withdraw --sha <the misbehaving release's sha256> --config <workspace>/node.toml
ducktape release schedule --sha <the previous release's sha256> --lead <blocks> --config <workspace>/node.toml
```

The withdrawal alone moves no node: the misbehaving binary is what `current`
names, and a network that designates nothing asks nothing of its launchers.
Withdrawing it first keeps it out of the standing designations, so no later
withdrawal can hand the network back to it.

Each launcher keeps the release it flipped away from as `previous`, sealed
under `updates/releases/<sha>`; its sha is the directory `previous` names:

```
basename "$(readlink <workspace>/previous)"
```

A designation naming it is staged from there — nothing is downloaded, and it
need not be the release the manifest names — then qualified and flipped at the
activation height like any other:

```
node_update_offered      release=<previous sha>
node_update_staged       release=<previous sha> display=<its short sha>
node_update_arming       armed at the committed height; stopping the node to qualify it
node_update_qualified    the staged binary reopened the workspace checkpoint at the committed root
node_update_flipped      from=<misbehaving sha> to=<previous sha>
```

Afterwards the release taken back is the one kept as `previous`. The verb's
preflight reads the designated archive off duckfs, so it runs for a release
that was published; one that never was (the binary `install` seeded) has no
archive there, and only `--skip-preflight-i-know-the-wit-moved` designates it.
A release two flips back is no longer on any node's disk: designating it is an
ordinary designation, staged only if the channel's manifest names it.

## 5. Check it

```
<workspace>/current/ducktape --version        # the new stamp
ducktape service status --config <workspace>/node.toml   # every kind still enabled
```

and the whole-chain check the re-found ends on, which is what proves the new
binary still runs the network rather than merely booting:

```
printf '%s\n' "$PASSWORD" | ops/refound-smoke.py --node http://127.0.0.1:<port> \
    --workspace <workspace> --binary <workspace>/current/ducktape \
    --key <workspace>/keys/operator.key --agent-id postflip --name PostFlip --channel postflip
```
