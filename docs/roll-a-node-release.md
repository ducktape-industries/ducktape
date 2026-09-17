# Roll a node release

Move a live network onto a new `ducktape` binary without founding it again.
The binary is published to the network's own duckfs, the network decides at
which block to run it, and every node's `ducktape-node-launcher` stages,
qualifies and flips on its own. Nothing is copied onto a host by hand.

This is for the node binary. A module's bytes move through the code registry
instead (`modules.update`), and the desktop app through its own channel.

## Before anything: the node must follow the channel

A workspace pins the release key it trusts at
`<workspace>/updates/keys/release.pub`. Without that file the launcher
supervises the node and nothing more — every designation is refused
`no_release_key`, with `this workspace pins no release key, so it follows no
node channel`.

`ops/refound-net.sh` pins the workspace wallet's public key on both nodes as
it founds them, and its report says so:

```
  release key pinned bb584d3b…dcc3bea9 (both nodes)
```

A network founded before that, or by hand, is pinned in place — no re-found:

```
PUB=$(<workspace>/current/ducktape user key status --key <workspace>/keys/operator.key | awk '{print $NF}')
ducktape-node-launcher install --workspace <workspace> --config <workspace>/node.toml \
    --from <workspace>/current/ducktape --release-key "$PUB"
```

`--from` names the release the workspace is already running, so nothing is
copied and `current` does not move; the only new file is the pin. The key is
read once per node life, so the node child is restarted afterwards — stop it
and its supervisor boots it again:

```
kill $(pgrep -f "<workspace>/updates/releases/.*/ducktape")   # or find it by /proc/<pid>/exe
```

`/proc/<pid>/exe` resolves THROUGH the `current` symlink, so a node child's
executable path names its release directory, never `current/`.

## 1. Archive the binary

A node archive is a `.tar.zst` carrying one executable `ducktape` at its root.
Nothing else is required, and symlinks, hard links, absolute paths and `..`
are refused by the launcher rather than unpacked.

```
mkdir -p stage && cp target/release/ducktape stage/ducktape && chmod 755 stage/ducktape
tar --zstd -cf ducktape-linux-x86_64.tar.zst -C stage ducktape
```

The file name is for the eye. The published name is derived from the
archive's own sha256 by `app_update::layout`.

## 2. Publish it

```
RELEASE_WALLET_PASSWORD=… ops/release/publish.sh --kind node \
    --node http://127.0.0.1:<http port> \
    --key <workspace>/keys/operator.key \
    --sequence 1 --display "0.1.0+97b4ef7bc" \
    --archive linux-x86_64=ducktape-linux-x86_64.tar.zst
```

It composes the manifest, signs it with that wallet, and lands the archive,
the manifest and the signature under `/shared/releases` — archives first, so
no reader ever sees a manifest naming an archive that is not there yet.
`--sequence` is the monotonic downgrade guard: strictly above the last
published. The key here must be the one the nodes pin.

## 3. Designate it

Publishing puts the bytes on the network. WHEN to run them is a separate
governance decision, and every member co-signing it passes the same numbers:

```
ducktape release schedule --sha <the archive's sha256> --at <height> --config <workspace>/node.toml
```

Pick a height a little ahead of the committed one. `release status` is what
the launcher reads and what an operator watches:

```
ducktape release status --json --config <workspace>/node.toml
{"base":"http://127.0.0.1:36989","height":6454,
 "designation":{"activation_height":6468,"sha256":"619b38a3…"}, …}
```

## 4. Watch it land

The launcher's events are in `<workspace>/launcher.log`, under
`ducktape::update`, and they are the whole story in order:

```
node_update_offered      the network designates a release this node is not running
node_update_downloading  reading it off the network        release=… size=27463016
node_update_staged       staged, waiting for its height    display="0.1.0+97b4ef7bc"
node_update_arming       armed at the committed height; stopping the node to qualify it
node_update_qualified    the staged binary reopened the workspace checkpoint at the committed root
node_update_flipped      from=<old sha> to=<new sha>
node_update_exec         starting the node
```

Staging is early and the flip is late: the bytes land while the designation is
still in the future, and the node is stopped only once, to qualify the binary
against its own checkpoint. A release that cannot reopen the checkpoint does
not flip — the node starts the one it was already running.

Afterwards `/v1/status` carries the new `version`, and the node spends its
recovery window reporting `phase: "recovering"` before it resumes producing.

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
