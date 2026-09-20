//! invite-after-genesis over real sockets: an UNINVITED key becomes a live
//! validator with zero config edits and zero member restarts.
//!
//! the flow under test (two humans, three commands):
//!   friend starts an out-of-mesh node        -> it parks, refused by the mesh
//!   a member grants resident standing, then runs `promote <pubkey>` ->
//!                                      governance passes, valset Join
//!   the epoch cutover re-tracks the mesh     -> the parked node syncs at the
//!                                                boundary, fabricates its
//!                                                recovery checkpoint, reboots,
//!                                                and votes in the new epoch
//!
//! two scenarios, two distinct promotion paths:
//! - `solo_founder_invites_a_friend`: n=1 -> 2. resident admission crosses one
//!   epoch before promotion; the promoted seat then freezes the next boundary
//!   at the 2-of-2 quorum floor.
//! - `live_quorum_admits_a_fourth_validator`: n=3 -> 4. the three incumbents
//!   keep finalizing past the cutover (quorum 3 of 4), so the joiner syncs a
//!   mid-epoch boundary and needs the served finalization floor certificate.

mod common;

use std::time::Duration;

use common::Cluster;
use common::wire::tasks::{
    TaskMsg, TaskQuery, TaskReply, decode_task_reply, encode_task_msg, encode_task_query,
};

/// convergence budget: mesh formation + leader rotation are real-time on a
/// possibly-loaded CI core; polls exit early, so generosity is free.
const CONVERGE: Duration = Duration::from_secs(180);
/// budget for one submitted op to finalize and become readable elsewhere.
const FINALIZE: Duration = Duration::from_secs(60);

/// a create for `task_id`; NOT an upsert — `tasks` refuses a duplicate id
/// (`task_board.rs:77-80`). That rejection is ISOLATED, not fatal: the op's
/// stage rolls back and it is recorded `Rejected` while the block still seals
/// (`host/src/lib.rs:237`, `:283`). So a duplicate never applies and never
/// announces itself — `write_and_confirm` spins to its timeout, or passes
/// VACUOUSLY when the re-created title happens to match the surviving one.
/// Every call site here has to carry a fresh id.
fn task_create(task_id: &str, title: &str) -> Vec<u8> {
    encode_task_msg(&TaskMsg::CreateTask {
        task_id: task_id.into(),
        title: title.into(),
        owner: None,
    })
}

fn task_title(cluster: &Cluster, idx: usize, task_id: &str) -> Option<String> {
    let req = encode_task_query(&TaskQuery::Get {
        task_id: task_id.into(),
    });
    let reply = cluster.query(idx, "tasks", &req)?;
    match decode_task_reply(&reply) {
        Ok(TaskReply::Task(task)) => task.map(|t| t.title),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    common::hex(bytes)
}

#[test]
fn solo_founder_invites_a_friend() {
    // a network of ONE: the founder is mesh and quorum all by itself.
    let mut cluster = Cluster::new(&[0], &[0]);
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.wait_marker(0, "converged root_hash=", CONVERGE);

    // the friend starts an out-of-mesh node: it must PARK (the founder's
    // tracked set does not contain this key), not sync and not crash.
    let joiner = cluster.spawn_joiner(1);
    cluster.wait_marker(joiner, "joiner mode:", Duration::from_secs(60));
    cluster.wait_marker(joiner, "joining:", Duration::from_secs(60));

    // A joiner must first hold resident standing: promotion is deliberately
    // gated on the staged-admission tier so a new quorum seat is caught up.
    let friend_hex = hex(&Cluster::identity(1));
    let cfg = cluster.config_file(0);
    let (ok, out) = cluster.run_verb(&[
        "node",
        "resident",
        "accept",
        &friend_hex,
        "--config",
        cfg.to_str().expect("utf-8 config path"),
    ]);
    assert!(ok, "resident accept failed:\n{out}");
    assert!(
        out.contains("granted resident standing"),
        "unexpected verb output:\n{out}"
    );
    cluster.wait_marker(joiner, "resident: pre-synced boundary", CONVERGE);

    // One command on the founder's node: propose + the deciding solo ballot
    // + execute, all through the running node's rpc.
    let (ok, out) = cluster.run_verb(&[
        "node",
        "member",
        "promote",
        &friend_hex,
        "--config",
        cfg.to_str().expect("utf-8 config path"),
    ]);
    assert!(ok, "promote failed:\n{out}");
    assert!(out.contains("admitted"), "unexpected verb output:\n{out}");

    // the cutover (the nop pusher advances the views) seats the friend after
    // the resident-admission epoch has already crossed.
    cluster.wait_marker(0, "cutover complete: epoch 1", CONVERGE);

    // the parked node notices its admission, syncs the boundary, and SEATS
    // ITSELF IN-PROCESS from its own folded state — it does not reboot.
    //
    // So there is no `recovered root_hash=` to wait for: that line is emitted
    // only by the validator BOOT path (`bin/node/src/validator/boot.rs`), and a
    // warm promotion returns a `PromotionBaton` already carrying the recovery
    // and the root hash (`replica/park.rs`). Waiting for it here could only
    // ever hang — it did, for 600 s. `restart_e2e` still waits on that marker,
    // correctly, because a real process restart does go through boot.
    cluster.wait_marker(joiner, "admitted at epoch", CONVERGE);
    cluster.wait_marker(joiner, "synced root_hash=", CONVERGE);
    cluster.wait_marker(joiner, "promoted: validator at epoch", CONVERGE);

    // THE property: consensus is live again, and only because the friend
    // votes — a 2-validator simplex finalizes nothing without both. an op
    // submitted via the FOUNDER must become readable via the FRIEND...
    cluster.submit(0, "tasks", &task_create("from-founder", "hello"));
    let value = cluster.await_committed(
        joiner,
        "founder's op to read on the friend",
        FINALIZE,
        || task_title(&cluster, joiner, "from-founder"),
    );
    assert_eq!(value, "hello");

    // ...and an op submitted via the FRIEND (whose bytes only the friend
    // holds until the relay lane gossips them) must read on the founder.
    cluster.submit(joiner, "tasks", &task_create("from-friend", "hi back"));
    let value = cluster.await_committed(0, "friend's op to read on the founder", FINALIZE, || {
        task_title(&cluster, 0, "from-friend")
    });
    assert_eq!(value, "hi back");

    // no fork: identical status root-hashes once both sides quiesce. a real
    // fork never reconciles, so it fails this wait's budget.
    cluster.await_committed(
        0,
        "founder and promoted friend to agree on state",
        FINALIZE,
        || {
            let founder = cluster.status(0)["root_hash"].clone();
            let agreed = !founder.is_null() && founder == cluster.status(joiner)["root_hash"];
            agreed.then_some(())
        },
    );
}

/// the PROMOTION twin of `cluster_e2e::reachability_plane_converges_mesh_on_boot`.
///
/// A fresh-booting validator targets its reachability plane at the epoch it
/// booted into. The in-process promotion seat wires a BRAND NEW plane — the
/// parked node's standby plane is shut down first, deliberately — and for a
/// while it wired that plane and never targeted it. A plane with no epoch state
/// is a black hole in BOTH directions: it drops every inbound record and advert
/// and sends none of its own, so phase-A assembly never completes on either
/// side and the promoted node keeps only the pre-warm tunnels it installed
/// while parked. On a live three-node network that read as a healthy chain from
/// the founder alone, while every op submitted at a promoted joiner timed out
/// awaiting finalization.
///
/// Nothing in the tree caught it: every promotion e2e runs on the harness's
/// `wireguard = false` default, where the plane does not exist at all. Hence
/// this one, which is the same flow with the plane turned on.
#[test]
fn a_promoted_validator_converges_the_overlay_mesh() {
    // two founding validators, so the promoted joiner has to mesh with a set
    // it never met — the live shape, and the one a single founder cannot test.
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    // the plane exists only with wireguard on. This line IS the regression.
    cluster.wireguard = true;
    // hermetic: without this every node dials the LIVE public coordinator
    // (`default_primary_coordinator()`) from inside the test.
    cluster
        .extra_toml
        .push("primary_coordinator = \"none\"".into());
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.spawn(1);
    for i in 0..2 {
        cluster.wait_marker(i, "converged root_hash=", CONVERGE);
    }

    let joiner = cluster.spawn_joiner(2);
    cluster.wait_marker(joiner, "joining:", Duration::from_secs(60));

    // strict majority of 2 is 2: both incumbents grant resident standing;
    // the second ballot decides and executes.
    let friend_hex = hex(&Cluster::identity(2));
    for member in [0usize, 1] {
        let cfg = cluster.config_file(member);
        let (ok, out) = cluster.run_verb(&[
            "node",
            "resident",
            "accept",
            &friend_hex,
            "--config",
            cfg.to_str().expect("utf-8 config path"),
        ]);
        assert!(ok, "resident accept via member {member} failed:\n{out}");
    }
    cluster.wait_marker(joiner, "resident: pre-synced boundary", CONVERGE);
    for member in [0usize, 1] {
        let cfg = cluster.config_file(member);
        let (ok, out) = cluster.run_verb(&[
            "node",
            "member",
            "promote",
            &friend_hex,
            "--config",
            cfg.to_str().expect("utf-8 config path"),
        ]);
        assert!(ok, "promote via member {member} failed:\n{out}");
    }

    for i in 0..2 {
        cluster.wait_marker(i, "cutover complete: epoch 2", CONVERGE);
    }
    cluster.wait_marker(joiner, "promoted: validator at epoch 2", CONVERGE);

    // THE property, and the one the seat's missing `Retarget` broke: the
    // promoted node's plane knows its epoch, so it sends its own endpoint
    // record, completes assembly, and installs MEMBER tunnels — not the
    // standby pre-warm set it carried in with.
    cluster.wait_marker(joiner, "mesh verified", CONVERGE);
    cluster.wait_marker(joiner, "tunnels applied (config accepted", CONVERGE);
}

#[test]
fn live_quorum_admits_a_fourth_validator() {
    // three validators — enough that the mesh KEEPS finalizing through the
    // whole admission (quorum(4) = 3), forcing the mid-epoch joiner path.
    let mut cluster = Cluster::new(&[0, 1, 2], &[0, 1, 2]);
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.spawn(1);
    cluster.spawn(2);
    for i in 0..3 {
        cluster.wait_marker(i, "converged root_hash=", CONVERGE);
    }

    let joiner = cluster.spawn_joiner(3);
    cluster.wait_marker(joiner, "joiner mode:", Duration::from_secs(60));

    // strict majority of 3 is 2: members 0 and 1 each grant resident
    // standing, then each run the promotion command; the second ballot in
    // each ceremony decides and executes.
    let friend_hex = hex(&Cluster::identity(3));
    for member in [0usize, 1] {
        let cfg = cluster.config_file(member);
        let (ok, out) = cluster.run_verb(&[
            "node",
            "resident",
            "accept",
            &friend_hex,
            "--config",
            cfg.to_str().expect("utf-8 config path"),
        ]);
        assert!(ok, "resident accept via member {member} failed:\n{out}");
    }
    cluster.wait_marker(joiner, "resident: pre-synced boundary", CONVERGE);
    for member in [0usize, 1] {
        let cfg = cluster.config_file(member);
        let (ok, out) = cluster.run_verb(&[
            "node",
            "member",
            "promote",
            &friend_hex,
            "--config",
            cfg.to_str().expect("utf-8 config path"),
        ]);
        assert!(ok, "promote via member {member} failed:\n{out}");
    }

    // staged admission: the promotion cutover seats the already-synced
    // resident on every incumbent.
    for i in 0..3 {
        cluster.wait_marker(i, "cutover complete: epoch 2", CONVERGE);
    }

    // advance the boundary PAST the epoch base while the joiner is still
    // syncing-or-parked: quorum(4) = 3, so the incumbents finalize without
    // it and the joiner must adopt a mid-epoch boundary plus its
    // finalization floor.
    for n in 0..5 {
        cluster.submit(0, "tasks", &task_create(&format!("epoch2-op-{n}"), "x"));
        let _ = cluster.await_committed(1, "epoch-2 filler to finalize", FINALIZE, || {
            task_title(&cluster, 1, &format!("epoch2-op-{n}"))
        });
    }

    // in-process seating again — no reboot, so no `recovered root_hash=`.
    cluster.wait_marker(joiner, "admitted at epoch 2", CONVERGE);
    cluster.wait_marker(joiner, "synced root_hash=", CONVERGE);
    cluster.wait_marker(joiner, "promoted: validator at epoch 2", CONVERGE);

    // the promoted validator's own op finalizes and reads on an incumbent —
    // its frame bytes start out ONLY in its store, so this proves the joiner
    // is wired into the payload relay and the vote lanes of the live epoch.
    cluster.submit(joiner, "tasks", &task_create("from-the-fourth", "present"));
    let value = cluster.await_committed(
        2,
        "the fourth validator's op to read on node 2",
        FINALIZE,
        || task_title(&cluster, 2, "from-the-fourth"),
    );
    assert_eq!(value, "present");

    // and it holds the full replicated state (no fork after quiesce).
    cluster.await_committed(
        0,
        "incumbent and promoted validator to agree on state",
        FINALIZE,
        || {
            let incumbent = cluster.status(0)["root_hash"].clone();
            let agreed = !incumbent.is_null() && incumbent == cluster.status(joiner)["root_hash"];
            agreed.then_some(())
        },
    );
}
