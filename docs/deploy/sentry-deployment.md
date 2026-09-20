# Sentry Deployment Recipes

How to front a Ducktape validator with a **sentry** so it never exposes a
public inbound port, while joiners still enter, sync, and promote through it.
A sentry is configuration only; the regression that proves a joiner enters
through one is `bin/node/tests/sentry_e2e.rs`.

## Why this is safe

The mesh transport (commonware `authenticated::discovery`) is **encrypted and
key-authenticated end-to-end**. A dialer dials an address and expects a specific
`ed25519` public key; the handshake authenticates that key regardless of what
network path delivered the bytes. Therefore any box on the path is at most a
transparent ciphertext forwarder: it can drop or delay, but it **cannot read
content and cannot impersonate a peer**. A sentry is not a mesh member, is not a
validator, and never serves state — it is pure path.

Concretely, a bootstrap hint is `pubkey@addr` and the validator's `advertised`
address is fully independent of its real `listen` address
(`bin/node/src/config/resolve.rs`, `resolve_advertised`). Fronting is
therefore *configuration only*: point `advertised` (and joiners' hints) at the
sentry, keep `listen` private. A sentry is addressed through a plain `direct`
reach hint naming its public address.

## (A) Forward sentry (Cosmos-style)

The sentry listens on a **public** address and TCP-splices to the validator's
**private** `listen` address. Requires sentry→validator reachability (shared
private network, VPC, or a firewall exception).

```
joiner ──▶ sentry(public :443) ──▶ validator(listen 10.0.0.7:8846, private)
             (transparent TCP splice; ciphertext only)
```

Validator config:

```toml
listen     = "10.0.0.7:8846"           # private; reachable only from the sentry
advertised = "sentry.example.com:443"  # what peers dial
```

Invites / bootstrap hints handed to joiners carry the **validator's key** at the
**sentry's** address:

```
<validator_pubkey_hex>@sentry.example.com:443
```

The joiner dials the sentry, the sentry splices to the validator, and the
encrypted mesh handshake terminates at the validator through the pipe — the
joiner never learns (and never needs) the validator's private `listen`.

Admission is orthogonal to the sentry and still required. `ducktape node invite`
mints a signed, single-use bearer grant; `ducktape node join <blob>` presents and
redeems it on first contact. A tokenless manual join can instead be granted with
`ducktape node resident accept <pubkey>`. Until standing lands, ordinary mesh
and state-sync traffic stays gated at the validator rather than at the sentry.
Once admitted, the handshake succeeds and state-sync flows through the pipe;
`ducktape node member promote <pubkey>` is the separate validator-seat step.

Realizations of the splice, cheapest first:

- **`nginx stream`** — `stream { server { listen 443; proxy_pass 10.0.0.7:8846; } }`
- **HAProxy TCP mode** — `mode tcp` frontend/backend.
- **A small Rust forwarder** — an accept loop that, per connection, dials the
  target and runs `std::io::copy` in both directions (`sentry_e2e.rs` stands
  up exactly such a forwarder).

Run multiple sentries per validator for redundancy — each is just another
`pubkey@sentry_addr` hint in the bootstrap set. Sentries rotate without touching
the validator's key.

## (B) Reverse tunnel

The validator dials **out** to an edge and the edge becomes its public face. No
inbound port on the validator at all — ideal when the validator is behind NAT
with no port-forward.

```
validator ──(dials out)──▶ edge(public :443) ◀── joiner
   listen private            the "sentry address" is the tunnel edge
```

Tooling: `frp`, `rathole`, `ssh -R`, or a cloudflared-style tunnel. The edge's
public `host:port` is what goes into `advertised` and into joiners' hints,
exactly as in recipe (A) — from the mesh's point of view the two recipes are
identical ("dial this address, expect this key").

## Caveats

- **`allow_private_ips` + `bypass_ip_check` coupling (fronting depends on
  both).** The node builds its mesh with the `local` lookup preset
  (`allow_private_ips: true`) and explicitly sets `bypass_ip_check: true`
  (`bin/node/src/boot/mesh.rs`, at the `lookup::Config::local` call; see the
  inline comment there). Both are load-bearing for fronting. Without
  `allow_private_ips`, the validator's listener would **reject any inbound
  whose observed source IP is not globally routable** — every fronting scheme
  that keeps `listen` private (a private-network forward splice, a reverse
  tunnel arriving over loopback). Without `bypass_ip_check`, lookup's own
  source-IP pinning would reject any inbound whose source IP differs from the
  peer's *tracked address* — which is exactly a fronted validator's situation
  (peers track the sentry's address; the validator dials out from its real
  IP), and a NAT'd member's too. With the bypass, admission is the
  cryptographic handshake plus key-in-a-tracked-set.

- **Handshake rate limit funnels through the sentry's IP.** commonware
  rate-limits inbound handshakes **per source IP** and per subnet (/24 for
  IPv4, /48 for IPv6), keyed on the accepted socket's IP. A **forward** splice
  (recipe A) makes the validator see every joiner as coming from the sentry's
  single IP, so all inbound handshakes share one IP's budget — under a
  reconnect storm or many simultaneous joiners a single sentry becomes a
  handshake bottleneck. The `local` preset the node uses allows 16 handshakes
  per second per IP; commonware's `recommended` preset allows about one per
  five seconds. Mitigate by running multiple sentries on **distinct IPs** (and
  distinct /24 subnets) — a second, load-distribution reason for the
  "multiple sentries" guidance above, beyond redundancy. (A transparent
  splicer does no filtering, and commonware has no PROXY-protocol parsing to
  recover the real client IP, so fronting *blinds* the validator's own per-IP
  defenses rather than adding them.)

- **DNS re-resolution, and the hint pin.** A DNS-named edge
  (`advertised = "sentry.example.com:443"`) stays a hostname in the address
  book and is **re-resolved at every dial** (`Ingress::Dns`; see
  `ingress_of` in `bin/node/src/config/`), so a moved A-record heals on the
  next dial without a restart. The mesh address book deliberately **pins a
  DNS hint against live reachability adverts** (the `dns_hint_pinned` reason
  in `bin/node/src/mesh_book.rs`): an advert would freeze the name to one
  stale resolution. The flip side: a fronted validator that moves behind a
  *new name* needs a descriptor edit — adverts will not retarget a DNS-hinted
  peer.

- **State-sync still terminates at a validator.** The sentry is pure path — it
  never serves state. A joiner's `--sync-only` sources are the descriptor's
  validator **keys** (`sync_sources` in `bin/node/src/boot/mesh.rs`, rotated
  by `bin/node/src/boot/sync_only.rs`), so only a validator — never the
  sentry, which has no key in the descriptor — can serve it. State-sync flows
  *through* the sentry pipe but *terminates at* the validator behind it.

- **Availability dependency — an in-path sentry is a SPOF, not just a
  new-connection concern.** A forward splice (recipe A) or reverse tunnel
  (recipe B) sits **in the data path**: the fronted validator advertises only
  the sentry and keeps its real `listen` private, so the sentry carries *all*
  of that validator's inbound mesh traffic for the process lifetime, not merely
  entry. When the sentry/edge restarts, every inbound connection transiting it
  drops (keepalive cannot preserve a session whose intermediary died); only
  sessions the validator itself dialed **out** survive. So a single-sentry
  outage **partitions that validator** from the mesh — and if it is
  quorum-critical (e.g. a 2-of-2 set) that is a **liveness** failure
  (finalization stalls), not merely degraded availability. Mitigate — as a
  liveness requirement, not an optional nicety — with **multiple independent
  sentries on distinct IPs** (the bootstrap set is a `Vec`) and/or a redundant
  advertised path; self-host; keep a direct fallback hint. An out-of-path
  [coordinator](coordinator.md) is where the "established connections
  survive; only new ones depend on it" framing holds.
