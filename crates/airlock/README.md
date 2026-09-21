# airlock

The barrier between **execution** and **auth**. An agent sandbox runs on one
side; the OAuth/API credential lives on the other. Nothing crosses without an
attested, session-scoped handshake — so the sandbox calls the Claude/Codex API
using a credential it never holds, and the operator of the credential side
cannot read the credential out of it.

## Crates

- **`airlock`** (this crate) — the pure attestation + sealing + session-key
  handshake core (`attest`, `seal`, `handshake`, `token`, `wire`, `aead`), plus
  the async client (`client::Gateway`) behind the `client` feature. Off-consensus;
  uses RustCrypto primitives directly.
- **`airlock-service`** (`crates/services/airlock`) — the LENDER half without a
  TEE: the disk-backed credential store a node co-hosts (`user cred add` writes
  it) plus the gateway router that serves it. Run as
  `ducktape service run airlock`, a standalone daemon beside the node. Its trust
  anchor is the seal PUBLIC key on consensus, which the borrower pins — no quote.
- **`airlock-gateway`** (`bin/`) — the ENCLAVE lender, and nothing else. Runs
  inside an Intel TDX / AMD SEV-SNP confidential VM. Holds a sealed refresh token
  in enclave memory, attests, proxies `/v1/*`, issues scoped session tokens. It
  stays a separate, minimal binary because the measurement covers every byte in
  it: folding `ducktape` in would churn the pinned measurement on every unrelated
  release.
- **`broker-host`** (`crates/services/broker`) — the Computation Provider's local
  api-snatch. With a `--features verify` build it verifies the gateway quote and
  handshakes once; without it, an `Attested` trust config is refused by name
  rather than silently trusting an unverified gateway. Either way it exposes a
  loopback `ANTHROPIC_BASE_URL` for an unmodified sandbox (the real `claude`
  CLI). The sandbox holds only an opaque per-run bearer.
- **`ducktape user cred`** (`bin/node`) — the operator verbs. `add`/`list`/
  `grant`/`revoke`/`remove` manage a self-hosted credential and its on-chain
  record; `inspect` (pin an enclave's measurement) and `seal` (verify the quote,
  then seal + upload the credential) are the enclave half.

## Pi harness

Pi can use either existing credential kind; it is not a third credential kind.
Install its standalone Linux bundle, then select Pi explicitly:

```sh
ducktape agent install pi -n <chain-id>
ducktape agent pty pi --account <account> --route <label> \
  --cred <credential-name> -n <chain-id>
```

`--account`/`--route` name the installed `ducktape-terminal` application that
serves the session (see `docs/deploy/application-service.md`); the node reaches
it through its signed gateway route and spawns no pty of its own. Interactive
Pi sessions are solo-only: shared command-driven sessions are refused because
Pi's tool allowlist does not restrict its interactive shell. The same `pi`
capability works for scheduled runs. A Claude credential selects
Pi's Anthropic provider; a Codex credential selects its OpenAI Codex provider.
Without `--cred`, Pi uses the host Anthropic credential path. Real credentials
stay behind the broker/airlock: the guest receives only a run-scoped capability,
a fresh `PI_CODING_AGENT_DIR`, and a loopback model endpoint. Pi uses SSE through
the broker; its headless Ducktape tools use a staged MCP extension, not a host
Pi configuration or installed plugin set.

## Two topologies

| topology | when | broker/cred flags |
|----------|------|-------------------|
| **Local** | Credential Provider == Computation Provider | `--gateway-host <url>` / `--host <url>` |
| **Remote** | Credential Provider != Computation Provider | `--remote <handle>.duck` (the `via` is this node's own browser gateway) |

Remote adds an `x-duck-authority` header; the local node's browser-gateway routes
it to the remote node's published `LoopbackHttp` route over the overlay.

## Remote overlay (cred ≠ compute) — how to run

The remote topology needs **no airlock or node code** beyond what ships here: the
compute side already speaks it, and the credential node exposes the gateway with
the stock gateway-route CLIs. `bin/node/tests/airlock_gateway_e2e.rs` is the
canonical, executable recipe (two real WireGuard nodes, in-process gateway + mock
upstream); run it where inline 2-node WireGuard works:

```sh
cargo test -p node-bin --test airlock_gateway_e2e --features verify -- --nocapture
```

Real quote verification is the opt-in `verify` feature — the test file compiles
to zero tests without it, and the recipe below needs a `ducktape` built with
`cargo build -p node-bin --features verify` (a default build refuses an
`Attested` trust config by name, and its `user cred inspect`/`seal`
subcommands don't exist at all).

For a real cross-machine deployment there are two lender shapes. Both publish
the same `airlock.<handle>.duck` route; they differ only in where trust comes
from.

**A. Self-hosted lender (no TEE)** — the everyday path. `ducktape user cred add`
captures the vendor login into the node's store, registers the record on chain
(pinning the store's seal PUBLIC key), and publishes the `airlock` route. The
lender daemon then serves it:

```sh
ducktape user cred add claude -n <chain-id>       # login + register + publish route
ducktape service run airlock --config node.toml   # the lender daemon (systemd unit target)
ducktape user cred grant <name> <account>         # lend it to another member
```

The daemon binds LOOPBACK and registers its port; the node reverse-proxies
overlay ingress to it, so the signed `RouteStatement` policy is a real
enforcement layer in front of it. A borrower pins `seal_pk` from the committed
credential record — no quote is involved, and the daemon spawns no container, so
a laptop with no container runtime lends perfectly well.
`bin/node/tests/cred_lending.rs` is the executable recipe (two real WireGuard
nodes, the daemon, a mock upstream).

**B. Enclave lender (TEE)** — when the borrower must not trust the operator at
all. The gateway runs inside a confidential VM and the credential is sealed to a
key that only exists in there:

```sh
MEAS=<48-byte audited-image hex>            # pinned from the audited CVM image
airlock-gateway serve --attest snp --listen 127.0.0.1:9100 \
    --anthropic-base https://api.anthropic.com     # must run IN a TDX/SNP guest
# read the measurement out of the quote for bootstrap pinning (TOFU; in prod pin
# from the audited build):
ducktape user cred inspect --attest snp --snp-product milan --host http://127.0.0.1:9100
# seal the credential (static bearer = no rotation; see "Credential" below)
ducktape user cred seal --attest snp --snp-product milan --measurement $MEAS \
    --host http://127.0.0.1:9100 \
    --credentials ~/.claude/.credentials.json --cred-kind bearer
# register the loopback port node-locally
ducktape gateway bind --workspace <node-workspace> --label airlock --port 9100
# publish the signed LoopbackHttp route (allow_authorization:true, max_response_bytes ≤ 4 MiB
# — the buffered proxy enforces this cap literally; 0/"unbounded" for live SSE);
# construct the RouteStatement exactly as signed_airlock_route() does in the test,
# then: user sign-gateway-route --key <user.key> --statement <json>  → submit to the
# node RPC (cmd:submit, target:"gateway"). Also SetHandle <handle> on duckdns.
```

A model credential sealed this way is reached by measurement (the compute
node pins `DUCKTAPE_AIRLOCK_MEASUREMENT`, below), so `cred seal` registers no
record for it. An `apple-codesign` identity is reached by NAME (`ducktape
release sign-bundle --credential <name>`), so `cred seal --vendor
apple-codesign` also writes the on-chain half: the credential record under the
attested seal key and the account's two airlock routes, both naming the node
the verb dials as their publisher (its identity and chain read off
`/v1/status`) — see "Release signing" below. The quote's roots are AMD's
pinned in the binary; `--snp-ark <pem> --snp-ask <pem>` (with `--snp-vcek`)
supply a chain out of band for an enclave that is not on AMD silicon, which
weakens only the operator who passes it.

`cred inspect`/`cred seal` reach a REMOTE enclave with `--remote <handle>.duck`
instead of `--host`; the `via` is read from this node's own browser gateway, so
the operator never pastes it. Attestation stays strictly bilateral either way —
the node is asked for nothing but that base.

**Compute node** (runs the sandbox): needs a `ducktape` built with
`cargo build -p node-bin --features verify` (a default build refuses this
`Attested` trust config by name instead of silently trusting an unverified
gateway). Point the broker at the remote gateway — no credential is held
locally:

```sh
export DUCKTAPE_AIRLOCK_REMOTE=airlock.<handle>.duck
export DUCKTAPE_AIRLOCK_VIA=$(curl -s http://<this-node-rpc>/v1/gateway/browser | jq -r .base)
export DUCKTAPE_AIRLOCK_MEASUREMENT=$MEAS
export DUCKTAPE_AIRLOCK_ATTEST=snp                  # or tdx
export DUCKTAPE_AIRLOCK_SNP_PRODUCT=milan           # snp: pin the platform generation
# optional transport overrides: DUCKTAPE_AIRLOCK_SNP_VCEK=<der file> (air-gapped),
# DUCKTAPE_AIRLOCK_PCCS_URL=<pccs> (tdx)
# then run a claude agent through the provider as usual — the run's
# /v1/messages flow crosses the overlay to the enclave.
```

The quote is fetched + verified **over the overlay** before any token is derived,
so a relaying node cannot substitute its key or read the session token. The
overlay proxy **streams** responses end to end: publish the route with
`max_response_bytes: 0` (an unbounded stream, literally) for live SSE; a
non-zero cap is enforced as a RUNNING total (declared over-length refused
before the head; unsized overflow truncates the body mid-stream). A request
body is counted at every hop against the route's own signed
`max_request_bytes`: the `airlock` route pins 16 MiB (a model turn), the
`airlock-sign` route 256 MiB (a release bundle). Nothing above a route pins a
ceiling over it — a policy may also decline to cap its body at all (`null`),
which is what a lane that never holds a request declares.

## Body AEAD (sealed sessions)

Airlock sessions are **sealed-body**: the broker seals request bodies and
unseals response streams under keys derived from the same handshake ECDH
(`bodyseal`; per-stream salted key, counter nonces, authenticated Final
marker). Consequences:

- **Path hosts see ciphertext** — including the credential-provider's node
  process OUTSIDE the enclave (`proxy_loopback` relays opaque bytes), closing
  the "operator reads conversation content" gap.
- **A stolen bearer alone is useless**: the enclave refuses plaintext bodies
  on a sealed session, and a plaintext success reply is refused by the broker
  as forgery. Missing Final marker = authenticated truncation (aborts, never a
  clean EOF). The request AEAD's associated data binds the HTTP method and
  path+query (`bodyseal::request_aad`), so a relay holding a live bearer plus
  a captured sealed blob cannot redirect it to a different method or endpoint
  — the tag fails to open under mismatched AAD. Headers stay unbound; the
  gateway's own denylist (`proxy_inner`) is the only header rule.
- The sandbox is unmodified — the broker terminates the AEAD.
- Session revocation = gateway restart (the token-signing and seal keys are
  memory-only, so every outstanding token and body key dies with the process);
  a per-sub revocation endpoint waits for real multi-tenancy.

## Session-key handshake

The client reads the gateway's `seal_pk` out of the **verified** quote REPORTDATA,
ECDHs against it, and derives a session key. `/session` returns the token
**AEAD-sealed under that key**, so only a client that talked to the *attested*
enclave can open it — a relaying node operator cannot substitute its key or read
the token.

## Credential the gateway holds

`ducktape user cred seal` uploads one of three sealed credentials
(`CredentialPayload`):

- **`Refresh`** — an OAuth refresh token; the gateway exchanges it for an access
  token and **rotates** on each refresh (subscription path).
- **`Bearer`** — a **static** access token, used as-is: no refresh, no rotation.
  `seal --credentials <file> --cred-kind bearer` seals a live subscription's
  *current* access token without invalidating the token chain its owner is still
  using — the safe way to point a run at a real credential.
- **`AppleCodesign`** (kind `apple-codesign`) — a Developer ID Application
  signing identity: the certificate + key as PKCS#12 with its password, the App
  Store Connect key as the JSON `rcodesign encode-app-store-connect-api-key`
  writes, and the Team ID. Not a model credential: a session on it is refused
  on `/v1/*` with `credential_kind_mismatch`. The gateway admits it only after
  `codesign::AppleCodesign::admit` opens the PKCS#12 (PBES1, what Keychain
  Access exports; `openssl pkcs12 -export -legacy` for the same), finds Apple's
  Developer ID Application marker extension `1.2.840.113635.100.6.1.13` on the
  leaf, and matches the leaf's subject OU to `team_id`; refusals are
  `p12_unparseable`, `not_developer_id_application`, `team_id_mismatch`,
  `api_key_malformed`. `ducktape user cred add apple-codesign --p12 <id.p12>
  --p12-password-file <pw> --api-key <key.json> --team-id <TEAMID> [name]` runs
  the same admission locally, writes the four files 0600 into this node's
  store (`airlock_service::apple_codesign_files`), and registers the record;
  `cred seal --vendor apple-codesign` with the same four flags seals it to a
  TEE gateway. `cred grant` lends it like any other kind.

## Release signing: `POST /sign/macos-bundle`

The enclave gateway signs, notarizes and staples a release bundle with an
`apple-codesign` credential it holds, so the identity never leaves it
(`sign.rs`; mounted beside `/v1/{*rest}` only when the gateway was built with
a `sign::Tools`, which `airlock-gateway` always is — the self-host lender
mounts no such route). The caller opens a SEALED session (`body_seal: true`,
`work: Direct`) on the signing credential — standing is the credential's
grant, exactly as for a model credential — and posts the `.tar.zst` of an
UNSIGNED `Ducktape.app` as the sealed body (`bodyseal::seal_request` under
`POST\n/sign/macos-bundle`). The
reply is the `.tar.zst` of the signed + notarized + stapled bundle as a sealed
chunk stream (head content type `application/zstd`). The head is committed
BEFORE the
pipeline runs — every hop between the enclave and the caller (the node's
gateway proxy, its overlay drain) waits seconds for a response head, and
Apple's notary takes minutes — and the outcome rides the stream: an empty
sealed data chunk every `sign::KEEPALIVE_INTERVAL` while the pipeline runs
(liveness for each hop's idle deadline; the node plane asserts the interval
sits under its `BODY_IDLE_TIMEOUT`), then the archive and `Final`, or a
refusal as the `Final` marker carrying its token
(`bodyseal::StreamSealer::seal_refused`, opened as `OpenedItem::Refused`).
Over the overlay the route is `airlock-sign.<handle>.duck`: the same
enclave the model lane reaches under `airlock.<handle>.duck`, under its own
label because its signed policy admits a bundle (256 MiB) where the model
lane's admits a turn (16 MiB); the node fronting the enclave binds both labels
to the same port. Inside, the pipeline is one state machine
(`Received → Validated → Signed → Notarized → Stapled`): the bundle's shape is
checked before the identity is touched (one top-level `Ducktape.app/`, no
escaping path or symlink, `CFBundleIdentifier` `dev.ducktape.app`,
`Contents/MacOS` exactly `ducktape-launcher` and `ducktape-app`), then
the p12, its password and the App Store Connect key are written 0600 into a
0700 directory under the tmpfs work root for `rcodesign sign
--code-signature-flags runtime --entitlements-xml-file …` (the launcher and
the nested `ducktape-app` both, as `ops/bundle-app-macos.sh` does),
`rcodesign notary-submit --wait` and `rcodesign staple`, and the directory is
removed on every exit path. Refusals are tokens: before the pipeline starts
they are the HTTP status and body — `credential_kind_mismatch` (403, a model
credential), a plaintext session (400); once it
runs the head is out and the token is the stream's `Final` —
`bundle_shape_refused`, `codesign_failed`, `notary_rejected`,
`staple_failed`, `tool_missing` (the image lacks `rcodesign` or the
entitlements). Audit under `target: "ducktape::airlock"`:
`release_sign_requested`, `release_sign_signed`, `release_sign_notarized`,
`release_sign_refused` — session, credential name, bundle SHA-256s, reason,
submission id; never key material. The toolchain is part of the image, not
the binary: `ops/airlock-gateway/install-rcodesign.sh` fetches the pinned
`rcodesign` release and checks its SHA-256, `ops/airlock-gateway/stage-image.sh`
(`make airlock-gateway-image`) stages it with the binary and
`app/packaging/entitlements.plist` at the paths `airlock-gateway`'s
`--rcodesign`/`--entitlements`/`--work-root` default to. `tests/sign_route.rs`
drives the route end to end with a throwaway Developer-ID-shaped identity and
the stubbed Apple seam (`sign::Notary::Stub`) — the real `rcodesign sign`, then
`rcodesign verify` on what came back; it needs `rcodesign` on `PATH` or
`DUCKTAPE_RCODESIGN` (`make rcodesign`). Notarization itself is only
exercised against Apple, from the release lane.

## Per-vendor attestation (`--attest tdx|snp`, gateway also `auto`)

Quote generation is vendor-generic via `configfs-tsm` (`tdx_guest`/`sev_guest`;
`auto` probes the provider). Verification (`airlock::verify`, feature `verify`)
is real and fails closed — there is NO mock:

- **`tdx`** — full DCAP verification via `dcap-qvl` (PCK chain anchored to the
  Intel root pinned inside the crate, TCB info, QE identity, quote signature).
  Collateral comes from Intel PCS (`--pccs-url`/`DUCKTAPE_AIRLOCK_PCCS_URL`
  overrides the endpoint, never the root). TCB status must be `UpToDate` or
  `SWHardeningNeeded`; anything else is refused by name.
- **`snp`** — full AMD chain via the `sev` crate (`crypto_nossl`): the VCEK
  (fetched from AMD KDS by chip id + reported TCB, cached; or supplied as a DER
  file) must chain to the **builtin AMD ARK/ASK for the operator-pinned product
  generation** (`--snp-product milan|genoa|turin`), and the report signature
  must verify under it. VLEK-signed reports are refused. The measurement is then
  compared to the pinned value. Known gap: no TCB-freshness gate — AMD issues
  VCEKs for older TCBs, so firmware rollback is not detected; TDX gets
  freshness from its TCB status.

`--attest` has **no default** anywhere. Trust roots are plain typed data in the
lib (`TrustRoots`); each binary parses its flags/env ONCE at its boundary — the
airlock crate itself never reads the environment.

**Dev boxes without TEE silicon** use `airlock::testkit` (feature `testkit`):
`SnpTestEnclave` mints a real-format SNP report signed by a freshly generated
test chain, the server takes it via the `build_with_quoter` seam, and clients
verify it through the REAL verifier under `enclave.roots()`. A minted quote
never verifies under the AMD builtins, so nothing here weakens production —
injecting fake roots only fools the injector. Real vendored fixtures
(`tests/fixtures/`: an Intel-signed TDX quote + collateral, an AMD-signed Milan
report + VCEK) prove the production chains offline.

## Test

```sh
cargo test -p airlock --features server,client,verify,testkit
```

Unit tests cover the crypto (seal/handshake/token/attest); the in-process e2e
test (`tests/e2e.rs`) boots the gateway server against a mock upstream and drives
the full custody path — attest → seal → handshake → proxied call → credential
swap → reply — asserting the session token never reaches the upstream (and that a
static `Bearer` credential is used without any OAuth refresh).

### The live chain

The whole chain runs against the real API from the provider crate's ignored
live tests (`claude_model_turn_in_a_microvm`, `codex_model_turn_in_a_microvm`
in `crates/services/provider/src/lib.rs`): a CLI inside a **microVM** holding
only the opaque run bearer, driven through the provider's broker to the
gateway and on to the vendor —
`microVM(claude) → provider broker → airlock gateway → api.anthropic.com`.
The sandbox never holds the credential, only the temp bearer. On a box
without TEE silicon the gateway attests through `airlock::testkit`; the
custody path is identical.

## Grafted into the product

The provider's Anthropic broker (`crates/services/provider`) can use a
verified airlock gateway as its credential SOURCE instead of a host-held
credential: set
`DUCKTAPE_AIRLOCK_GATEWAY=<url>` (local) or `DUCKTAPE_AIRLOCK_REMOTE=<handle>.duck`
+ `DUCKTAPE_AIRLOCK_VIA=<browser-gw>` (remote), plus `DUCKTAPE_AIRLOCK_MEASUREMENT`
(the pinned audited-image hex) and `DUCKTAPE_AIRLOCK_ATTEST` (`tdx`/`snp`; no
default). For snp, `DUCKTAPE_AIRLOCK_SNP_PRODUCT` pins the platform generation
(optional `DUCKTAPE_AIRLOCK_SNP_VCEK` file, `DUCKTAPE_AIRLOCK_PCCS_URL` for
tdx) — read once at the config boundary (`AirlockConfig::from_env`), so
misconfig fails there rather than mid-verify, except `attest` and
`DUCKTAPE_AIRLOCK_SNP_PRODUCT` themselves, which stay raw strings until
`attested::verify` parses them, still before any network call. The run's
`claude` traffic is then verified, handshaked, and forwarded to the gateway with
a scoped session token (re-minted on a gateway 401). The local path is exercised
end-to-end by in-process tests (`cargo test -p broker-host airlock`),
including a check that a sandbox child cannot inject the overlay routing header.

## Deferred

Per-subject session revocation (today revocation is a gateway restart, which
kills every outstanding token and body key at once), the SNP TCB-freshness
gate, and a rerun of the live chain on real TEE silicon. Subscription OAuth
proxied by a third party remains an accepted, named ToS risk — attested
custody is mitigation, not a solution.
