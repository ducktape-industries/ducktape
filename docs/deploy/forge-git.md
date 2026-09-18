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
```

Setup links `git-remote-duck` beside the `ducktape` it ran as (that link is
the helper; there is no second binary) and prints every change it made; a
rerun prints what is already in place. It refuses on a machine with no
registered network: redeem an invite first. `--node` reads the chain id off
that node's `/v1/status` and registers it as a remote workspace
(`<ducktape home>/<label>-<salt>/remote.toml`); `ducktape node list` shows it
as `remote node <url>`.

## What the helper does

git runs `git-remote-duck <remote> <duck://address>`. The helper:

1. parses the address (a malformed one is refused with the parser's sentence);
2. finds the ONE registered workspace, local or remote, whose chain id has
   the address's salt, and refuses when none does, when several do, or when
   the registry knows that salt under another label. Nothing else selects a
   network: not `-n`, not `DUCKTAPE_NODE`, not the lone workspace;
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

Cargo's built-in fetcher cannot run a remote helper, so a repository with a
`duck://` git dependency commits `.cargo/config.toml`:

```toml
[net]
git-fetch-with-cli = true
```

A dependency is pinned by commit, and the lock records the source URL and the
commit: moving one dependency between its GitHub URL and its `duck://` address
edits that URL in `Cargo.toml` and in `Cargo.lock`'s `source =` line, never
the `#<sha>`.

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
