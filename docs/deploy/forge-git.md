# Git over `duck://` addresses

A Forge repository has one address a developer commits:

```
duck://<label>-<salt>/forge/<owner>/<repo>
```

`<label>-<salt>` is the network's chain id (`<label>#<salt>` in
`ducktape node list`) spelled for a URL. `<owner>` is the handle of the
account whose Gateway route `git` serves the network's Forge (the Git service
in `application-service.md`); `<repo>` is the Forge repository name. So the
address above is the repository `<repo>` behind the route `git.<owner>.duck`.

## One-time setup

```sh
ducktape forge setup                       # a machine with a workspace
ducktape forge setup --node <http-url>     # a machine that only reaches a node
ducktape forge setup --config <node.toml>  # several workspaces on one network
```

Setup links `git-remote-duck` beside the `ducktape` it ran as (that link is
the helper; there is no second binary) and prints every change it made; a
rerun prints what is already in place. It refuses on a machine with no
registered network: redeem an invite first. `--node` reads the chain id off
that node's `/v1/status` and registers it as a remote workspace
(`<ducktape home>/<label>-<salt>/remote.toml`); `ducktape node list` shows it
as `remote node <url>`.

Setup resolves every registered network exactly as the helper will, and links
nothing while any of them would be refused. A home holding two workspaces of
one network (a validator and its resident, as `ops/refound-net.sh` lays them
out) needs one picked: `--config <path>` records that workspace's file (as the
refusal lists it) in `<ducktape home>/git-workspaces.toml`, and every address
on that network then resolves to it. Setup also reads each network's Gateway
state and names what its Git door lacks: an account with a handle, and that
account's published `git` route.

## Opening the door on a network

The following is the complete path on a founded Linux node. It uses the
independently installed Git service described in [`application-service.md`,
"Git service"](application-service.md#git-service), the existing Gateway
installer, and the existing account and Forge verbs. It does not require
constructing or submitting a Gateway frame by hand.

### Install the Git service

Build or obtain a trusted `ducktape-forge-service` executable and record its
SHA-256. On the node host, set these to the node's actual values:

```sh
NODE=http://127.0.0.1:8844
WORKSPACE=/var/lib/ducktape/<chain-id>
ACCOUNT=<account-number>
PORT=<unused-loopback-port>
BINARY=/srv/releases/ducktape-forge-service
STATUS=$(mktemp)
MANIFEST=/etc/ducktape/forge.json
SEED=/etc/ducktape/forge-signing-seed
curl -fsS "$NODE/v1/status" >"$STATUS"
sudo install -d -m 0750 /etc/ducktape
if ! sudo test -s "$SEED"; then
  sudo python3 - "$SEED" <<'PY'
import os
import secrets
import sys

fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as output:
    output.write(secrets.token_hex(32))
PY
fi
sudo chmod 0600 "$SEED"
```

The service seed is a new private key for this service, not the node key and
not the account wallet. Generate it once and keep it root-readable. Create the
installer manifest without printing the seed or any Gateway credential:

```sh
sudo python3 - "$STATUS" "$SEED" "$MANIFEST" "$NODE" "$WORKSPACE" \
  "$ACCOUNT" "$PORT" "$BINARY" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import sys

status_file, seed_file, manifest_file, node, workspace, account, port, binary = sys.argv[1:]
status = json.loads(Path(status_file).read_bytes())
seed = Path(seed_file).read_text().strip()
binary_path = Path(binary)
workspace_path = Path(workspace)
config = {
    "node_url": node,
    "node_key": status["public_key"],
    "chain_id": status["chain_id"],
    "account": int(account),
    "label": "git",
    "module": "forge",
    "git_store": "/var/lib/application-storage/git",
    "signing_seed": seed,
}
manifest = {
    "name": "forge",
    "binary": str(binary_path),
    "sha256": hashlib.sha256(binary_path.read_bytes()).hexdigest(),
    "workspace": str(workspace_path),
    "node_user": "ducktape",
    "account": int(account),
    "label": "git",
    "port": int(port),
    "config": config,
    "memory_max": 536870912,
    "cpu_quota": 100,
    "tasks_max": 64,
    "readonly_paths": [{
        "source": str(workspace_path / "storage" / "forge-repo"),
        "destination": "/var/lib/application-storage/git",
    }],
    "devices": [],
}
payload = json.dumps(manifest, indent=2) + "\n"
fd = os.open(manifest_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
try:
    os.fchmod(fd, 0o600)
    with os.fdopen(fd, "w") as output:
        output.write(payload)
except BaseException:
    os.close(fd)
    raise
PY
rm -f "$STATUS"
```

`WORKSPACE/storage/forge-repo` is the node's materialized Forge store. The
installer mounts only that directory read-only; it must already exist and its
directories and files must permit the isolated service to read them. Do not
mount the whole workspace, `storage`, a node key, or an administrator
credential. The manifest's `git_store` is the mount destination, not the host
path.

Install and activate through the existing helper. Activation generates the
private upstream handoff credential and performs the local `gateway bind`; do
not create or copy that credential yourself:

```sh
sudo python3 ops/application-service/install.py \
  --ducktape /usr/local/bin/ducktape install "$MANIFEST"
sudo python3 ops/application-service/install.py \
  --ducktape /usr/local/bin/ducktape activate forge
systemctl status ducktape-application-forge.socket ducktape-application-forge.service
```

### Set the owner and publish the route

Use the wallet of `ACCOUNT` on the node that runs the service. The signing
verbs read its password from stdin; they do not take a password or a raw frame
on the command line:

```sh
DT=(sudo -u ducktape env DUCKTAPE_HOME=/var/lib/ducktape /usr/local/bin/ducktape)
"${DT[@]}" account set-handle --handle <owner> -n <chain-id>
"${DT[@]}" forge publish -n <chain-id>
```

`gateway bind` is the installer activation step above. It binds the service's
exact account and `git` label to the port, while `forge publish` signs the
network-audience GET/POST route whose authority is `git.<owner>.duck`.
`forge publish` signs the `git` route of the wallet's account with the node it
dials as the publisher: GET and POST, audience `network`, no byte cap either
way. A rerun that would publish the same route publishes nothing; a publish
from another node continues the route's revision stream.

Finally, on each client machine, run the one-time helper setup and use the
address whose owner is the handle just assigned:

```sh
ducktape forge setup --config <node.toml>   # or --node <http-url>
git clone duck://<label>-<salt>/forge/<owner>/<repo>
```

`forge setup` checks the same door the helper will use and names the missing
handle or route if the path is incomplete. Re-running the installer activation,
`account set-handle`, `forge publish`, or `forge setup` is safe and idempotent.

## What the helper does

git runs `git-remote-duck <remote> <duck://address>`. The helper:

1. parses the address (a malformed one is refused with the parser's sentence);
2. finds the ONE registered workspace, local or remote, whose chain id has
   the address's salt — the one `forge setup --config` picked when several
   do — and refuses when none does, when several do and none is picked, or
   when the registry knows that salt under another label. Nothing else
   selects a network: not `-n`, not `DUCKTAPE_NODE`, not the lone workspace;
3. asks that workspace's node for its browser Gateway base
   (`GET /v1/gateway/browser`) and hands the conversation to git's own
   `git remote-http` on `<gateway>/<repo>` with the header
   `x-duck-authority: git.<owner>.duck`.

No credential travels: a read is admitted by the route's audience, and a push
is authorized by the certificate `git push --signed` signs against the
service's nonce, with the pusher's SSH key. An unsigned push is refused.

The browser Gateway listens on its node's own 127.0.0.1. A remote workspace
whose node this machine dials on a loopback base (its http and gateway ports
forwarded here at the same numbers, `ssh -L`) works as a local one; a node
dialed across the network is refused (`gateway_not_local`), never dialed at
this machine's loopback.

## Cargo

Cargo's built-in fetcher cannot run a remote helper. `forge setup` writes

```toml
[net]
git-fetch-with-cli = true
```

into `$CARGO_HOME/config.toml` (`~/.cargo/config.toml` by default), so every
Cargo on this machine shells out to `git` and reaches the helper. A `[net]`
table that is already there is left alone and named instead.

A lock names one source per crate, so a dependency graph moves onto a network
at the git layer, not in its manifests:

```sh
ducktape forge setup --instead-of https://github.com/ducktape-industries/
```

records

```
url.duck://<chain-id>/forge/<owner>/.insteadOf = https://github.com/ducktape-industries/
```

in this repository's git configuration, or the user's with `--global`, and
`--owner <handle>` picks the Forge when the network serves more than one. Every
dependency under the prefix — direct or transitive, in any crate of the graph —
then resolves through the network, while `Cargo.toml` and `Cargo.lock` keep the
URL and the `#<sha>` they have, so `--locked` holds. A rerun writes nothing; a
prefix already rewritten to another Forge is refused, naming the `git config
--unset` that clears it.

## Mirroring GitHub into Forge

`ops/forge-mirror.sh` runs on the node's host under
`ops/node/ducktape-forge-mirror.timer`, every five minutes. Per repository and
branch it fetches the source, reads Forge's head with `ops/forge-import.py
head`, and pushes the fetched tip with `ops/forge-import.py push` when Forge is
empty or behind. A rerun with nothing new pushes nothing. A non-fast-forward
is refused, never force-synced: the pass fails and says whether the source
rewound or something other than the mirror moved Forge. A human reconciles.

Install:

```sh
sudo install -d /usr/local/lib/ducktape /etc/ducktape
sudo install -m 0755 ops/forge-mirror.sh ops/forge-import.py /usr/local/lib/ducktape/
sudo install -m 0644 ops/node/ducktape-forge-mirror.service \
  ops/node/ducktape-forge-mirror.timer /etc/systemd/system/
sudo tee /etc/ducktape/forge-mirror.env <<'EOF'
FORGE_MIRROR_NODE=http://127.0.0.1:8844
FORGE_MIRROR_TOKEN_FILE=/var/lib/ducktape/<chain-id>/admin.token
FORGE_MIRROR_SOURCE=https://github.com/ducktape-industries
FORGE_MIRROR_REPOS=ducktape-sdk ducktape
FORGE_MIRROR_BRANCHES=dev main
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now ducktape-forge-mirror.timer
```

A refused pass shows in `journalctl -u ducktape-forge-mirror` as a line
starting `[forge-mirror] REFUSED`.
