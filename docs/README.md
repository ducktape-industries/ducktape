# Documentation index

One line per document, grouped by the question it answers, so an agent or an
operator loads the one file that answers it instead of the tree. The rule for
what lives here is `AGENTS.md` § "Docs Are Not a Record": `docs/` holds what
an operator executes and the few references code or a skill cites by path.
Operational references describe implemented behavior. Explicitly labeled protocol
proposals are listed separately for review and do not imply shipped support. A
document nothing cites is deleted, not archived.

## Start here

| Question | Read |
| --- | --- |
| What is this, how is the tree laid out, how do I build, test and run it | [`../README.md`](../README.md) |
| What rules bind an assistant working in this repo | [`../AGENTS.md`](../AGENTS.md) |

## Operate a node

| Question | Read |
| --- | --- |
| Keep a node and its service daemons up under systemd (Linux) or launchd (macOS); ports; logs; why three validators tolerate nothing | [`deploy/node-service.md`](deploy/node-service.md) |
| Install isolated application executables behind authenticated Gateway HTTP/WebSocket routes; stop, restart, and replace them | [`deploy/application-service.md`](deploy/application-service.md) |
| Clone, push and depend on a Forge repository by its `duck://` address (`ducktape forge setup`, `git-remote-duck`, Cargo); mirror GitHub into Forge on a timer | [`deploy/forge-git.md`](deploy/forge-git.md) |
| Which files are secrets, which are irreplaceable, what to copy, what a restore looks like | [`deploy/backup-and-keys.md`](deploy/backup-and-keys.md) |
| Run the untrusted coordinator (rendezvous + first-contact relay); stand up two NAT'd validators | [`deploy/coordinator.md`](deploy/coordinator.md) |
| Front a validator with a sentry so it exposes no inbound port | [`deploy/sentry-deployment.md`](deploy/sentry-deployment.md) |
| Replace a network: stop it, archive its workspaces, found a new one and re-import its forge | [`refound-a-network.md`](refound-a-network.md) |
| Move a live network onto a new node binary: pin, publish, designate, and watch every node flip | [`roll-a-node-release.md`](roll-a-node-release.md) |
| Run the dogfooding loop: this repo in its own forge, an agent working it | [`dogfood.md`](dogfood.md) |
| Install and operate the shared resident Chief conversation and its Pages workspace | [`../agents/chief/README.md`](../agents/chief/README.md) |
| Bring the microVM sandbox up on macOS (the vz shim) | [`sandbox-macos.md`](sandbox-macos.md) |
| Which operator scripts, units and harnesses live under `ops/` | [`../ops/README.md`](../ops/README.md) |
| The coordinator's deploy artifacts (unit, env file, Dockerfile) | [`../ops/coordinator/README.md`](../ops/coordinator/README.md) |
| The hosted WebAuthn auth page, its request/result shapes and its relay | [`../ops/auth-page/README.md`](../ops/auth-page/README.md) |
| Lend a credential to a sandbox through airlock, self-hosted or from an enclave | [`../crates/airlock/README.md`](../crates/airlock/README.md) |

## References code cites by path

| Question | Read | Cited by |
| --- | --- | --- |
| The capability spec TOML that describes an executor | [`records/specs/capability-spec.md`](records/specs/capability-spec.md) | `crates/services/provider` |
| The per-module index guest contract: fold rules, view rules, backfill | [`records/specs/indexable-spec.md`](records/specs/indexable-spec.md) | `crates/kernel/indexer` |
| The WireGuard tunnel upgrade protocol: records, mesh version, handshake, overlay addressing | [`records/protocols/wireguard-tunnel-upgrade.md`](records/protocols/wireguard-tunnel-upgrade.md) | `crates/networking/wireguard` |
| The reachability plane: control mesh beside data tunnel, the tunnel-first invite and its fronts, cold restart, rendezvous | [`records/architecture/reachability.md`](records/architecture/reachability.md) | `crates/networking/reachability` |
| The ordering contract agents get and the module architecture that keeps it | [`records/architecture/agent-collaboration-design.md`](records/architecture/agent-collaboration-design.md) | `saga` |
| Writing, building and live-updating a wasm module | [`records/architecture/wasm-module-authoring.md`](records/architecture/wasm-module-authoring.md) | `crates/kernel/wasm-host` (ducktape-sdk) |

## Proposed protocols

| Question | Read |
| --- | --- |
| How should Claude and Codex sessions collaborate across devices, with explicit delivery and task ownership | [`records/specs/agent-messaging.md`](records/specs/agent-messaging.md) |

## Agent runbooks (`skills/`)

| When | Skill |
| --- | --- |
| Verifying a running node or a cluster | [`../skills/qa/SKILL.md`](../skills/qa/SKILL.md) |
| A Rust test needs a deterministic in-process node | [`../skills/sim-lane/SKILL.md`](../skills/sim-lane/SKILL.md) |

`docs/superpowers/` is gitignored planning scratch; nothing under it ships.
