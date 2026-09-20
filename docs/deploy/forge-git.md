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

On the node that runs forge's Git service (`application-service.md`, "Git
service"), with the wallet of the account that will own the door (each signing
verb reads the wallet password on stdin):

```sh
ducktape account set-handle --handle <owner> -n <chain-id>
ducktape gateway bind --label git --port <service-port> \
  --credential-file <handoff-token> -n <chain-id>
ducktape forge publish -n <chain-id>
```

`forge publish` signs the `git` route of the wallet's account with the node it
dials as the publisher: GET and POST, audience `network`, no byte cap either
way. A rerun that would publish the same route publishes nothing; a publish
from another node continues the route's revision stream.

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
