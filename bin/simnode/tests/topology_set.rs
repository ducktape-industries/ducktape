//! Genesis-composition parity: the sim's default (and `--with-valset`) genesis
//! sets ARE the `sim_base` (+ `sim_valset`) selections of the single-source
//! `topology`, composed through the SAME `noded::compose::compose` bin/node
//! runs. `bin/noded/tests/daemon_e2e.rs` pins the same `sim_base` against
//! noded; this pins the sim composer against the topology it draws from,
//! and pins that composing `sim_valset` gives governance its code registry.

mod harness;

use harness::Sim;

fn module_ids(status: &serde_json::Value) -> Vec<String> {
    status["modules"]
        .as_array()
        .expect("status carries a modules array")
        .iter()
        .map(|m| m["id"].as_str().expect("module id is a string").to_string())
        .collect()
}

#[test]
fn default_genesis_composes_topology_sim_base() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &[]);
    let status = sim.status();

    let mut want: Vec<String> = topology::SIM_BASE.iter().map(|s| s.to_string()).collect();
    want.sort_unstable();
    assert_eq!(
        module_ids(&status),
        want,
        "sim default genesis composes topology sim_base; status lists the host's set by id"
    );
    // the module a capability-tagged saga draws its Strict-lease pool from
    // ANSWERS here — an empty set — instead of erroring `UnknownModule`. That
    // is the precondition composing it buys, and the whole of what the DEFAULT
    // set can show: the registry's WRITE path is member-gated on valset, which
    // `sim_base` does not carry, so every announce here is refused and the pool
    // is empty by construction. Populating it (and so making the Strict lease
    // bind instead of degrading to accept-any) needs `--with-valset` — see
    // `with_valset_announce_populates_the_capability_provider_pool` below.
    let providers = sim.query(
        "capability",
        serde_json::json!({ "providers": { "capability": "echo" } }),
    );
    assert_eq!(
        providers,
        serde_json::json!({ "providers": [] }),
        "the default sim composes capability, with nothing announced"
    );
}

#[test]
fn with_valset_genesis_appends_topology_sim_valset_and_wires_the_code_registry() {
    // any 32-byte value is an accepted genesis validator key (the binary checks
    // length, not curve membership).
    let key = "11".repeat(32);
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto", "--with-valset", &key]);
    let status = sim.status();

    let mut want: Vec<String> = topology::SIM_BASE
        .iter()
        .chain(topology::SIM_VALSET)
        .map(|s| s.to_string())
        .collect();
    want.sort_unstable();
    assert_eq!(
        module_ids(&status),
        want,
        "--with-valset composes topology sim_base plus sim_valset; status lists the host's set by id"
    );

    // governance composes as a WASM tenant now, so the modules code registry
    // it is wired to comes with it: an UpdateModule proposal opens a ballot
    // instead of being refused at the door. the code hash names no component
    // the network has — the registry refuses that at execute, which is a different
    // (and later) gate; the claim here is only that a registry exists at all.
    let propose = harness::module_contracts::GovMsg::Propose {
        proposal_id: "u".into(),
        action: harness::module_contracts::GovAction::UpdateModule {
            name: "x".into(),
            module_id: "chat".into(),
            activation_lead: 10_000,
            code_hash: vec![0; 32],
        },
        voting_period: 600_000,
    };
    let (code, reply) = sim.submit(
        "governance",
        serde_json::to_value(propose).expect("Propose serializes"),
        // the genesis validator's own `hex:` origin — the electorate gate wants
        // a member NODE key, and only the escape can express raw key bytes.
        Some(&format!("hex:{key}")),
    );
    assert_eq!(
        code, 200,
        "an UpdateModule proposal must open a ballot in the sim: {reply}"
    );

    // and the ballot is really THERE: governance took the action into committed
    // state, so the registry is wired all the way through, not just past the door.
    let proposal = sim.query(
        "governance",
        serde_json::json!({ "proposal": { "proposal_id": "u" } }),
    );
    assert_eq!(
        proposal["proposal"]["status"], "open",
        "the UpdateModule proposal is open for votes: {proposal}"
    );
    assert_eq!(
        proposal["proposal"]["action"]["update_module"]["module_id"], "chat",
        "the open ballot carries the proposed swap: {proposal}"
    );
}

/// The capability WRITE path, which only `--with-valset` can reach: the
/// registry gates every announce on valset standing (`members_and_residents`),
/// so it is here — not in the default set — that a scenario can populate the
/// provider pool a Strict-lease saga draws from. Without this the sim composes
/// capability but can never hold a provider, and the lease still degrades to
/// accept-any everywhere.
#[test]
fn with_valset_announce_populates_the_capability_provider_pool() {
    // the announcer IS the seeded genesis validator: the `hex:` origin escape
    // resolves to the same raw 32 bytes `--with-valset` seeds valset with, so
    // the member gate admits it.
    let node = vec![0x22u8; 32];
    let key = "22".repeat(32);
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto", "--with-valset", &key]);

    // --auto commits each submit, so the read below sees the announce with no
    // step and nothing to wait on.
    sim.submit_ok(
        "capability",
        serde_json::json!({ "announce": { "capabilities": ["echo"], "resources": {} } }),
        Some(&format!("hex:{key}")),
    );

    let providers = sim.query(
        "capability",
        serde_json::json!({ "providers": { "capability": "echo" } }),
    );
    assert_eq!(
        providers,
        serde_json::json!({ "providers": [node] }),
        "the announcing validator is the echo provider pool"
    );
}
