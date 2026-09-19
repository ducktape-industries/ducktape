//! BYZANTINE payload-flood tolerance over the real relay / per-process-store path.
//!
//! the honest N=5 milestone (`simplex_agreed_order.rs`) runs `SimplexOrderer::
//! spawn` over ONE shared `ContentStore` — there is no payload lane to attack.
//! this harness runs `SimplexOrderer::spawn_with_resolver` (the production
//! constructor, starve off) instead: each validator owns a PER-PROCESS store,
//! gossips its proposed frame's bytes on a dedicated payload channel, and a
//! STORE-ONLY drain caches peer-relayed frames. that lane is the surface a
//! byzantine peer floods. (the resolver fetch backstop is wired but idle here —
//! every honest peer caches the eager gossip; the miss path is proven by
//! `payload_fetch_late_join.rs`.)
//!
//! two tests:
//!  * `relay_path_converges_including_qmdb_root` — the honest baseline: prove the
//!    relay path CONVERGES here (byte-identical root-hash + order-dependent qmdb
//!    root), so a peer resolves a finalized digest solely via the relay into its
//!    own store. a broken relay hangs every peer to the deadline.
//!  * `byzantine_payload_flood_is_inert` — ONE validator (v4) is byzantine: it
//!    still votes/relays honestly, but ALSO floods the payload channel with
//!    (a) random attacker-chosen bytes AND (b) a WELL-FORMED node frame it never
//!    proposed to consensus. assert the honest nodes still converge on the
//!    byte-identical CORRECT root-hash incl. the qmdb root, applying EXACTLY the
//!    finalized ops and nothing flooded, and hold every flooded blob in their
//!    stores under its OWN sha256 (content-addressed -> can never match a
//!    finalized digest, so `store.get(finalized_digest)` never resolves it).
//!
//! consensus-level byzantine faults (equivocation, forged votes) are commonware
//! simplex's own BFT guarantee (f < N/3) and are NOT re-tested here; this pins
//! the app-level content-addressing defense on the payload lane.

use std::collections::HashMap;
use std::time::Duration;

use commonware_consensus::simplex::{mocks, scheme::ed25519 as simplex_ed25519};
use commonware_consensus::types::Epoch;
use commonware_cryptography::Sha256;
use commonware_p2p::simulated::{self, Link};
use commonware_p2p::{Recipients, Sender as _};
use commonware_runtime::{
    Clock as _, IoBuf, Quota, Runner as _, Spawner as _, Supervisor as _, deterministic,
};
use commonware_utils::{NZU32, NZUsize};

use consensus::{Cadence, ContentStore, Digest, PAYLOAD_CACHE_BYTES, SimplexOrderer, digest_of};

/// the beat the sim runs at — simulated time, so its size is not wall-clock.
const CADENCE: Cadence = Cadence::from_millis(1_000);
use directory::Directory;
use directory::{DirMsg, encode_msg};
use host::Host;
use kv::Kv;
use kv::{KvMsg, encode};
use node::{OrderedNode, encode_frame};
use sdk::Msg;
use statesync::qmdb::QmdbStore;

const N: usize = 5;
const LABELS: [&str; N] = ["v0", "v1", "v2", "v3", "v4"];
/// the single byzantine validator: a full consensus participant that ALSO floods
/// the payload channel with garbage.
const BYZ: usize = N - 1;

async fn genesis_host(ctx: deterministic::Context) -> Host {
    let kv = Kv::new("kv", Box::new(QmdbStore::init(ctx, "kv").await));
    Host::genesis(vec![Box::new(kv), Box::new(Directory::new("directory"))]).expect("genesis")
}

fn kv_set(k: &[u8], v: &[u8]) -> Msg {
    Msg {
        target: "kv".into(),
        payload: encode(&KvMsg::Set {
            key: k.to_vec(),
            value: v.to_vec(),
        }),
    }
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

/// same op-set as the shared-store proof: distinct origins/keys so the FINAL kv
/// CONTENT is permutation-invariant (divergence can only come from LOG ORDER),
/// plus two order-INDEPENDENT directory writes. one op per validator.
fn op_set() -> Vec<(commonware_cryptography::ed25519::PrivateKey, u64, Msg)> {
    vec![
        (op_signer(101), 0, kv_set(b"aaa", b"1")),
        (op_signer(102), 0, kv_set(b"bbb", b"2")),
        (op_signer(103), 0, kv_set(b"ccc", b"3")),
        (op_signer(104), 0, dir_set("x", "10")),
        (op_signer(105), 0, dir_set("y", "20")),
    ]
}

/// random attacker bytes: NOT a valid node frame, unrelated to any op. distinct
/// per `i` so the flood is many distinct content-addresses, not one repeated.
fn garbage_blob(i: usize) -> Vec<u8> {
    format!("BYZANTINE-GARBAGE-{i:08}-poison-the-payload-lane").into_bytes()
}

/// a fixed random blob resent every flood tick, so it reliably lands in every
/// honest store regardless of interleaving — a post-convergence probe target.
fn sentinel_garbage() -> Vec<u8> {
    b"BYZANTINE-SENTINEL-GARBAGE".to_vec()
}

/// the SHARPER attack: a perfectly WELL-FORMED node frame (it would `decode_frame`
/// + `host.submit` cleanly) that the byzantine peer NEVER submitted to consensus.
///   its origin/key (`Z`/`zzz`) is disjoint from every honest op. content-addressing
///   stores it under `digest_of(frame)`, which is never finalized (never proposed),
///   so it is never delivered — isolating the defense from mere frame-validity.
fn unproposed_valid_frame() -> Vec<u8> {
    encode_frame(&op_signer(200), 0, &kv_set(b"zzz", b"999"))
}

/// the N=5 relay convergence scenario. `flood`: when true, validator `BYZ` spawns
/// a task that floods garbage on its payload-channel sender (in ADDITION to its
/// honest relay + votes).
async fn run_relay(mut context: deterministic::Context, flood: bool) {
    let namespace = b"consensus".to_vec();
    let epoch = Epoch::new(333);

    let fixture = simplex_ed25519::fixture(&mut context, &namespace, N as u32);
    let participants = fixture.participants.clone();
    let schemes = fixture.schemes.clone();

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

    // vote(0)/cert(1)/resolver(2) — consumed positionally by engine.start — PLUS
    // the payload channel(3) the relay gossips frame bytes on and the catch-up
    // fetch channel(4) the resolver engine runs on. 3/4 are free: the engine
    // here uses only 0/1/2 (no broadcast channel to collide with).
    let quota = Quota::per_second(NZU32!(128));
    let mut registrations = HashMap::new();
    let mut payload_chans = HashMap::new();
    let mut fetch_chans = HashMap::new();
    for v in participants.iter() {
        let control = oracle.control(v.clone());
        let vote = control.register(0, quota).await.expect("register vote");
        let certificate = control
            .register(1, quota)
            .await
            .expect("register certificate");
        let resolver = control.register(2, quota).await.expect("register resolver");
        let payload = control.register(3, quota).await.expect("register payload");
        let fetch = control.register(4, quota).await.expect("register fetch");
        registrations.insert(v.clone(), (vote, certificate, resolver));
        payload_chans.insert(v.clone(), payload);
        fetch_chans.insert(v.clone(), fetch);
    }

    let link = Link {
        latency: Duration::from_millis(10),
        jitter: Duration::from_millis(1),
        success_rate: commonware_utils::probability!(1.0),
    };
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

    let genesis_floor: Digest = mocks::application::genesis::<Sha256>(epoch);

    // stash a probe clone of every node's per-process store (Arc-backed: the clone
    // shares the backing map the payload drain writes into).
    let mut stores: Vec<ContentStore> = Vec::new();
    let mut nodes: Vec<OrderedNode<SimplexOrderer>> = Vec::new();
    for (idx, v) in participants.iter().enumerate() {
        let host = genesis_host(context.child(LABELS[idx])).await;
        let (vote, certificate, resolver) = registrations.remove(v).expect("validator registered");
        let payload = payload_chans.remove(v).expect("validator payload channel");
        let fetch = fetch_chans.remove(v).expect("validator fetch channel");

        // INJECTION: before the honest wiring consumes the payload tuple, clone the
        // byzantine node's payload SENDER and spawn a flooder on it. this touches
        // ONLY validator BYZ's own outbound sender — the honest nodes and the
        // orderer wiring are untouched.
        if flood && idx == BYZ {
            let mut byz_sender = payload.0.clone();
            context.child("byz_flood").spawn(move |ctx| async move {
                for i in 0..30usize {
                    // (a) a distinct random content-address every tick ...
                    let _ = byz_sender.send(Recipients::All, IoBuf::from(garbage_blob(i)), false);
                    // (b) the resent random sentinel (reliably lands everywhere) ...
                    let _ =
                        byz_sender.send(Recipients::All, IoBuf::from(sentinel_garbage()), false);
                    // (c) a WELL-FORMED frame never proposed to consensus — the
                    // adversarial case a skeptic can't wave off as "wouldn't decode".
                    let _ = byz_sender.send(
                        Recipients::All,
                        IoBuf::from(unproposed_valid_frame()),
                        false,
                    );
                    ctx.sleep(Duration::from_millis(30)).await;
                }
            });
        }

        // PER-PROCESS store: the only path into a peer's store is the relay drain
        // (the resolver fetch lane stays idle while the eager gossip lands).
        let store = ContentStore::new();
        stores.push(store.clone());
        let orderer = SimplexOrderer::spawn_with_resolver(
            context.child("validator"),
            schemes[idx].clone(),
            oracle.control(v.clone()),
            oracle.manager(),
            v.clone(),
            v.to_string(),
            epoch,
            genesis_floor,
            None,
            store,
            CADENCE,
            vote,
            certificate,
            resolver,
            payload,
            fetch,
            false,
        );
        nodes.push(OrderedNode::new(host, orderer));
    }

    let genesis = nodes[0].root_hash();
    for n in &nodes {
        assert_eq!(
            n.root_hash(),
            genesis,
            "identical genesis -> identical root-hash"
        );
    }
    let genesis_kv = nodes[0].host().module_root("kv").unwrap();
    // the order-INDEPENDENT directory root the two legit dir writes MUST produce,
    // computed locally with NO consensus — the "only legit ops landed" anchor.
    let expected_dir = {
        let mut h = genesis_host(context.child("baseline")).await;
        h.submit(dir_set("x", "10")).await.expect("baseline dir x");
        h.submit(dir_set("y", "20")).await.expect("baseline dir y");
        h.module_root("directory").unwrap()
    };

    let ops = op_set();
    for (i, (origin, seq, msg)) in ops.iter().enumerate() {
        nodes[i]
            .submit(origin, *seq, msg.clone())
            .await
            .expect("submit");
    }
    for n in &nodes {
        assert_eq!(
            n.root_hash(),
            genesis,
            "no optimistic echo: submit does not advance root-hash"
        );
    }

    // PUMP to convergence: stop only when EVERY node has applied the whole op-set.
    let target = ops.len();
    let mut applied = [0usize; N];
    loop {
        context.sleep(Duration::from_millis(50)).await;
        for (i, n) in nodes.iter_mut().enumerate() {
            // NOTE: a flooded blob that ever leaked into DELIVERY (instead of
            // store-only) reaches `decode_frame` here: random bytes Err out (this
            // `.expect` panics) and the well-formed frame would inflate `applied`
            // past `target` (asserted below). store-only is enforced, not assumed.
            // the production run loop flushes pending ops event-driven (bin/node
            // `pump_eager_flush`); enqueue-only submits never propose without a flush —
            // the sim drives that flush on its own tick (a no-op when nothing is pending).
            n.flush_batch().await.expect("flush");
            applied[i] += n.drain_delivered().await.expect("drain");
        }
        if applied.iter().all(|&c| c == target) {
            break;
        }
    }

    // ---- convergence: identical CORRECT state on every validator ----
    let converged = nodes[0].root_hash();
    let converged_kv = nodes[0].host().module_root("kv").unwrap();
    assert_ne!(
        converged, genesis,
        "the finalized ops moved the root-hash off genesis"
    );
    assert_ne!(converged_kv, genesis_kv, "the qmdb root moved off genesis");
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(
            applied[i], target,
            "validator {i} applied EXACTLY the finalized ops (nothing flooded entered delivery)"
        );
        assert_eq!(
            n.root_hash(),
            converged,
            "validator {i} converges on the identical root-hash"
        );
        assert_eq!(
            n.host().module_root("kv").unwrap(),
            converged_kv,
            "validator {i} converges on the identical qmdb (order-DEPENDENT) root"
        );
        // order-INDEPENDENT anchor: the directory reflects EXACTLY the 2 legit dir
        // writes — rules out "agreed but wrong" (cross-node root_hash equality alone
        // cannot: all nodes could agree on a poisoned root).
        assert_eq!(
            n.host().module_root("directory").unwrap(),
            expected_dir,
            "validator {i} directory root reflects only the legit dir writes"
        );
    }

    if !flood {
        return;
    }

    // ---- content-addressing inertness: every flooded blob IS in honest stores,
    // keyed under its OWN sha256, so none can resolve a finalized digest ----
    //
    // sequence the store probe AFTER convergence: pump until BOTH the random
    // sentinel and the well-formed-but-unproposed frame are observable in every
    // honest store (bounded; the timed runner backstops a genuine no-show).
    let sentinel_digest = digest_of(&sentinel_garbage());
    let valid_frame_digest = digest_of(&unproposed_valid_frame());
    let honest: Vec<usize> = (0..N).filter(|&i| i != BYZ).collect();
    let mut landed = false;
    for _ in 0..200 {
        if honest.iter().all(|&i| {
            stores[i].get(&sentinel_digest).is_some()
                && stores[i].get(&valid_frame_digest).is_some()
        }) {
            landed = true;
            break;
        }
        context.sleep(Duration::from_millis(50)).await;
    }
    assert!(
        landed,
        "byzantine garbage + unproposed frame reached every honest store via the payload lane"
    );

    for &i in &honest {
        // random bytes: keyed by their OWN content-address (re-hash on put).
        assert_eq!(
            stores[i].get(&sentinel_digest),
            Some(sentinel_garbage()),
            "validator {i}: random garbage keyed by digest_of(garbage), inert"
        );
        // the WELL-FORMED frame: present + would decode/apply cleanly, yet it is
        // NOT in committed state (converged root-hash is correct, `applied == target`
        // above). the ONLY reason it never applied is that its digest was never
        // FINALIZED — content-addressing + finalization-gating, not frame-validity.
        assert_eq!(
            stores[i].get(&valid_frame_digest),
            Some(unproposed_valid_frame()),
            "validator {i}: unproposed valid frame keyed by its own digest, never delivered"
        );
        // inert for MEMORY too: the flood never outweighs the cache's byte cap.
        assert!(
            stores[i].cached_bytes() <= PAYLOAD_CACHE_BYTES,
            "validator {i}: the payload cache outgrew its byte cap under the flood"
        );
    }
}

#[test]
fn relay_path_converges_including_qmdb_root() {
    deterministic::Runner::timed(Duration::from_secs(300))
        .start(|context| run_relay(context, false));
}

#[test]
fn byzantine_payload_flood_is_inert() {
    deterministic::Runner::timed(Duration::from_secs(300))
        .start(|context| run_relay(context, true));
}

#[test]
fn byzantine_flood_tolerance_is_robust_across_schedules() {
    // the flood tolerance must not depend on one lucky task-interleaving: finalize
    // the honest ops + absorb the garbage flood under several distinct seeds.
    for seed in [1u64, 7, 99, 2718] {
        let cfg = deterministic::Config::default()
            .with_seed(seed)
            .with_timeout(Some(Duration::from_secs(300)));
        deterministic::Runner::new(cfg).start(|context| run_relay(context, true));
    }
}

/// a deterministic dev signer for test frames (any u64 seed).
fn op_signer(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}
