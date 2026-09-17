//! a module registered after genesis, live-swapped, then carried across a
//! crash restart and to a statesynced joiner — the acceptance test for
//! "admitted modules across restart and statesync".
mod common;

use std::time::Duration;

use common::Cluster;
use common::module_verbs::{
    AFTER, active_hash, assert_ceremony_scheduled, fixture, run_on_each, sha256_hex, spawn_founders,
};
use governance::{
    GovAction, GovMsg, GovQuery, GovReply, ProposalStatus, decode_reply, encode_msg, encode_query,
};

/// a query or status read that should already be true lands within a block
/// or two; this is the budget for a ws-block-fed wait on it.
const FINALIZE: Duration = Duration::from_secs(60);
/// a swap activates `AFTER` idle blocks after the deciding ballot.
const ACTIVATE: Duration = Duration::from_secs(180);

/// three founders (3-of-3) plus a DECLARED fourth peer (idx 3 = id 4) that
/// is started later with `cluster.spawn(3)`: statesync is fail-closed for a
/// peer with no committed standing, so the joiner must exist in the cluster
/// layout from the start — and `Cluster::spawn_joiner` keys on the ID and
/// appends it to `peer_ids`, so it would declare id 4 twice.
fn founders_and_declared_joiner() -> Cluster {
    let mut cluster = Cluster::new(&[1, 2, 3, 4], &[1, 2, 3]);
    // every sealed block stays in the journal-replay window: the restart
    // must REPLAY the register, the swap and both incs, not restore hello
    // from a checkpoint that happened to land after them.
    cluster.extra_toml.push("checkpoint_blocks = 100000".into());
    spawn_founders(cluster)
}

/// hello's query is "any bytes → the counter as LE u64"; `None` while the
/// node cannot answer for it yet (not serving, or the module not active).
fn count(cluster: &Cluster, idx: usize) -> Option<u64> {
    let reply = cluster.query(idx, "hello", b"")?;
    let bytes: [u8; 8] = reply.get(..8)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// the composite root every listed node reports, when they all agree.
fn root_hashes_agree(cluster: &Cluster, idxs: &[usize]) -> Option<String> {
    let hashes: Vec<String> = idxs
        .iter()
        .map(|&idx| {
            cluster.status(idx)["root_hash"]
                .as_str()
                .map(str::to_string)
        })
        .collect::<Option<_>>()?;
    let first = hashes.first()?;
    let all_same = hashes.iter().all(|h| h == first);
    all_same.then(|| first.clone())
}

/// `inc` on one node, the new count readable on every founder.
fn inc_and_confirm(cluster: &Cluster, submit_on: usize, expect: u64) {
    cluster.submit(submit_on, "hello", b"inc");
    for idx in 0..3 {
        let seen = cluster.await_committed(
            idx,
            &format!("hello count == {expect} on node {idx}"),
            FINALIZE,
            || count(cluster, idx).filter(|c| *c == expect),
        );
        assert_eq!(seen, expect);
    }
}

/// governance's view of proposal `id` via node `idx`: (status, ballots cast);
/// `None` until visible.
fn proposal_status(cluster: &Cluster, idx: usize, id: &str) -> Option<(ProposalStatus, usize)> {
    let reply = cluster.query(
        idx,
        "governance",
        &encode_query(&GovQuery::Proposal {
            proposal_id: id.into(),
        }),
    )?;
    match decode_reply(&reply) {
        Ok(GovReply::Proposal(Some(view))) => Some((view.status, view.votes.len())),
        _ => None,
    }
}

/// node 0 proposes `action`, nodes 0+1 vote (2 of 3 = majority), node 1
/// executes; returns once the proposal has settled Passed on node 0.
fn pass_proposal(cluster: &Cluster, id: &str, action: GovAction) {
    cluster.submit(
        0,
        "governance",
        &encode_msg(&GovMsg::Propose {
            proposal_id: id.into(),
            action,
            voting_period: 600_000,
        }),
    );
    cluster.await_committed(1, "proposal to open", FINALIZE, || {
        proposal_status(cluster, 1, id).filter(|(s, _)| *s == ProposalStatus::Open)
    });
    let vote = encode_msg(&GovMsg::Vote {
        proposal_id: id.into(),
        approve: true,
    });
    cluster.submit(0, "governance", &vote);
    cluster.submit(1, "governance", &vote);
    cluster.await_committed(1, "both ballots to land", FINALIZE, || {
        proposal_status(cluster, 1, id).filter(|(_, votes)| *votes == 2)
    });
    cluster.submit(
        1,
        "governance",
        &encode_msg(&GovMsg::Execute {
            proposal_id: id.into(),
        }),
    );
    cluster.await_committed(0, "the proposal to settle as Passed", FINALIZE, || {
        proposal_status(cluster, 0, id).filter(|(s, _)| *s == ProposalStatus::Passed)
    });
}

/// stage `key` in as a resident and then seat it, the way a live network does:
/// a joiner redeems its invite into RESIDENT standing and is promoted out of
/// that tier, so a promotion names a key the network has already met (#2507).
/// The passing promotion emits the valset Join, and the founders cross the
/// epoch-1 cutover on their own idle blocks.
fn admit_validator(cluster: &Cluster, key: Vec<u8>) {
    pass_proposal(
        cluster,
        "stand-joiner",
        GovAction::AddResident { key: key.clone() },
    );
    pass_proposal(cluster, "admit-joiner", GovAction::AddValidator { key });
    cluster.await_committed(0, "the epoch-1 cutover on every founder", ACTIVATE, || {
        let every_founder_cut_over =
            (0..3).all(|idx| cluster.marker(idx, "cutover complete: epoch 1").is_some());
        every_founder_cut_over.then_some(())
    });
}

fn register_and_activate(cluster: &Cluster) {
    let runs = run_on_each(
        cluster,
        &[
            "module",
            "register",
            "hello",
            &fixture("hello"),
            "--after",
            AFTER,
        ],
    );
    assert_ceremony_scheduled(&runs, "hello");
    let first = sha256_hex(&fixture("hello"));
    for idx in 0..3 {
        let seen = cluster.await_committed(idx, "hello active", ACTIVATE, || {
            active_hash(cluster, idx, "hello").filter(|h| *h == first)
        });
        assert_eq!(seen, first);
    }
}

fn update_and_activate(cluster: &Cluster) {
    let runs = run_on_each(
        cluster,
        &[
            "module",
            "update",
            "hello",
            &fixture("hello-replacement"),
            "--after",
            AFTER,
        ],
    );
    assert_ceremony_scheduled(&runs, "hello");
    let second = sha256_hex(&fixture("hello-replacement"));
    for idx in 0..3 {
        let seen = cluster.await_committed(idx, "hello swapped", ACTIVATE, || {
            active_hash(cluster, idx, "hello").filter(|h| *h == second)
        });
        assert_eq!(seen, second);
    }
}

#[test]
fn a_registered_module_survives_a_live_swap_a_restart_and_statesync() {
    let mut cluster = founders_and_declared_joiner();

    // 2. register hello on all three; 3. inc → 1 everywhere
    register_and_activate(&cluster);
    inc_and_confirm(&cluster, 1, 1);

    // 4. swap in the replacement (steps by 100); 5. inc → 101 everywhere,
    // and the composite root agrees across the founders
    update_and_activate(&cluster);
    inc_and_confirm(&cluster, 1, 101);
    let root_after_swap = cluster.await_committed(
        0,
        "founders' root-hashes to agree after the swap",
        FINALIZE,
        || root_hashes_agree(&cluster, &[0, 1, 2]),
    );

    // 6. crash node 2 (SIGKILL) and respawn it over the same storage. with
    // 3-of-3 the chain halts while it is down and resumes when it is back.
    // the boot must RECOVER — replaying hello's register, swap and both
    // incs from the journal — not re-run genesis.
    cluster.kill(2);
    cluster.spawn(2);
    let recovered = cluster.wait_marker(2, "recovered root_hash=", Duration::from_secs(120));
    println!("node 2 recovered root_hash={recovered}");
    let recovered_hash = recovered.split_whitespace().next().expect("recovered hash");
    assert_eq!(
        recovered_hash, root_after_swap,
        "node 2 recovered a different root than the founders' post-swap boundary"
    );
    assert!(
        cluster.marker(2, "genesis root_hash=").is_none(),
        "a restart must not re-run genesis"
    );
    // `recovered` is printed mid-boot: the engine still has the rest of the
    // journal to resume, and the rpc listener binds after it. the node's own
    // bind line is the readiness event — without it the first query races a
    // node that is still replaying, and the race is won or lost by how many
    // blocks the chain happened to seal.
    cluster.wait_marker(2, "rpc listening on", Duration::from_secs(120));
    let seen = cluster.await_committed(2, "hello count == 101 after restart", FINALIZE, || {
        count(&cluster, 2).filter(|c| *c == 101)
    });
    assert_eq!(seen, 101);
    let root_after_restart = cluster.await_committed(
        0,
        "founders' root-hashes to agree after the restart",
        FINALIZE,
        || root_hashes_agree(&cluster, &[0, 1, 2]),
    );
    assert_eq!(
        root_after_restart, root_after_swap,
        "a restart moved the state root"
    );

    // 7. seat the declared joiner, then let it statesync as a fresh resident:
    // every module — hello included, whose bytes it can only pull over the
    // blob plane — must compose the founders' root. the ceremony moved
    // governance/valset state, so the joiner is held to the POST-admission
    // root, not `root_after_restart`.
    admit_validator(&cluster, Cluster::identity(4));
    let root_before_sync = cluster.await_committed(
        0,
        "founders' root-hashes to agree before the sync",
        FINALIZE,
        || root_hashes_agree(&cluster, &[0, 1, 2]),
    );
    let (ok, log) = cluster.run_sync_only(3, Duration::from_secs(180));
    assert!(ok, "sync-only joiner failed:\n{log}");
    let synced = log
        .lines()
        .find_map(|l| l.split("synced root_hash=").nth(1))
        .expect("joiner printed a synced root-hash")
        .trim();
    println!("sync-only joiner synced root_hash={synced}");
    assert_eq!(
        synced, root_before_sync,
        "joiner composed a DIFFERENT root-hash"
    );

    // the joiner boots LIVE over the synced storage (a sync-only run binds
    // no rpc). a non-genesis key always enters the replica park: it syncs
    // the epoch-1 boundary, finds itself seated, and promotes in-process —
    // then hello answers 101 from state it never executed. the count is
    // probed on the founders' block feed: node 3 answers `None` until it
    // serves, and the chain's own blocks are the wait seam either way.
    cluster.spawn(3);
    let promoted = cluster.wait_marker(3, "promoted: validator at epoch 1", ACTIVATE);
    println!("node 3 promoted: validator at epoch 1 {promoted}");
    let live_synced = cluster.marker(3, "synced root_hash=");
    println!("node 3 live boot synced root_hash={live_synced:?}");
    assert_eq!(
        live_synced.as_deref(),
        Some(synced),
        "the seated joiner re-synced a different root"
    );
    let seen = cluster.await_committed(0, "hello count == 101 on the joiner", ACTIVATE, || {
        count(&cluster, 3).filter(|c| *c == 101)
    });
    assert_eq!(seen, 101);
    let root_with_joiner =
        cluster.await_committed(0, "all four root-hashes to agree", FINALIZE, || {
            root_hashes_agree(&cluster, &[0, 1, 2, 3])
        });
    assert_eq!(root_with_joiner, root_before_sync);
}
