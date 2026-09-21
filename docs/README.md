# Documentation index

One line per document, grouped by the question it answers, so an agent or an
operator loads the one file that answers it instead of the tree. The rule for
what lives here is `AGENTS.md` § "Docs Are Not a Record": `docs/` holds what
an operator executes and the few references code cites by path. A document
nothing cites is deleted, not archived.

## Start here

| Question | Read |
| --- | --- |
| What is this, how is the tree laid out, how do I build, test and run it | [`../README.md`](../README.md) |
| What rules bind an assistant working in this repo | [`../AGENTS.md`](../AGENTS.md) |

## Operate

| Question | Read |
| --- | --- |
| Bring the microVM sandbox up on macOS (the vz shim) | [`sandbox-macos.md`](sandbox-macos.md) |
| Which operator scripts and harnesses live under `ops/` | [`../ops/README.md`](../ops/README.md) |
| The coordinator's deploy artifacts (unit, env file, Dockerfile) | [`../ops/coordinator/README.md`](../ops/coordinator/README.md) |
| Lend a credential to a sandbox through airlock, self-hosted or from an enclave | [`../crates/airlock/README.md`](../crates/airlock/README.md) |

## References code cites by path

| Question | Read | Cited by |
| --- | --- | --- |
| The capability spec TOML that describes an executor | [`records/specs/capability-spec.md`](records/specs/capability-spec.md) | `crates/services/provider` |
| The WireGuard tunnel upgrade protocol: records, mesh version, handshake, overlay addressing | [`records/protocols/wireguard-tunnel-upgrade.md`](records/protocols/wireguard-tunnel-upgrade.md) | `crates/networking/wireguard` |
| The reachability plane: control mesh beside data tunnel, the tunnel-first invite and its fronts, cold restart, rendezvous | [`records/architecture/reachability.md`](records/architecture/reachability.md) | `crates/networking/reachability` |

`docs/superpowers/` is gitignored planning scratch; nothing under it ships.
