# Re-found a network

A ducktape network is re-founded, not migrated. There is no compat path and no
upgrade path: when a wire moves, the old chain is replaced by a new one founded
from the current build, its content reset, and its forge repos re-imported.

`ops/refound-net.sh` does the whole sequence in one command, and is written to
be run again over the same directories.

```
ops/refound-net.sh --root ~/.ducktape/dognet --yes \
    --guest ~/.ducktape/guest --mirror ~/dev/ducktape/ducktape
```

It stops what is running, archives the workspaces, founds a validator, joins a
resident, installs the agent executors, mints the workspace wallet and founds
its account, starts the service daemons, mirrors a repo into the forge,
rebuilds the app, and prints the ports, chain id and contract an operator
needs.

## The target is explicit

`--root` is required and has no default. Every destructive step is scoped to
the directories named on the command line, and the script refuses to stop or
archive anything without `--yes`.

Processes are found by `/proc/<pid>/exe` and by the workspace path in their
argv — never by a `pkill -f` pattern, which also matches an editor, a grep, or
another network's node.

Workspaces are ARCHIVED, never deleted: each is moved to
`<path>.archived-<timestamp>` and every archived path is printed in the final
report. A re-found resets content by design, so that copy is the only one.

## What each flag is for

| Flag | Why |
| --- | --- |
| `--root DIR` | the founder's workspace root. Required. |
| `--joiner-root DIR` | the resident's root (default `<root>-joiner`). |
| `--name NAME` | the network name (default: the root's basename). |
| `--binary PATH` | a node binary. Default: build one from this checkout. |
| `--guest DIR` | a guest image (`vmlinux`, `rootfs.ext4`) installed as the workspace's own. |
| `--mirror REPO` | a git checkout to import into the network's forge. |
| `--port-offset N` | add N to every port. |
| `--wallet-name`, `--wallet-password` | the workspace's active wallet and the account founded for it. |
| `--skip-app` | do not rebuild the desktop app. |
| `--yes` | proceed past stopping and archiving an existing root. |

## What the script encodes, and why

**The binary and the founding set are one artifact.** Both are copied to a
staging directory and the network is founded from the copies. A shared cargo
target directory holds whichever checkout built it last, so a binary taken
straight from one may belong to a sibling whose wire has already moved; the
copy must prove its ancestry (`git merge-base --is-ancestor`) against this
checkout's HEAD before anything is founded with it. The set is checked for
`*.pending` markers, which are views whose staging was interrupted.

**The launcher's child needs the set too.** `ducktape-node-launcher` runs
`<workspace>/current/ducktape`, and a node resolves its founding set beside its
own binary — so under the launcher there is nothing beside it to find. The
workspace keeps its own copy and the child is pointed at it with
`DUCKTAPE_MODULES_DIR`, which also survives a release flip moving the binary to
a new directory. Without it the failure is not a genesis error: the reachability
plane refuses with `netstack_guest_unreadable`, WireGuard and the invite
listener never bind, and a joiner that cannot redeem its invite dials the p2p
port forever and is answered `PeerRejected`.

**The launcher supervises, so it is stopped first.** It restarts its child when
the child exits, and its own executable lives outside the workspace it runs. A
teardown that matches only the workspace path kills the node and gets a fresh
one a second later.

**Ports are checked after the teardown, not before.** Freeing them is what the
teardown does; checking first refuses every re-run. A port still listening
afterwards belongs to something else — two workspaces sharing the fixed
defaults is the failure that surfaces later as a join blaming its invite.

**Executors are installed before the services are granted.** A grant snapshots
the host's capabilities. A compute service granted before the agent CLIs exist
takes `capabilities=[]`, announces no provider tag, and refuses every run with
`accept_not_capability_provider` until it is disabled and enabled again.

**`service run --enable`, not `service enable`.** The latter consents to a
daemon that is already signalling; with nothing running it refuses. The script
starts each daemon and grants it in one step. `compute` and `agent` open the
sandbox at boot and exit without a microVM kernel, so they are started only
when `--guest` is given.

**A fresh workspace has no wallet, and a fresh chain has no account.** Every
keyless verb signs with the active wallet and the service daemons refuse to
boot without one. `wallet new` prints a mnemonic, so the script writes it to a
`0600` file in the workspace and never to its own output. The account is the
on-chain identity that key belongs to, and `account create` is a submitted
transaction, so it runs against the serving founder rather than beside `node
init`. Without it a daemon does not stop at the grant: it enables, announces,
and then exits `FATAL: the active wallet key is on no account`.

**A grant line is not a live daemon.** `announced at height N` is printed
before the daemon has finished booting, so the script waits out the exit
window and then requires the process to still be there — found by the kind and
config in its argv, since the pid `$!` returns belongs to the `setsid` wrapper
and not to the daemon it execs.

## After it runs

Two workspaces now share one chain id, so `-n <chain>` is ambiguous and
resolves to whichever registration it finds first. Name the config instead:

```
ducktape node log-filter 'info,ducktape::join=debug' --config <workspace>/node.toml
```

Both nodes run under `ducktape-node-launcher`, so a later core update flips
through the release plane rather than by replacing a file.
