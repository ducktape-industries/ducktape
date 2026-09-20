//! the global `.duck` gateway registry over the wire: the consensus surface a
//! publisher drives and a resolver reads, driven deterministically over noded's
//! exact /v1 lane. reactor_seams.rs already proved ONE member-signed `SetRoute`
//! rides the whole-registry sweep; this file generalizes that ceremony and pins
//! the registry's own laws:
//!
//! - the `Get`/`List` query surfaces: `Get` returns the full record (a tombstone
//!   included — a publisher continues its revision stream through it), `List`
//!   returns only the LIVE routes in canonical [`gateway::RouteName`] order.
//! - the per-name MONOTONIC revision CAS: a route's next write must carry exactly
//!   `current + 1` (a fresh name starts at 1), so a stale or skipped revision is
//!   refused with the module's verbatim `route revision must be N` string.
//! - the `route = None` authenticated TOMBSTONE (a signed unset): it commits like
//!   any route, ADVANCES the per-name revision (replay-proof), stays queryable
//!   through `Get`, and drops out of `List`.
//! - two policy rejections the module enforces at ingest: the DuckFs content-route
//!   constraint (GET+HEAD, bodyless, capped) and the strictly-sorted method rule.
//!   both trip the module's OWN `route_signing_preimage` validation, ahead of any
//!   signature check — so no honest signature over the malformed statement exists
//!   (or is needed): the placeholder authorization never gets inspected.
//!
//! authorship is the reactor_seams ceremony exactly: the founding Ed25519 member
//! signs the route-signing preimage under `GATEWAY_ROUTE_NS` and submits the op
//! as the origin (its `hex:` escape). the sim wires `Gateway::new(.., None,
//! LOCAL_CHAIN_ID)` (no valset), so membership gating is absent and the only ceremony
//! is the account's Create plus the member signature — the same reach
//! reactor_seams walks.

mod harness;

use commonware_cryptography::Signer as _;
use harness::{Sim, create, key_origin};
use serde_json::{Value, json};
use std::path::Path;

type Ed = commonware_cryptography::ed25519::PrivateKey;

/// the sim's gateway chain id (the composer's `Bindings { chain_id }`).
const CHAIN: &str = noded::LOCAL_CHAIN_ID;
/// the account the first Create founds.
const ACCOUNT: u64 = 1;

// ── the founding ceremony ───────────────────────────────

/// spawn an `--auto` sim and found one Identity account for `key` (account 1 on
/// the `local` chain), seating it as the sole current Ed25519 member. returns the
/// sim, the account key, and the key's submit origin (its `hex:` escape).
fn published(storage: &Path) -> (Sim, Ed, String) {
    let sim = Sim::spawn(storage, &["--auto"]);
    let key = Ed::from_seed(7);
    let origin = key_origin(&key);
    sim.submit_ok("identity", create("alice"), Some(&origin));
    (sim, key, origin)
}

// ── route builders ──────────────────────────────────────

/// a permissive owner-only loopback route (GET) — the default live route body.
fn loopback_route() -> gateway::RouteDefinition {
    gateway::RouteDefinition {
        target: gateway::RouteTarget::LoopbackHttp,
        policy: gateway::RoutePolicy {
            audience: gateway::RouteAudience::Owner,
            methods: vec![gateway::RouteMethod::Get],
            max_request_bytes: Some(0),
            max_response_bytes: 1024,
            allow_authorization: false,
            allow_upgrade: false,
        },
    }
}

/// a route statement for the account `key` founded, published by a node the
/// account vouches for (any 32-byte key; the module checks no publisher).
fn statement(
    key: &Ed,
    name: gateway::RouteName,
    revision: u64,
    route: Option<gateway::RouteDefinition>,
) -> gateway::RouteStatement {
    gateway::RouteStatement {
        chain_id: CHAIN.into(),
        account_id: ACCOUNT,
        name,
        publisher_node: key.public_key().as_ref().to_vec(),
        revision,
        route,
    }
}

/// a member-signed `SetRoute` op: `key` signs the route-signing preimage under
/// `GATEWAY_ROUTE_NS`. sound only for a VALID statement (the preimage validates).
fn signed_set_route(key: &Ed, statement: gateway::RouteStatement) -> Value {
    let preimage = gateway::route_signing_preimage(&statement).expect("valid statement");
    let signature = key.sign(gateway::GATEWAY_ROUTE_NS, &preimage);
    let authorization = gateway::MemberAuthorization {
        signer: key.public_key().as_ref().to_vec(),
        signature: signature.as_ref().to_vec(),
    };
    serde_json::to_value(gateway::GatewayMsg::SetRoute {
        statement,
        authorization,
    })
    .expect("gateway op serializes")
}

/// a `SetRoute` op over a STRUCTURALLY-INVALID statement. the module rejects it
/// at its own `route_signing_preimage` (statement validation) BEFORE checking the
/// signature, so no honest signature exists — the placeholder is never inspected.
/// the signer is still the current member, so the rejection is the policy gate,
/// not the membership gate.
fn unsigned_set_route(key: &Ed, statement: gateway::RouteStatement) -> Value {
    let authorization = gateway::MemberAuthorization {
        signer: key.public_key().as_ref().to_vec(),
        signature: vec![0u8; 64],
    };
    serde_json::to_value(gateway::GatewayMsg::SetRoute {
        statement,
        authorization,
    })
    .expect("gateway op serializes")
}

// ── query helpers ───────────────────────────────────────

/// the `Get` reply's `Option<RouteRecord>` for one exact name.
fn get_route(sim: &Sim, account: u64, name: gateway::RouteName) -> Value {
    let query = serde_json::to_value(gateway::GatewayQuery::Get {
        account_id: account,
        name,
    })
    .expect("query serializes");
    sim.query("gateway", query)["route"].clone()
}

/// the `List` reply's `Vec<RouteSummary>` for one account.
fn list_routes(sim: &Sim, account: u64) -> Value {
    let query = serde_json::to_value(gateway::GatewayQuery::List {
        account_id: account,
    })
    .expect("query serializes");
    sim.query("gateway", query)["routes"].clone()
}

fn apex() -> gateway::RouteName {
    gateway::RouteName::apex()
}
fn named(label: &str) -> gateway::RouteName {
    gateway::RouteName::named(label)
}

// ── Get / List + the tombstone ──────────────────────────

/// `Get` exposes every record (live or tombstoned); `List` is the bounded LIVE
/// projection. a signed `route = None` tombstone commits, advances the per-name
/// revision, stays visible to `Get`, and drops from `List` — so a resolver's
/// enumeration never surfaces an unset route, while a publisher can still read
/// (and continue) the revision stream through the exact-name `Get`.
#[test]
fn get_returns_records_list_returns_live_routes_and_a_tombstone_drops_from_the_list() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, key, origin) = published(storage.path());

    // publish an apex route and a named `api` route, each at its opening revision.
    sim.submit_ok(
        "gateway",
        signed_set_route(&key, statement(&key, apex(), 1, Some(loopback_route()))),
        Some(&origin),
    );
    sim.submit_ok(
        "gateway",
        signed_set_route(
            &key,
            statement(&key, named("api"), 1, Some(loopback_route())),
        ),
        Some(&origin),
    );

    // Get resolves each exact name to a live record (route present).
    let apex_rec = get_route(&sim, ACCOUNT, apex());
    assert!(
        apex_rec["statement"]["route"].is_object(),
        "apex is live: {apex_rec}"
    );
    assert!(
        get_route(&sim, ACCOUNT, named("api"))["statement"]["route"].is_object(),
        "api is live"
    );

    // List carries both, in canonical RouteName order (apex sorts before labels).
    let live = list_routes(&sim, ACCOUNT);
    let names: Vec<Value> = live
        .as_array()
        .expect("routes array")
        .iter()
        .map(|s| s["name"]["label"].clone())
        .collect();
    assert_eq!(
        names,
        vec![Value::Null, json!("api")],
        "List is the live set in canonical order: {live}"
    );
    assert_eq!(live[0]["target"], "loopback_http", "target kind reported");

    // TOMBSTONE the apex: a signed `route = None` at the next revision.
    sim.submit_ok(
        "gateway",
        signed_set_route(&key, statement(&key, apex(), 2, None)),
        Some(&origin),
    );

    // Get STILL resolves the apex — but its route is now null (a queryable
    // tombstone the publisher reads to continue the revision stream).
    let tomb = get_route(&sim, ACCOUNT, apex());
    assert!(
        !tomb.is_null(),
        "the tombstone record is still queryable: {tomb}"
    );
    assert!(
        tomb["statement"]["route"].is_null(),
        "the tombstone carries no route: {tomb}"
    );
    assert_eq!(
        tomb["statement"]["revision"], 2,
        "the unset advanced the revision"
    );

    // List drops it: only the live `api` route remains.
    let live = list_routes(&sim, ACCOUNT);
    assert_eq!(
        live.as_array().map(Vec::len),
        Some(1),
        "the tombstone left the live list: {live}"
    );
    assert_eq!(live[0]["name"]["label"], "api", "only api is live: {live}");

    // the tombstone genuinely advanced the per-name revision: the next apex write
    // must be 3, so re-stating revision 2 is a stale CAS.
    let error = sim.submit_rejected(
        "gateway",
        signed_set_route(&key, statement(&key, apex(), 2, Some(loopback_route()))),
        Some(&origin),
    );
    assert!(
        error.contains("route revision must be 3, got 2"),
        "the tombstone advanced the revision past 2: {error}"
    );
}

// ── the per-name monotonic revision CAS ─────────────────

/// each route name carries an INDEPENDENT monotonic revision, and a write must
/// carry exactly `current + 1`: a fresh name opens at 1, a stale or skipped
/// revision is refused. this is the registry's replay/authority gate — a resolver
/// and a publisher never disagree on which statement is current.
#[test]
fn the_per_name_revision_is_a_strict_monotonic_cas() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, key, origin) = published(storage.path());

    // open `api` at revision 1.
    sim.submit_ok(
        "gateway",
        signed_set_route(
            &key,
            statement(&key, named("api"), 1, Some(loopback_route())),
        ),
        Some(&origin),
    );

    // re-stating revision 1 is stale: the CAS demands 2.
    let error = sim.submit_rejected(
        "gateway",
        signed_set_route(
            &key,
            statement(&key, named("api"), 1, Some(loopback_route())),
        ),
        Some(&origin),
    );
    assert!(
        error.contains("route revision must be 2, got 1"),
        "a stale revision is refused: {error}"
    );

    // skipping to revision 3 is refused too — the CAS is strict `current + 1`.
    let error = sim.submit_rejected(
        "gateway",
        signed_set_route(
            &key,
            statement(&key, named("api"), 3, Some(loopback_route())),
        ),
        Some(&origin),
    );
    assert!(
        error.contains("route revision must be 2, got 3"),
        "a skipped revision is refused: {error}"
    );

    // revision 2 lands and becomes current.
    sim.submit_ok(
        "gateway",
        signed_set_route(
            &key,
            statement(&key, named("api"), 2, Some(loopback_route())),
        ),
        Some(&origin),
    );
    assert_eq!(
        get_route(&sim, ACCOUNT, named("api"))["statement"]["revision"],
        2,
        "revision 2 is current"
    );

    // an INDEPENDENT fresh name must open at 1 — opening at 2 is refused, proving
    // the revision stream is per-name, not global.
    let error = sim.submit_rejected(
        "gateway",
        signed_set_route(
            &key,
            statement(&key, named("other"), 2, Some(loopback_route())),
        ),
        Some(&origin),
    );
    assert!(
        error.contains("route revision must be 1, got 2"),
        "a fresh name opens at revision 1: {error}"
    );
}

// ── policy / audience validation ────────────────────────

/// the module validates the route statement at ingest (inside its own
/// `route_signing_preimage`), so a malformed policy is refused BEFORE the
/// signature is ever checked. two of those gates pinned here: DuckFs content
/// routes must be GET+HEAD/bodyless/capped, and every policy's methods must be
/// strictly sorted and unique. the signer is a genuine current member, so these
/// are the POLICY gate refusing — not the membership or signature gates.
/// The request cap is the route's own number and the module pins no ceiling
/// over it: a route that declares none, and a route that declares one far past
/// anything a node would buffer, are both admitted. What a publisher will hold
/// in memory is a property of the lane serving the route, not of consensus.
#[test]
fn a_route_declares_its_own_request_cap_or_none_at_all() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, key, origin) = published(storage.path());

    let uncapped = gateway::RouteDefinition {
        target: gateway::RouteTarget::LoopbackHttp,
        policy: gateway::RoutePolicy {
            audience: gateway::RouteAudience::Owner,
            methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
            max_request_bytes: None,
            max_response_bytes: 1024,
            allow_authorization: false,
            allow_upgrade: false,
        },
    };
    sim.submit_ok(
        "gateway",
        signed_set_route(&key, statement(&key, named("git"), 1, Some(uncapped))),
        Some(&origin),
    );

    let huge = gateway::RouteDefinition {
        target: gateway::RouteTarget::LoopbackHttp,
        policy: gateway::RoutePolicy {
            audience: gateway::RouteAudience::Owner,
            methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
            max_request_bytes: Some(u64::MAX),
            max_response_bytes: 1024,
            allow_authorization: false,
            allow_upgrade: false,
        },
    };
    sim.submit_ok(
        "gateway",
        signed_set_route(&key, statement(&key, named("huge"), 1, Some(huge))),
        Some(&origin),
    );
}

#[test]
fn malformed_route_policies_are_refused_by_the_content_and_method_gates() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, key, origin) = published(storage.path());

    // a DuckFs content route offering only GET (missing HEAD) breaks the content
    // constraint — GET+HEAD, no request body, no auth, no upgrade, bounded cap.
    let duckfs = gateway::RouteDefinition {
        target: gateway::RouteTarget::DuckFs {
            manifest_sha256: "ab".repeat(32), // 64 lowercase hex
        },
        policy: gateway::RoutePolicy {
            audience: gateway::RouteAudience::Owner,
            methods: vec![gateway::RouteMethod::Get],
            max_request_bytes: Some(0),
            max_response_bytes: 1024,
            allow_authorization: false,
            allow_upgrade: false,
        },
    };
    let error = sim.submit_rejected(
        "gateway",
        unsigned_set_route(&key, statement(&key, named("site"), 1, Some(duckfs))),
        Some(&origin),
    );
    assert!(
        error.contains("content routes require GET+HEAD"),
        "the DuckFs content-route constraint: {error}"
    );

    // a loopback route whose methods are out of order trips the strict-sort gate.
    let unsorted = gateway::RouteDefinition {
        target: gateway::RouteTarget::LoopbackHttp,
        policy: gateway::RoutePolicy {
            audience: gateway::RouteAudience::Owner,
            methods: vec![gateway::RouteMethod::Post, gateway::RouteMethod::Get],
            max_request_bytes: Some(1024),
            max_response_bytes: 1024,
            allow_authorization: false,
            allow_upgrade: false,
        },
    };
    let error = sim.submit_rejected(
        "gateway",
        unsigned_set_route(&key, statement(&key, named("svc"), 1, Some(unsorted))),
        Some(&origin),
    );
    assert!(
        error.contains("methods must be strictly sorted and unique"),
        "the strict-method-order gate: {error}"
    );
}
