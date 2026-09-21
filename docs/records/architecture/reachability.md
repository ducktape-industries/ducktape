# The reachability plane

How a Ducktape node reaches its peers: the control mesh and the WireGuard data
tunnel it drives beside it, the invite that brings a joiner's tunnel up before
any TCP, and what heals from disk. The host half is
`crates/networking/reachability` (the executor, the rendezvous runtime, the
WireGuard keystore, sealed envelopes, the persisted-mesh store); the decision
core — the protocol state machine, per-epoch state, wire messages, derived
bindings, the persisted-mesh codec — is `crates/networking/netstack-machine`.
The signed record and handshake formats are
`../../records/protocols/wireguard-tunnel-upgrade.md`.

## 1. Two planes, composed orthogonally

- The **control mesh** is commonware `authenticated::discovery` over TCP,
  key-authenticated end to end. The reachability plane never changes it: it
  exchanges its own messages over one dedicated mesh channel and drives only
  the data tunnel.
- The **data tunnel** is member↔member WireGuard on a dedicated, chain-scoped
  `dt-*` interface: per-epoch record gossip → signed advertisements → mesh
  view verification → pairwise handshakes → one interface configuration per
  epoch. `NatResolver` (STUN, rendezvous and hole-punch through the
  coordinator; no relay — a failed punch is terminal) resolves each peer's
  UDP endpoint.

Coexistence with a personal Tailscale is load-bearing: an `fd::/48` ULA
overlay derived from the chain id, and per-peer AllowedIPs of exactly one
/128 — never a default route, never `100.64.0.0/10`. Everything derives from
public inputs: overlay addresses from `(chain_id, identity)`
(`ula_v6_member_addr`), epoch bindings from `(chain_id, epoch, members)` — no
allocator, no coordination, no consensus-state change.

## 2. Transitive gossip

Reachability-plane messages never assume pairwise transport: records and
adverts flood with nonce dedup, handshake messages fan to every peer and relay
through stage-superseding per-pair slots, and every message authenticates by
its owner's content signature (`SignedEndpointRecord`, the handshake triple) —
never by the delivering link. A joiner with a single ingress link assembles
tunnels with every member through that one link.

## 3. Mesh over the tunnels

`advertised = "overlay"` makes a node advertise its chain-derived overlay ULA
at its mesh listen port (requires `listen = "[::]:<port>"`). Discovery gossip
distributes it; dialers retry until the tunnel makes it routable. A
`coordinated` reach hint routes the WireGuard path through its coordinator;
the overlay advertisement is how the TCP mesh follows.

## 4. Membership: registration → online → activation

Validator membership is two-phase. Governance `Join` registers a key as
STANDBY: transport-tracked, statesync-served, quorum-exempt, arming a
transport-only cutover. The parked node probes its registration from the
valset snapshot, proves a full state sync, and announces itself online over
the lobby with its own height-windowed signature; any active member relays the
proof (re-verified in-module), and the activation cutover widens the quorum. A
registered-but-absent node costs consensus nothing.

## 5. The tunnel-first invite

The invite blob IS the VPN credential, and the join window's carrier is the
invite tunnel itself — there is no TCP ingress at all.

1. A member mints an invite: the signed blob carries the network
   descriptor, the member's WireGuard public key and underlay UDP endpoint,
   its UDP intro endpoint (`invite_listen`, default the WireGuard port + 1),
   its overlay mesh port, an expiry, and a single-use token — minting IS the
   admission decision.
2. Joining with the blob writes the workspace with WireGuard-shape
   defaults (own plane, dual-stack mesh listen, `advertised = "overlay"`, the
   inviter's overlay ULA as a direct dial hint) and the node starts: it
   installs the inviter as a join-window tunnel peer straight from the blob,
   announces its identity and WireGuard key to the intro listener (one
   token-authenticated datagram; the inviter installs the peer and acks with
   an `IntroAck`), and the tunnel comes up before any p2p.
3. The mesh dials the inviter's overlay ULA the moment the tunnel routes; the
   joiner's lobby announce rides it, and the receiving member submits the
   governance `Redeem` op automatically — no approval verb. The grant cutover
   re-tracks the mesh, the pre-warm layer (§7) assembles tunnels with every
   member, and the joiner syncs (rotating across every serving validator)
   into a serving full node.
4. Seating it in the quorum stays a separate, deliberate act
   (a member promotion) — the machinery of §4, unchanged.

### Fronts: every path the inviter offers

The blob bundles EVERY entry the inviter offers as ONE candidate set:
`{inviter} ∪ {fronts}`, where each front is a reachable member the inviter
already meshes with, read from its persisted `mesh-state.json` at mint. A
front carries only:

- `member_key` — the member's real ed25519 node identity (the joiner
  authenticates this end-to-end);
- `wireguard_public_key` — the member's **public** X25519 key;
- `mesh_port` — the member's overlay control port;
- `endpoint` — the member's routable WireGuard underlay endpoint when it is
  host-capable, else `None` (a punchable, NAT'd member reached by identity
  through the joiner's coordinator).

No WireGuard **private** key is ever transported. The joiner races the whole
union: a candidate with an endpoint is dialed directly and announced at its
intro port; an endpoint-less one is driven through
`BootstrapCoordinatedInvitePeer` (rendezvous → install → intro over the
punched socket). The first `IntroAck.installed` wins, the rest are cancelled,
and exhaustion is an honest terminal: a distinct non-zero exit with a
mode-naming fatal, never a silent success. So a fully-NATed (including
symmetric) inviter still onboards as long as ONE offered path — itself via its
coordinator, or any reachable front — comes up.

The coordinator is AMBIENT: the joiner's resolver binds
`primary_coordinator_or_default` (the public default), never an address baked
into the blob; the invite embeds no coordinator address at all. Fronts live
OUTSIDE the genesis fingerprint (advisory reachability, not validator
identity — pinned by a fingerprint-exclusion test), so they never affect
consensus identity or which network a blob admits to. A first-boot inviter
with no persisted mesh (or one that holds only itself) mints an invite with no
fronts and prints a warning; the join still works over the inviter's own
paths — re-mint once the mesh has peers to widen the race.

Because the blob names the inviter's reachable members (identities, WireGuard
public keys, overlay ports, and public endpoints for host-capable ones), a
leaked invite widens who sees that data from admitted members to whoever
holds the blob. Invites are single-use and expire; treat one like the secret
it is.

Bounds by design: a DIRECT candidate's intro listener is the peer's own UDP
(`wg_port + 1`), so a direct path needs that member's port
underlay-reachable (one forwarded UDP port suffices; the joiner needs
nothing), while a coordinated path needs no forwarded port at all.

## 6. Cold restart

A member restarting with zero TCP links (ingress gone, tunnels torn down at
exit) — and the whole-network cold start — heals from disk. Every applied
epoch persists its verified advert set (`<storage>/mesh-state.json`,
signature-verified on load); the first boot retarget re-applies that mesh
purely locally — peer WireGuard keys and advertised endpoints from the
persisted records, overlay ULAs re-derived from `(chain_id, identity)`, NATed
endpoints re-resolved FRESH through the coordinator (`NatResolver` needs no
gossip; one-sided resolution suffices because WireGuard roams on
authenticated inbound) — and the dialer is seeded with the persisted control
ULAs for peers without a configured hint. The restored mesh is purely a
gossip carrier: the boot epoch's own live assembly replaces it at its apply.
What it deliberately does NOT cover is the FIRST join (nothing persisted
yet) — that is the invite tunnel's job (§5).

## 7. Standby tunnel pre-warming

The plane's epochs version ACTIVE members only — a registered-but-absent key
must never stall an epoch — but the resident tier rides a separate PRE-WARM
layer with the opposite trade: never versioned, never handshaked, applied
live. Every retarget carries the epoch's resident set as standbys; a
standby's owner-signed endpoint record (bound to the epoch tuple,
policy-checked, nonce-superseded) installs a tunnel by re-applying the full
interface config in place — the same record-derived trust model as the
cold-restart restore — and a higher-nonce re-advertisement moves its endpoint
live. The parked joiner runs the plane in the standby role off its manifest
polls (its gossip is admitted under the derived lobby identity; replies route
back over the delivering transport key), installs every member on its own
interface, and persists the member adverts — so the promotion reboot restores
full connectivity from disk before the new epoch's assembly replaces it with
the verified mesh. A joiner's tunnels exist BEFORE its activation cutover, not
seconds after.

## 8. Rendezvous

The rendezvous plane holds up over real time and real NATs three ways.
Coordinator registrations EXPIRE (`REGISTRATION_TTL_SECS`): an expired key
resolves to an honest `None` and receives no `PunchSync` toward its dead
pinhole, and the nonce anti-rollback guard yields for corpses so a rebooted
node re-registers cleanly. Nodes hold their mapping with a keepalive
`Readvertise` every `RENDEZVOUS_KEEPALIVE` from the resolver's pump task —
the same datagram keeps the NAT pinhole open, and its wall-clock-seeded nonce
supersedes the node's own pre-reboot mapping. And the punch does not need both
sides to resolve simultaneously: the pump answers coordinator-vouched
`PunchSync` fan-outs while the node is otherwise idle, and the active side
re-`Lookup`s on every punch retry (each one re-fans the sync), so one-sided
resolution completes against an idle-but-alive peer.

Every rendezvous request carries proof of possession of the node key it
claims (`crates/networking/nat-traversal/src/auth.rs`); in private mode the
coordinator additionally admits only keys rooted in the network's genesis
set. A compromised coordinator can deny service and, for a pair with no
direct path, make itself their underlay relay — but it can never substitute
keys: records pin WireGuard keys under the owner's ed25519 signature, and the
tunnel payload stays end-to-end encrypted. There is no data relay in the
product, and this design needs none.

## 9. Proofs

- `crates/networking/reachability/tests/rendezvous_simnat.rs` — the
  production resolver punching over a simulated NAT topology.
- `crates/networking/wireguard/tests/tunnel_e2e.rs` — the fixed
  mesh-version vector every node must reproduce.
