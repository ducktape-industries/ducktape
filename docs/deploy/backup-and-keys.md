# Backup and keys — what to copy, what losing it means

What a ducktape host keeps on disk, which of it is a secret, which of it is
irreplaceable, and what a restore looks like. Paths are relative to
`DUCKTAPE_HOME` (`~/.ducktape` by hand, `/var/lib/ducktape` under the
shipped units — see [node-service.md](node-service.md)) and every claim
cites the code that writes the file.

There is no `backup`, `restore` or `rotate` verb. Backup is `cp`; restore
is putting the files back and starting the node. Procedures marked
**untested** were never rehearsed on a live network — read them as the
expected shape, not as a promise.

## The inventory

| File | Written by | Secret? | Lose it and… |
| --- | --- | --- | --- |
| `<chain-id>/identity.key` | `node init` / `node join` (`crates/workspace-config/src/identity.rs`: hex ed25519 seed, `create_new`, mode 0600, read without a password so unattended boot works) | **yes** | **This node's seat is gone.** It is the mesh identity, the frame-signing key and the validator-set key. A resident replica rejoins from a new invite with a fresh key. A **validator** cannot: the set still counts the old key, so losing one seat key on an n≤3 network **halts the chain** with no verb left that can vote the seat out — see *Validator count: three seats tolerate nothing* in `node-service.md` for the arithmetic. |
| `<chain-id>/node.toml`, `network.toml` | `node init` / `node join` | no | Regenerable from another member: `node join` a fresh invite into the same directory rewrites both (`crates/workspace-config/src/join.rs`, idempotent on an existing `identity.key`). Back them up anyway — they carry the listeners, `[sandbox]` table and reach hints you tuned. |
| `<chain-id>/storage/` | the node at runtime | no (see airlock below) | Consensus state, checkpoints, the blob store, `mesh-state.json`, forge repos. **Regenerable by state sync** from any current validator — for a resident. For a validator this is the part nobody has rehearsed (below). |
| `<chain-id>/storage/airlock-creds/seal.key` | `ducktape service run airlock` on first boot (`crates/services/airlock/src/lib.rs`, `load_or_create_seal_keypair`, 0600) | **yes** | The seal keypair every credential this node lends is sealed to; its public half is what `user cred add` publishes on chain and borrowers pin. Lose it and every credential in `airlock-creds/` is unreadable and every published seal is stale: re-add every credential (`ducktape user cred add`). |
| `<chain-id>/storage/airlock-creds/*` | `ducktape user cred add` | **yes** | The lent provider credentials (OAuth refresh tokens, API keys), sealed to `seal.key`. Re-addable from the provider. |
| `<chain-id>/service-link.token` | the node at boot (`crates/noded/src/services.rs`, `LINK_TOKEN_FILE`, 0600) | **yes** | The node↔daemon link secret. Regenerated at the next boot; nothing to back up, everything to keep private (a holder can attach as a service daemon). |
| `<chain-id>/wireguard.key` | the node / `node join` (`WireGuardKeypair::load_or_generate`) | yes | The tunnel keypair. Regenerable: peers learn the new public key from the next signed mesh record. Losing it costs one re-assembly of every tunnel. |
| `<chain-id>/services.toml`, `work-admit.toml`, `gateway-routes.json`, `invite-fronts.json` | the `service` / `node work` / `gateway` verbs | no | Operator consent and routing. Re-runnable verbs; back them up to avoid re-consenting. |
| `<chain-id>/coord.cap` | the join flow (`crates/workspace-config/src/lib.rs`, `COORD_CAP_FILE`) | no (a capability, not a secret) | The coordinator admission capability a member was issued. Without it a member behind NAT cannot rendezvous through a private (`--genesis-set`) coordinator until re-issued. |
| `<chain-id>/keys/<name>.key` + `<chain-id>/keys/active` | `ducktape wallet new` / `wallet import` (`crates/keystore/src/userkey.rs`: argon2id + XChaCha20-Poly1305 at rest, born 0600 with `create_new`; `wallet.rs` owns only the `keys/<name>.key` naming and the `active` pointer, per workspace: a wallet is an identity on one network) | **yes** (encrypted) | **Your user identity on that network** — the key your account's ops are signed with. The 24-word mnemonic is the backup, and you are handed it exactly once (*The 24 words*, below). Lose both file and phrase and the account is unreachable until another of its keys adds a new one (`ducktape account key add`); an account with one key is gone. `active` is a one-line pointer, regenerable with `ducktape wallet use`. |
| `<chain-id>/genesis` | `node init` (composed from the founding set), `node join --genesis`, or the node's first boot (fetched off the mesh) | no | The network's wasm: every component and index guest, pinned whole and per component by `network.toml`. The node's blob store keeps a copy under that pin and rewrites a missing file from it; a member that lost both takes any other member's copy — the pin refuses any other bytes. |
| `<chain-id>/executors/*`, `<chain-id>/guest/*` | `ducktape agent install -n <chain-id>`, `OUT=<workspace>/guest ops/build-guest-rootfs.sh` | no | Rebuildable per workspace from the repository at the same commit: the agent CLIs at their pins (`executors.toml` beside the directory records them) and the guest images. |
| `/usr/local/lib/ducktape/modules/*.wasm` (outside the home) | `make install-node` (staged beside the built binary, copied there by the installer) | no | The founding set: what `node init` composes a genesis from, and where the netstack guest is read at boot. It pins nothing — a founded network's bytes are its `genesis` file's. |

Nothing under `~/.cargo`, `/tmp/dt-vm-*` or `$XDG_RUNTIME_DIR` needs copying.

### The 24 words

`ducktape wallet import <name>` rebuilds the **key inside** `<chain-id>/keys/<name>.key`
from the mnemonic, byte for byte — stdin takes the mnemonic line first, then a
password line (`bin/node/src/wallet_cli.rs`). The **file** is not reproduced
and never will be: every seal draws a fresh argon2 salt and nonce
(`seal_user_key`) under whatever password you hand it, so the ciphertext
differs on every restore. Check a restore by the pubkey `wallet import`
prints, never by comparing file hashes: it is the identity, and it comes back
identical under a *different* password
(`crates/keystore/src/wallet.rs`, `create_import_activate_round_trip`). That
is what the words are for; there is nothing else to back up about a user key.

You are handed them exactly **once**. `ducktape wallet new` prints them, and
the desktop app's first run shows all 24 and asks three of them back at random
positions — getting those right is what writes the key file, so an abandoned
ceremony leaves no key behind. Neither surface will show them a second time.

While you still hold the key file *and* its password, `ducktape user key
reveal --key $DUCKTAPE_HOME/<chain-id>/keys/<name>.key` reads the words back out of it.
That is the only reveal there is, and it is no help once the disk is gone.

Chat, DMs and members-only channels are **replicated in the clear** to every
member's node (the chat module gates posting, not reading, and holds no
encryption) — never paste an `identity.key` hex, a wallet password or an
invite blob into chat. Credentials go through `ducktape user cred add`.

An **invite blob** is a secret in its own right: besides the single-use
admission token it names the inviter's reachable members — their node
identities, WireGuard public keys, overlay ports, and public endpoints for
the host-capable ones (`docs/records/architecture/reachability.md` §5). That
is data the signed mesh already carries, but a leaked blob widens who sees it
from admitted members to whoever holds it. Mint one per joiner, hand it over
a private channel, and let it expire.

## What to copy

For **every** host, off-host and encrypted:

```sh
W=$DUCKTAPE_HOME/<chain-id>
tar czf - \
  "$W/identity.key" "$W/node.toml" "$W/network.toml" \
  "$W/services.toml" "$W/work-admit.toml" "$W/coord.cap" \
  "$W/keys" 2>/dev/null \
| age -r <your-recipient> > ducktape-$(hostname)-$(date +%F).tar.gz.age
```

(`tar` skips absent files; `age` is one option, any encryption you already
trust is fine. The archive holds a seat key and your wallets: treat it like
the keys themselves.)

Copy `identity.key` **before** the node is promoted to a validator seat,
not after: promotion is the moment losing it stops being cheap.

`storage/` is deliberately not in the archive. It is large, changes every
second, and a resident rebuilds it by syncing. If you want a cold copy of a
validator's `storage/` anyway, take it with the node **stopped**
(`systemctl stop ducktape-node@…`; SIGTERM checkpoints first) — a copy of a
running node's storage is not a consistent checkpoint.

## Restoring a resident (replica) — rehearsed shape

A resident holds no seat; its state is a cache of the network's. On a new
disk:

```sh
# 1. put the workspace files back (identity.key, node.toml, network.toml)
# 2. seed its first release and start it under the launcher (or enable its
#    unit, node-service.md) — the launcher pins the network's release key again
W=<workspace>
ducktape-node-launcher install --workspace "$W" --config "$W/node.toml" \
    --from "$(readlink -f "$(command -v ducktape)")"   # the file, never a link: install refuses one
ducktape-node-launcher run --workspace "$W" --config "$W/node.toml"
```

With `identity.key` restored the node comes back under its old key, keeps
its resident standing, and re-syncs `storage/` from a current validator —
the replica loop does this on its own whenever its local suffix is older
than the validators' retained one (`bin/node/src/replica/park.rs`,
"re-syncing at a fresh boundary"). A restarted joiner on the 4-CT LXC lane
logged `synced`, not `recovered`, after coming back with its files intact.

**Untested:** the same restore with an *empty* `storage/` and an intact
`identity.key`. Expected: a full checkpoint download; the code path is the
same re-sync. Not rehearsed on a live network.

Without `identity.key`, do not restore anything: ask a member for a fresh
invite, `ducktape node join <blob>`, and the new key gets its own standing.

## Restoring a validator — NOT rehearsed

A validator's `identity.key` is the seat. The honest options, in order:

1. **You have the key and the disk (stopped copy of `storage/`).** Put both
   back and start. The node recovers from its last checkpoint and catches up
   the suffix from its peers. This is the ordinary restart path, exercised on
   every graceful stop; a restore of the *copied* directory onto a new host
   has **not** been rehearsed.
2. **You have the key, not the disk.** Put `identity.key`, `node.toml` and
   `network.toml` back, start with an empty `storage/`. Expected: the node
   boots as its old key, is still in the set, and must state-sync the whole
   checkpoint before it can vote. While it syncs it is an absent seat: on
   n≤3 the chain is halted **until the sync completes**, and the sync source
   must be a validator that is itself serving. **Untested**; the danger is a
   node that believes it is a validator and votes on a stale state — the
   replica-side seal check (`bin/node/src/sync/catchup.rs`) refuses a served
   seal that disagrees with local replay, which is the guard that should
   catch it. Rehearse this on the LXC lane before relying on it.
3. **You do not have the key.** The seat is dead. If quorum still holds
   without it (n≥4), vote it out: `ducktape node member remove <pubkey>`
   from a live member, then admit the replacement through a fresh invite and
   `member promote`. If quorum does **not** hold (n≤3), no governance op can
   finalize; the network is halted for good short of a restart from genesis
   with a new descriptor. This is the case the backup exists to prevent.

Do not promote the third validator of a three-set without an off-host copy
of all three `identity.key`s, and prefer four seats on four
failure-independent hosts — the first configuration that survives one host.

## Rotating a key

There is no rotate verb and no in-place key rotation for a **node** key:
the key *is* the seat. Rotating a validator means admitting a new node
(fresh invite → `node join` → `member promote`) and then removing the old
seat (`member remove`), in that order, so quorum never drops below the
floor. Rotating a **user** key is `ducktape account key add` from an
existing key followed by removing the old one — see `ducktape account
--help`.

## Halt recovery after a bad module swap, binary rollback

Both are governance-driven, not file-driven: a module swap is proposed,
voted and executed on chain (`ducktape module --help`); rolling a
**binary** back is reinstalling the previous build on every host and
restarting (`node-service.md`, "Restart, stop, upgrade") — the chain has no
binary version and admits nothing on it. Neither has a rehearsed
*recovery* procedure for the case where the swap halted the network; the
live-upgrade e2e (`bin/node/tests/module_upgrade_e2e.rs`) covers a
registry refusal rolling `Execute` back in-kernel, not an operator
undoing a finalized swap.
