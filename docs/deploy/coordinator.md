# Coordinator (`relay.ducktape.industries`)

How to run `bin/coordinator`: an **untrusted, non-validator reachability
helper** that lets two NAT'd members find each other and hole-punch a direct
WireGuard path, plus a TCP lane that relays exactly one thing — a joiner's
sealed first-contact intro — when every UDP path to an inviter is exhausted.
It never carries the established overlay or peer data. The node-side plane it
serves is `docs/records/architecture/reachability.md`; the deploy artifacts
are `ops/coordinator/`.

## What it is

`coordinator --listen <addr> --relay-listen <addr|none> --workers <1|4>`
(defaults: UDP `0.0.0.0:3478`, TCP `0.0.0.0:443`, one worker). It is
stateless. The UDP control socket provides two services:

- **Rendezvous** — peers `register` their key and `lookup` each other; a
  `Lookup` fans a `PunchSync` to both sides so they simultaneous-open.
- **STUN reflexive** — answers a `BindRequest` with the peer's observed
  public `ip:port` so it can learn its own NAT-mapped address.

The TCP listener is a bounded fallback for the sealed first-contact intro
only. It resolves a member through the same live advert book, forwards one
opaque datagram, and returns the member's opaque reply. It is not a
WireGuard-over-TCP or general peer-data relay. Keep it on: a joiner assumes it
at `<coordinator host>:443` and is never told otherwise, so `--relay-listen
none` is a deliberate "members behind a non-punching NAT cannot join through
this coordinator" decision, not a footprint trim.

Registrations have a lifetime: a `register`/`readvertise` mapping expires
`REGISTRATION_TTL_SECS` after the last accepted advert; an expired key
resolves to `None` and receives no `PunchSync` (its NAT pinhole is long dead
anyway). Live nodes hold their mapping with a keepalive `Readvertise` every
`RENDEZVOUS_KEEPALIVE`, which doubles as the NAT-pinhole keepalive. The book
heals itself across coordinator restarts — the same keepalives re-register
everyone within one interval (their nonces are wall-clock-seeded, so a
rebooted node supersedes its own stale mapping instead of being rejected as a
replay).

Everything it answers derives from the **observed source** of the datagram, so
a wildcard `--listen 0.0.0.0:3478` bind is fully functional on a single-IP
host. **Multi-homed caveat:** on a box with more than one routable IP, bind the
concrete public IP peers dial. Replies from a wildcard-bound UDP socket egress
with the kernel's route-chosen source address, and `NatClient` (correctly)
discards any reply that does not come from the exact address it dialed — so a
coordinator answering from the "wrong" IP looks healthy while every client
times out.

No disk. No secret. On bind it prints `coordinator listening on <addr>` and,
when enabled, `coordinator relay listening on tcp/<addr>` to stderr, then
serves.

## Why it is safe to run untrusted

The two planes this coordinator sits beside are both authenticated and
end-to-end encrypted **without** it:

- The **control mesh** is commonware `authenticated::discovery`: a dialer dials
  an address and expects a specific `ed25519` public key; the handshake
  authenticates that key regardless of what network path delivered the bytes
  (the same property that makes a [sentry](sentry-deployment.md) safe).
- The **data tunnel** is member↔member **WireGuard**, keyed and encrypted
  between the two members' own keys.

What a compromised or malicious coordinator **cannot** do is the load-bearing
guarantee: it **holds no key, and cannot decrypt, impersonate, forge, serve
state, or join consensus**. Therefore it is safe to run as **throwaway infra
with no key on the box**. The hardening in
`ops/coordinator/ducktape-coordinator.service` makes that structural: if a
reviewer can find a place a secret *would* live on this host, the recipe is
wrong.

> **What it CAN do — read this before trusting a third party's coordinator.**
> The coordinator is untrusted for *confidentiality and authenticity*, but it is
> **not** out of the data path in every case. It brokers the punched endpoint
> each side installs, and the underlay endpoint is **not** cryptographically
> bound to the peer's identity — only the peer's WireGuard *public key* is
> pinned. So a malicious coordinator can answer a lookup with **its own**
> address, punch from it, and become the **underlay relay** between two peers
> that have no direct path. Riding that relay it can **observe traffic volume
> and timing** and **censor** (drop) an established tunnel — but it still cannot
> read, forge, or alter the WireGuard-encrypted payload. This is **inherent** to
> rendezvous-based NAT traversal: for a peer pair with no direct route, the
> party that tells each side where the other *is* must be trusted not to name
> itself. A signed punch would not close it (the coordinator can relay the
> peer's own valid packets); only a directly-reachable signed endpoint lets a
> peer bypass the coordinator entirely.
>
> **Operational guidance:** run coordinators **you** trust (or your own), and run
> **several** — a peer that can reach a direct signed endpoint never installs a
> coordinator-punched override, and multiple coordinators dilute any single
> one's leverage. Do not treat a stranger's coordinator as neutral: it cannot
> steal your keys or your data, but it can watch and throttle the connections
> that depend on it.

## Auth modes

The coordinator is keyless in every mode:

- **Default public mode** — no auth flag. Requests must carry proof of
  possession for the node key they claim.
- **Private mode** — `--genesis-set <network.toml>`. The coordinator reads only
  the public `validators = [...]` keys from that descriptor. They are the
  floor: a genesis validator, and a node presenting an unexpired cap a genesis
  validator signed, are always admitted.
- **Private mode following the live set** — add
  `--valset-node http://<host>:<port>`. Every 10 s the coordinator reads that
  node's committed validator set off its open `POST /v1/query` lane (the
  valset module's `validators` query) and also admits those validators and the
  caps they sign. Every validator seat mints a cap for the joiner it admits,
  so a joiner seated by a validator promoted after genesis carries a cap only
  a coordinator following the live set admits. A reading admits for 60 s after
  it was taken. Once the node has not answered for that long, a key only that
  reading named is refused (`coordinator_request_refused`,
  `reason = "valset_stale"`) until a read succeeds again, while the genesis
  set keeps admitting.

Name a node of this network that you run: the coordinator believes the set it
reports, over plain HTTP, so reach it on the same host
(`http://127.0.0.1:8844`, the node's default `http_listen` port) or over a
private link. A container's `127.0.0.1` is not the host's; give a Docker
coordinator an address the node's `http_listen` actually serves. The read logs
`coordinator_valset_read` when the set first arrives, changes, or recovers,
and `coordinator_valset_read_failed` (with `reason`, `attempts`, and whether
the last reading has `lapsed`) on the first failure and every 30th after.

Malformed `--listen`, `--relay-listen`, `--workers`, `--metrics-interval`, and
malformed/value-less `--genesis-set` or `--valset-node` are hard errors, not
silent fallbacks to a weaker policy; `--valset-node` without `--genesis-set` is
refused.

## Authentication workers

`--workers 1` verifies inline with no worker threads. `--workers 4` runs only
the Ed25519 checks on four fixed 512 KiB-stack threads; UDP I/O and the single
rendezvous state machine remain ordered on the current-thread runtime. The
workers pull from one shared bounded queue, so an idle verifier can take the
next job instead of waiting behind a busy worker's round-robin queue. The work
queue, result queue, and ordered completion window are bounded, so overload
falls back to the kernel's bounded UDP queue/drop behavior instead of growing
process memory.

The systemd and container recipes select `--workers 4` and set
`MALLOC_ARENA_MAX=1` to avoid glibc reserving a large virtual arena per worker.
Use `--workers 1` on a single-vCPU or minimum-footprint host.

## Logs and metrics

The bind announcements and the metrics rows are bare parseable lines on
stderr; everything else is `tracing` on the `ducktape::reachability` plane
with a `reason` on every refusal, relay session, advert eviction and
unresolved lookup. `RUST_LOG` ADDS to the `info` floor, so
`RUST_LOG=ducktape::reachability=debug` turns the plane up without turning
anything else off.

The server emits one parseable `coordinator_metrics` line every
`--metrics-interval` seconds (default 10; `0` disables it). Counters are
cumulative. `cpu_pct` is process CPU across all coordinator threads, so four
fully busy auth workers can approach 400%. `rss_mib` and the
`inflight`/`inflight_max`/`saturated` fields make bounded overload and recovery
visible without opening a metrics socket, and every row carries
`relay=on|off`.

### Cross-host load, flood, and soak

Build both executables, put `coordinator` on the server host and
`coordinator-load` (`bin/coordinator/src/bin/coordinator-load.rs`) on a
different host, then run the baseline:

```sh
cargo build --release -p coordinator-bin --bins

# Server host. With systemd, set the same arguments in COORDINATOR_ARGS and use
# journalctl -fu ducktape-coordinator for the metrics rows.
MALLOC_ARENA_MAX=1 ./coordinator --listen 0.0.0.0:3478 --workers 4 \
  --metrics-interval 1

# Load host (a distinct machine/VM): client-local RTT needs no clock sync.
# Sweep concurrency until coordinator CPU or drop rate reaches the desired load.
for clients in 16 64 256; do
  ./coordinator-load --target SERVER_IP:3478 --duration 60 \
    --clients "$clients" --report-interval 0
done
```

The default output is a compact comparison table with end-to-end p99, drops,
request/flood rates, and load-host CPU; use `--output log` for stable
`key=value` ingestion. The matching grouped server metrics rows supply
coordinator CPU/RSS and show whether the bounded window saturated. Each valid
client reuses one signed, response-correlated request, refreshes its timestamp
periodically, and rotates the correlation key after a timeout, so late replies
stay out of later latency samples without making load-host signing throughput
the benchmark's ceiling. Latency uses fixed 0.1 ms buckets up to the
configured timeout, so a 24-hour run keeps constant memory.

An invalid-signature flood while retaining a valid probe, followed by a
valid-only recovery phase:

```sh
./coordinator-load --target SERVER_IP:3478 --duration 60 --clients 1 \
  --invalid-clients 16 --timeout-ms 1000 --recovery 10 --report-interval 10
```

Invalid packets use a fresh claimed public key but a different signing key and
refresh their timestamp, so they exercise Ed25519 rejection rather than the
cheaper stale-request check. Recovery passes only if at least one valid
response arrives after the flood stops; on the server, `rejected` rises during
the flood and `inflight` returns to zero afterward.

The same bounded-memory probe is the 24-hour soak:

```sh
./coordinator-load --target SERVER_IP:3478 --duration 86400 --clients 16 \
  --rate 1000 --timeout-ms 1000 --report-interval 60 | tee coordinator-soak.log
```

Keep the corresponding server metrics log: a useful soak artifact contains
minute p99/drop rows plus CPU, RSS, saturation, send-error, and recovery
evidence from the server. Omit `--rate` only when the soak host can sustain an
unlimited run without disturbing colocated services. Do not add `NodeKey`
sharding unless this cross-host profile shows the ordered state actor — not
signature verification or UDP drops — has become the measured bottleneck.

## Deploy A — systemd (bare VPS)

```sh
# 1. Build the binary (repo root).
cargo build --release -p coordinator-bin

# 2. Install it.
sudo install -m 0755 target/release/coordinator /usr/local/bin/ducktape-coordinator

# 3. Install the env file and the hardened unit.
sudo install -D -m 0644 ops/coordinator/coordinator.env.example /etc/ducktape/coordinator.env
sudo cp ops/coordinator/ducktape-coordinator.service /etc/systemd/system/

# Optional: edit /etc/ducktape/coordinator.env to choose a bind address and auth
# mode. The supplied file binds the TCP relay lane on 0.0.0.0:443, selects four
# auth workers and public proof-of-possession. For private mode, following the
# validator set of a node on this host, use:
# COORDINATOR_ARGS=--relay-listen 0.0.0.0:443 --workers 4 --metrics-interval 10 --genesis-set /etc/ducktape/network.toml --valset-node http://127.0.0.1:8844

# 4. Start it.
sudo systemctl daemon-reload
sudo systemctl enable --now ducktape-coordinator

# 5. Verify — BOTH lanes.
systemctl status ducktape-coordinator
ss -lunp 'sport = :3478'     # the fixed control socket, owned by the dynamic user
ss -ltnp 'sport = :443'      # the relay lane; absent = the bind failed
journalctl -u ducktape-coordinator | grep -E 'relay listening|relay_lane_disabled'
journalctl -u ducktape-coordinator | grep coordinator_metrics | tail -1   # carries relay=on|off
```

The unit runs under `DynamicUser=yes` (an ephemeral throwaway user — no home,
no shell, nothing to compromise), with **exactly one** capability —
`CapabilityBoundingSet=CAP_NET_BIND_SERVICE` and
`AmbientCapabilities=CAP_NET_BIND_SERVICE`, so the relay lane can bind 443
(port 3478 needs none) — `NoNewPrivileges=yes`, `ProtectSystem=strict` +
`ReadOnlyPaths=/` (no writable path at all — the coordinator keeps no state),
and `RestrictAddressFamilies=AF_INET AF_INET6` (Internet IP sockets only; no
unix/raw/packet sockets). This posture is deliberate: **there is no secret to
steal and no state to corrupt**, so the box can be treated as disposable and
replaced at will.

A relay bind failure does **not** stop the unit: the coordinator keeps
serving UDP, logs `relay_lane_disabled` at ERROR on boot (with the fix in the
message), and every `coordinator_metrics` row thereafter says `relay=off`.
Joiners derive their fallback as `<this host>:443` regardless, so treat
`relay=off` on a public coordinator as an outage for every member behind a NAT
that cannot hole-punch.

## Deploy B — Docker / OCI

```sh
docker build -f ops/coordinator/Dockerfile -t ducktape-coordinator .

# Harden the container to match the systemd unit. The relay lane binds an
# unprivileged port inside the container and the host maps TCP 443 onto it,
# so the non-root container needs no capability at all.
docker run \
  --cap-drop=ALL \
  --security-opt no-new-privileges \
  --read-only \
  --restart unless-stopped \
  -p 3478:3478/udp \
  -p 443:8443/tcp \
  ducktape-coordinator --listen 0.0.0.0:3478 --relay-listen 0.0.0.0:8443 --workers 4
```

For private mode in Docker, append the auth args after the image name:

```sh
docker run --cap-drop=ALL --security-opt no-new-privileges --read-only \
  -p 3478:3478/udp -p 443:8443/tcp \
  -v /etc/ducktape/network.toml:/etc/ducktape/network.toml:ro \
  ducktape-coordinator --listen 0.0.0.0:3478 --relay-listen 0.0.0.0:8443 --workers 4 \
    --genesis-set /etc/ducktape/network.toml --valset-node http://<node host>:8844
```

The image is multi-stage: a `rust:1.96-bookworm` build stage compiles exactly
`-p coordinator-bin`, and the runtime stage is
`gcr.io/distroless/cc-debian12:nonroot` — glibc + libgcc for the
dynamically-linked binary, **no shell, no package manager**, running as the
built-in non-root uid `65532`. The `docker run` flags mirror the systemd
unit's posture and are not optional decoration:

- `--cap-drop=ALL` — the container needs **no** Linux capability. The unit
  holds `CAP_NET_BIND_SERVICE` for exactly one reason, binding the relay lane
  on 443; in Docker the relay binds unprivileged 8443 and the host's
  `443:8443` map exposes the standard port, so even that one is dropped.
- `--security-opt no-new-privileges` — mirrors `NoNewPrivileges=yes`.
- `--read-only` — the coordinator keeps **no** state, so its root filesystem can
  be immutable, mirroring `ProtectSystem=strict` + `ReadOnlyPaths=/`.
- `--restart unless-stopped` — the production restart policy; the disposable box
  comes back after a crash or reboot. Drop it (and add `--rm`) only for a
  throwaway smoke run.

Publish exactly the two ports (not `--network host`). The `-p 443:8443/tcp`
map is load-bearing: a joiner derives the relay as `<coordinator host>:443`
and is never told otherwise, so a container without it (or run with
`--relay-listen none`) is the `relay=off` outage the systemd section describes
— every member behind a non-punching NAT is locked out.

## DNS + firewall

- Point an `A` record `relay.ducktape.industries` → the VPS IP.
- Open **inbound UDP 3478** and **inbound TCP 443** (the relay lane, on by
  default; with a reverse proxy or DNAT, the port it forwards to).

## Redundancy — the coordinator is not load-bearing

Run **multiple** coordinators. An invite carries a `Vec` of reach hints and
`NatClient::discover_reflexive_failover` walks them, so a single coordinator
outage is not fatal to entry. A tunnel punched to a peer's **own** address (a
direct signed endpoint, or a reflexive that stayed valid) survives a
coordinator restart entirely — only *new* rendezvous depends on a live
coordinator. The one caveat is the relay case above: if a **malicious**
coordinator made itself a peer's underlay endpoint, that tunnel rides *it*, so
that tunnel does depend on it (and it can drop it) — which is exactly why you
run coordinators you trust and prefer direct signed endpoints. This is still a
sharp contrast with an in-path [sentry](sentry-deployment.md), which is
*always* in the data path and a single point of failure for the validator it
fronts.

## Two NAT'd validators — the zero-exposure procedure

Two validators behind **real, distinct NATs**, neither exposing an inbound
port, plus this coordinator on a public VPS:

```
   Validator A ──┐                          ┌── Validator B
   (NAT, no      │      Coordinator         │   (NAT, no
    inbound)     └────▶ relay.ducktape.industries ◀──────┘    inbound)
                        (public VPS)
     A and B dial OUT to the coordinator, then hole-punch a direct
     A<->B WireGuard tunnel and drop the coordinator out of the path.
```

1. **Deploy the coordinator** per Deploy A or B; confirm it binds UDP `:3478`
   (`ss -lunp 'sport = :3478'`) and answers a live `BindRequest`
   (`bin/coordinator/tests/deploy_smoke.rs` is the same check as a test).
2. **A founds the network**: `ducktape node init --name <net>` and
   `ducktape node run`. Each node reaches the coordinator from its own
   workspace configuration (`primary_coordinator_or_default`, the public
   coordinator by default); no invite ever carries a coordinator address.
3. **A mints an invite**: `ducktape node invite`. The signed, single-use blob
   is the admission decision and the VPN credential in one; what it bundles
   is in `docs/records/architecture/reachability.md`.
4. **B joins**: `ducktape node join <blob>` then `ducktape node run`. B races
   first contact across every path the blob offers — the inviter's own
   endpoint and its reachable members, directly where an endpoint is known
   and through this coordinator where one is not — and redeems its standing
   on the first path that answers. To seat B in the quorum a member runs
   `ducktape node member promote <B-pubkey>`; admission and reachability are
   independent decisions.
5. **State sync and consensus ride the punched tunnel.** Whether the tunnel
   comes up depends on the two NATs admitting a direct punched path: the
   tunnel has no relay fallback, so a pair that cannot punch fails honestly
   instead of routing peer traffic through the coordinator. A symmetric ↔
   symmetric pair with no routable endpoint needs a different entry path (a
   forwarded UDP port on either side suffices).

The library-level proof of the punch is
`crates/networking/reachability/tests/rendezvous_simnat.rs`, which drives the
production resolver over a simulated NAT topology.

## What this recipe does NOT do

This page does not promise universal zero-exposure connectivity. The TCP/443
lane relays exactly one thing — the joiner's sealed first-contact intro — so a
member behind a NAT that cannot punch can *redeem an invite* through it and
then has no tunnel: no statesync, no huddle, no gateway, no agent telemetry.
Every per-use plane rides the WireGuard overlay, and the overlay has no relay
by design.
