//! IN-PROCESS multi-validator convergence — the first mesh-free multi-validator
//! coverage in the repo.
//!
//! Three `OrderedNode<SimplexOrderer>` over `host::Host`s carrying the native
//! `directory` module, each driving a LIVE simplex
//! [`Engine`](commonware_consensus::simplex::Engine), all wired to ONE commonware
//! `simulated::Network` in ONE process — NO OS processes, NO TCP, NO
//! `authenticated::discovery` mesh. The transport bundle each engine consumes is
//! the [`consensus::MeshCarrier`] seam's sim arm ([`consensus::SimMesh`]); the
//! real node wires the discovery-network arm to the IDENTICAL
//! [`SimplexOrderer::spawn_with_carrier`] entry point, so this exercises the exact
//! production spawn path (eager relay + resolver fetch, per-process stores) with
//! only the carrier swapped.
//!
//! Each validator submits ONE distinct directory op; simplex BFT-orders them and
//! every validator applies on finalization. `directory` is order-INDEPENDENT, so
//! the three writes converge on a BYTE-IDENTICAL root-hash under any interleaving —
//! isolating the property under test (multi-validator consensus over the swapped
//! carrier) from op ordering.
//!
//! WAIT DISCIPLINE: the loop terminates on the DELIVERED-FRAME event — it exits
//! exactly when every node has drained the full op-set, never after a fixed number
//! of iterations. The `context.sleep` between drains is the deterministic virtual
//! block-tick (a coarser but still valid cut of `bin/node`'s production run
//! loop, which flushes pending ops and drains finalizations event-driven with
//! the tick as backstop), NOT a wall-clock wait — virtual time makes
//! it non-flaky regardless of CI speed. `Runner::timed` is the liveness backstop:
//! a wiring regression (unshared inbox, dropped engine handle, a carrier that
//! hands out a dead channel) surfaces as a deadline panic — you cannot make BFT
//! "not agree", so a stall is the only failure shape.
//!
//! PARTITION TESTS: `N == 3` gives byzantine tolerance `f == 0`, so simplex
//! quorum is ALL THREE validators — dropping either direction of a single link
//! is enough to strand a finalization no matter which validator it isolates.
//! `pump_ticks` is the bounded-tick counterpart to the event-driven
//! `pump_to_target`: under a partition the delivered-frame target never fires,
//! so the halt assertion instead runs a fixed number of deterministic ticks
//! (still virtual time, not a wall-clock wait) and checks NOTHING moved.

use std::collections::HashMap;
use std::time::Duration;

use commonware_consensus::simplex::{mocks, scheme::ed25519 as simplex_ed25519};
use commonware_consensus::types::Epoch;
use commonware_cryptography::{Sha256, Signer as _, ed25519};
use commonware_p2p::simulated::{self, Link};
use commonware_runtime::{Clock as _, Quota, Runner as _, Supervisor as _, deterministic};
use commonware_utils::{NZU32, NZUsize};

use consensus::{Cadence, ContentStore, Digest, SimMesh, SimplexOrderer};

/// the beat the sim runs at — simulated time, so its size is not wall-clock.
const CADENCE: Cadence = Cadence::from_millis(1_000);
use directory::Directory;
use directory::{DirMsg, encode_msg};
use host::Host;
use node::OrderedNode;
use sdk::Msg;

const N: usize = 3;
const LABELS: [&str; N] = ["v0", "v1", "v2"];

/// a fresh validator host carrying the native `directory` example module.
fn genesis_host() -> Host {
    Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis")
}

fn dir_set(k: &str, v: &str) -> Msg {
    Msg {
        target: "directory".into(),
        payload: encode_msg(&DirMsg::Set {
            key: k.into(),
            value: v.into(),
        }),
    }
}

/// one distinct directory op per validator: distinct origins (so every frame is a
/// distinct digest a peer must resolve, not echo) and distinct keys (so the final
/// directory content is identical under any permutation — order-independent).
/// `base_seed`/`key_prefix` let successive rounds in the same test mint a
/// disjoint op-set so ops never collide across rounds.
fn ops_round(base_seed: u64, key_prefix: &str) -> Vec<(ed25519::PrivateKey, u64, Msg)> {
    (0..N)
        .map(|i| {
            (
                ed25519::PrivateKey::from_seed(base_seed + i as u64),
                0u64,
                dir_set(&format!("{key_prefix}{i}"), &format!("node-{i}")),
            )
        })
        .collect()
}

fn op_set() -> Vec<(ed25519::PrivateKey, u64, Msg)> {
    ops_round(101, "k")
}

/// the perfect link every validator pair starts wired with.
fn full_link() -> Link {
    Link {
        latency: Duration::from_millis(10),
        jitter: Duration::from_millis(1),
        success_rate: commonware_utils::probability!(1.0),
    }
}

/// stand up N validators over ONE simulated network, fully linked, each an
/// `OrderedNode<SimplexOrderer>` through the SAME `spawn_with_carrier` entry
/// point the real node calls. Shared by every test in this file — convergence
/// and partition alike start from this identical cluster shape.
async fn build_cluster(
    context: &mut deterministic::Context,
) -> (
    simulated::Oracle<ed25519::PublicKey, deterministic::Context>,
    Vec<ed25519::PublicKey>,
    Vec<OrderedNode<SimplexOrderer>>,
) {
    let namespace = b"consensus".to_vec();
    let epoch = Epoch::new(333);

    // N sorted participants + index-aligned per-validator ed25519 schemes.
    let fixture = simplex_ed25519::fixture(context, &namespace, N as u32);
    let participants = fixture.participants.clone();
    let schemes = fixture.schemes.clone();

    // ONE simulated network seeded with the participant set — the drop-in for the
    // real encrypted-TCP discovery mesh (which live-locks under the deterministic
    // clock). No OS processes, no sockets.
    let (network, oracle) = simulated::Network::new_with_peers(
        context.child("network"),
        simulated::Config {
            max_size: 1024 * 1024,
            disconnect_on_block: true,
            max_peers_per_set: NZUsize!(32),
            tracked_peer_sets: NZUsize!(1),
        },
        participants.clone(),
    )
    .await;
    network.start();

    // register each validator's mesh carrier: the sim arm bundles the five channel
    // pairs (vote/cert/resolver/payload/fetch) + the oracle's provider/blocker,
    // registered up front (before any engine starts) exactly as the production boot
    // path registers its five fixed engine lanes.
    let quota = Quota::per_second(NZU32!(128));
    let mut carriers: HashMap<ed25519::PublicKey, SimMesh<deterministic::Context>> = HashMap::new();
    for v in participants.iter() {
        carriers.insert(
            v.clone(),
            SimMesh::register(&oracle, v.clone(), quota).await,
        );
    }

    // perfect all-pairs links so votes/certs/relayed payloads propagate.
    let link = full_link();
    for v1 in participants.iter() {
        for v2 in participants.iter() {
            if v1 == v2 {
                continue;
            }
            oracle
                .add_link(v1.clone(), v2.clone(), link.clone())
                .await
                .expect("link validators");
        }
    }

    // byte-identical genesis Floor on every validator (else engines never agree on
    // genesis and the deadline panics).
    let genesis_floor: Digest = mocks::application::genesis::<Sha256>(epoch);

    // stand up N validators, each an OrderedNode over a SimplexOrderer built from
    // its carrier through the SAME spawn_with_carrier the real node calls.
    let mut nodes: Vec<OrderedNode<SimplexOrderer>> = Vec::new();
    for (idx, v) in participants.iter().enumerate() {
        let carrier = carriers.remove(v).expect("validator carrier registered");
        // PER-PROCESS store (the production path): the leader gossips its proposed
        // frame's bytes on the payload channel, peers cache them, and the resolver
        // fetch backstops any miss — the carrier owns the wiring.
        let orderer = SimplexOrderer::spawn_with_carrier(
            context.child(LABELS[idx]),
            schemes[idx].clone(),
            carrier,
            v.clone(),
            v.to_string(),
            epoch,
            genesis_floor,
            None,
            ContentStore::new(),
            CADENCE,
            false,
        );
        nodes.push(OrderedNode::new(genesis_host(), orderer));
    }

    // identical genesis module set -> identical genesis root-hash.
    let genesis = nodes[0].root_hash();
    for n in &nodes {
        assert_eq!(
            n.root_hash(),
            genesis,
            "identical genesis -> identical root-hash"
        );
    }

    (oracle, participants, nodes)
}

/// submit op `i` to node `i` — the FIFO distribution `converge` and the
/// partition tests all rely on (exactly one op per validator).
async fn submit_ops(
    nodes: &mut [OrderedNode<SimplexOrderer>],
    ops: &[(ed25519::PrivateKey, u64, Msg)],
) {
    for (i, (origin, seq, msg)) in ops.iter().enumerate() {
        nodes[i]
            .submit(origin, *seq, msg.clone())
            .await
            .expect("submit");
    }
}

/// pump EVENT-DRIVEN to convergence: exits the instant every node has drained
/// `target` more delivered frames. `context.sleep` is the deterministic virtual
/// block-tick, never a wall-clock wait.
async fn pump_to_target(
    context: &mut deterministic::Context,
    nodes: &mut [OrderedNode<SimplexOrderer>],
    target: usize,
) -> [usize; N] {
    let mut applied = [0usize; N];
    while applied.iter().any(|&c| c < target) {
        context.sleep(CADENCE.block_time).await;
        for (i, n) in nodes.iter_mut().enumerate() {
            n.flush_batch().await.expect("flush");
            applied[i] += n.drain_delivered().await.expect("drain");
        }
    }
    applied
}

/// pump a FIXED number of deterministic ticks and report what drained. Used
/// only where the event-driven target can never fire by construction (a
/// partition below quorum) — the bound is a liveness ceiling, not a deadline
/// panic, and it is still virtual time under `deterministic::Context`.
async fn pump_ticks(
    context: &mut deterministic::Context,
    nodes: &mut [OrderedNode<SimplexOrderer>],
    ticks: usize,
) -> [usize; N] {
    let mut applied = [0usize; N];
    for _ in 0..ticks {
        context.sleep(CADENCE.block_time).await;
        for (i, n) in nodes.iter_mut().enumerate() {
            n.flush_batch().await.expect("flush");
            applied[i] += n.drain_delivered().await.expect("drain");
        }
    }
    applied
}

/// remove both directions of the link between `a` and `b`.
async fn sever(
    oracle: &simulated::Oracle<ed25519::PublicKey, deterministic::Context>,
    a: &ed25519::PublicKey,
    b: &ed25519::PublicKey,
) {
    oracle
        .remove_link(a.clone(), b.clone())
        .await
        .expect("remove link a->b");
    oracle
        .remove_link(b.clone(), a.clone())
        .await
        .expect("remove link b->a");
}

/// restore both directions of the perfect link between `a` and `b`.
async fn heal(
    oracle: &simulated::Oracle<ed25519::PublicKey, deterministic::Context>,
    a: &ed25519::PublicKey,
    b: &ed25519::PublicKey,
) {
    let link = full_link();
    oracle
        .add_link(a.clone(), b.clone(), link.clone())
        .await
        .expect("heal link a->b");
    oracle
        .add_link(b.clone(), a.clone(), link)
        .await
        .expect("heal link b->a");
}

/// the number of deterministic block-ticks the halt assertion pumps through —
/// generous relative to the ~single-digit ticks convergence normally takes, so
/// a real halt is never mistaken for a slow-but-live round.
const HALT_TICKS: usize = 30;

async fn converge(mut context: deterministic::Context) {
    let (_oracle, _participants, mut nodes) = build_cluster(&mut context).await;
    let genesis = nodes[0].root_hash();

    // distribute the op-set ONE PER VALIDATOR: each digest lands in exactly one
    // node's FIFO, proposed only when that node leads.
    let ops = op_set();
    submit_ops(&mut nodes, &ops).await;

    // no optimistic echo: after every submit, before any finalization drains, every
    // node is still at genesis.
    for n in &nodes {
        assert_eq!(
            n.root_hash(),
            genesis,
            "no optimistic echo: submit does not advance root-hash"
        );
    }

    // PUMP to convergence: exit is EVENT-DRIVEN — the moment every node has drained
    // the full op-set (delivered frames == target).
    let target = ops.len();
    let applied = pump_to_target(&mut context, &mut nodes, target).await;

    // THE MILESTONE: byte-identical root-hash on every validator, moved off genesis,
    // reached with no OS-process mesh — only the swapped in-process carrier.
    let converged = nodes[0].root_hash();
    assert_ne!(
        converged, genesis,
        "the finalized ops moved the root-hash off genesis"
    );
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(
            applied[i], target,
            "validator {i} applied EXACTLY the op-set"
        );
        assert_eq!(
            n.root_hash(),
            converged,
            "validator {i} converges on the identical root-hash"
        );
    }
}

/// isolate ONE validator (index 2) both directions: with `N == 3` (`f == 0`)
/// quorum needs all three, so the two still-linked validators cannot finalize
/// anything either — the chain halts for everyone, not just the isolated node.
/// Heal and confirm the pending ops finalize and every root agrees.
async fn partition_isolated_c(mut context: deterministic::Context) {
    let (oracle, participants, mut nodes) = build_cluster(&mut context).await;
    let genesis = nodes[0].root_hash();

    // reach height H first, all links up.
    submit_ops(&mut nodes, &op_set()).await;
    let applied = pump_to_target(&mut context, &mut nodes, N).await;
    for (i, &a) in applied.iter().enumerate() {
        assert_eq!(a, N, "validator {i} reaches height H");
    }
    let height_h = nodes[0].root_hash();
    assert_ne!(height_h, genesis, "height H moved off genesis");
    for n in &nodes {
        assert_eq!(
            n.root_hash(),
            height_h,
            "every validator agrees at height H"
        );
    }

    // partition: sever validator C from both A and B.
    let c = participants[2].clone();
    sever(&oracle, &participants[0], &c).await;
    sever(&oracle, &participants[1], &c).await;

    // submit a disjoint op-set behind the partition — nothing can finalize it.
    submit_ops(&mut nodes, &ops_round(201, "p")).await;
    pump_ticks(&mut context, &mut nodes, HALT_TICKS).await;
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(
            n.root_hash(),
            height_h,
            "validator {i} does not finalize past height H while C is partitioned"
        );
    }

    // heal: finalization resumes and every validator converges on one root.
    heal(&oracle, &participants[0], &c).await;
    heal(&oracle, &participants[1], &c).await;
    let applied = pump_to_target(&mut context, &mut nodes, N).await;
    let converged = nodes[0].root_hash();
    assert_ne!(
        converged, height_h,
        "the healed op-set moved the root-hash past height H"
    );
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(
            applied[i], N,
            "validator {i} applies exactly the healed op-set"
        );
        assert_eq!(
            n.root_hash(),
            converged,
            "validator {i} converges on the identical root-hash"
        );
    }
}

/// partition A from {B, C}: the minority side is a single validator, same as
/// the previous test in shape but severing a DIFFERENT pair of links (A-B and
/// A-C, leaving B-C up) — still halts under `f == 0` quorum, and heals to one
/// root exactly the same way.
async fn partition_isolated_a(mut context: deterministic::Context) {
    let (oracle, participants, mut nodes) = build_cluster(&mut context).await;
    let genesis = nodes[0].root_hash();

    submit_ops(&mut nodes, &op_set()).await;
    let applied = pump_to_target(&mut context, &mut nodes, N).await;
    for (i, &a) in applied.iter().enumerate() {
        assert_eq!(a, N, "validator {i} reaches height H");
    }
    let height_h = nodes[0].root_hash();
    assert_ne!(height_h, genesis, "height H moved off genesis");

    // partition A (index 0) from {B, C}: sever A-B and A-C, leave B-C up.
    let a = participants[0].clone();
    sever(&oracle, &a, &participants[1]).await;
    sever(&oracle, &a, &participants[2]).await;

    submit_ops(&mut nodes, &ops_round(301, "q")).await;
    pump_ticks(&mut context, &mut nodes, HALT_TICKS).await;
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(
            n.root_hash(),
            height_h,
            "validator {i} does not finalize past height H while A is partitioned"
        );
    }

    // heal: every validator, on both sides of the old cut, reaches ONE root.
    heal(&oracle, &a, &participants[1]).await;
    heal(&oracle, &a, &participants[2]).await;
    let applied = pump_to_target(&mut context, &mut nodes, N).await;
    let converged = nodes[0].root_hash();
    assert_ne!(
        converged, height_h,
        "the healed op-set moved the root-hash past height H"
    );
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(
            applied[i], N,
            "validator {i} applies exactly the healed op-set"
        );
        assert_eq!(
            n.root_hash(),
            converged,
            "validator {i} converges on the identical root-hash"
        );
    }
}

#[test]
fn three_validators_converge_in_process() {
    deterministic::Runner::timed(Duration::from_secs(120)).start(converge);
}

#[test]
fn in_process_convergence_is_robust_across_schedules() {
    // convergence must not depend on one lucky task interleaving: round-robin
    // leaders + perfect links make the result schedule-independent — pin it down
    // under several distinct seeds (each its own 120s liveness bound).
    for seed in [1u64, 7, 99] {
        let cfg = deterministic::Config::default()
            .with_seed(seed)
            .with_timeout(Some(Duration::from_secs(120)));
        deterministic::Runner::new(cfg).start(converge);
    }
}

#[test]
fn a_single_partitioned_validator_halts_the_chain() {
    for seed in [1u64, 7, 99] {
        let cfg = deterministic::Config::default()
            .with_seed(seed)
            .with_timeout(Some(Duration::from_secs(120)));
        deterministic::Runner::new(cfg).start(partition_isolated_c);
    }
}

#[test]
fn a_healed_partition_reaches_one_root() {
    for seed in [1u64, 7, 99] {
        let cfg = deterministic::Config::default()
            .with_seed(seed)
            .with_timeout(Some(Duration::from_secs(120)));
        deterministic::Runner::new(cfg).start(partition_isolated_a);
    }
}
