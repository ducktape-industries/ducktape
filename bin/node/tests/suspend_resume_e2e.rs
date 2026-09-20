//! a suspended follower resumes following, end to end: freeze the resident
//! (SIGSTOP) for longer than the mesh read/write deadline, thaw it, and
//! require a post finalized AFTER the thaw to reach its local reads within a
//! bounded window.
//!
//! this is the laptop-sleep regression net for the desktop node. a slept
//! machine leaves exactly this shape behind: the frozen node goes silent, so
//! the peer's next read runs into `MESH_IO_TIMEOUT` (`overlay_net::userspace
//! ::seam::IO_TIMEOUT`) and tears the connection down; on wake the follower
//! holds half-open sockets and must heal through teardown → redial →
//! catch-up. `FREEZE` is deliberately built ABOVE that deadline (deadline +
//! slack) so the founder-side teardown really happens — under the old 60s
//! default the founder would still be holding the dead-quiet connection when
//! the resident thaws. (linux cannot fake the one macOS-only aggravation —
//! `CLOCK_UPTIME_RAW` pausing across sleep, which makes the wakened node
//! itself burn its full residual deadline — so the bound on that side is the
//! constant itself, asserted where it is defined.)
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test suspend_resume_e2e -- --nocapture --test-threads=1

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::NetworkShapeCluster;
use common::wire::chat::{
    Block, ChatMsg, ChatQuery, ChatReply, PostPolicy, decode_reply, encode_msg, encode_query,
};

/// generous like the sibling legs: standing → follow-arm sync → first
/// pre-synced boundary is several blocks of slack.
const CONVERGE: Duration = Duration::from_secs(180);

/// slack above the founder's read deadline: enough that the deadline firing
/// mid-freeze is never a photo finish on a slow box.
const FREEZE_SLACK: Duration = Duration::from_secs(10);

/// freeze long enough that the founder's read deadline
/// (`overlay_net::userspace::seam::IO_TIMEOUT`, the mesh's only half-open
/// detector) fires mid-freeze and tears the resident's connections down —
/// the slept-laptop shape. derived from that constant plus slack, not a
/// restated literal, so the two can never drift apart.
const FREEZE: Duration = Duration::from_secs(
    overlay_net::userspace::seam::IO_TIMEOUT.as_secs() + FREEZE_SLACK.as_secs(),
);

/// the thaw-to-caught-up bound. healing is deadline-driven: the resident's
/// own dead reads fail fast (its sockets got FIN/RST while frozen), the
/// dialer re-dials at 500ms cadence, and the follow loop backfills. 45s is
/// several times the worst honest path (one 15s residual deadline + redial +
/// backfill) while staying far from the minutes-long stall this test exists
/// to catch.
const RECOVER: Duration = Duration::from_secs(45);

/// the founder's sync-retention lease, READ OUT OF ITS OWN SOURCE.
///
/// `SYNC_LEASE_SECS` is `pub(crate)` in `sync::serve` and node-bin has no lib
/// target, so an integration test cannot import it — and restating the number
/// is the drift this file already refuses to accept for `IO_TIMEOUT`. Parsing
/// the declaration keeps the two welded: `include_str!` fails the BUILD if the
/// file moves, and the parse fails the TEST if the declaration is reshaped, so
/// the day someone retunes the lease is the day this test says its freeze is
/// no longer "past the lease".
fn sync_lease_secs() -> u64 {
    let src = include_str!("../src/sync/serve.rs");
    let decl = src
        .lines()
        .find(|line| line.contains("const SYNC_LEASE_SECS"))
        .expect("sync::serve still declares SYNC_LEASE_SECS");
    decl.rsplit('=')
        .next()
        .map(|rhs| rhs.trim().trim_end_matches(';'))
        .and_then(|secs| secs.parse().ok())
        .unwrap_or_else(|| panic!("SYNC_LEASE_SECS is no longer a plain literal: {decl:?}"))
}

/// slack above the lease, so the founder's prune hold has demonstrably
/// lapsed before the thaw even on a box whose clock is under load.
const PAST_LEASE_SLACK: Duration = Duration::from_secs(20);

/// the thaw-to-converged bound for the RE-BOOTSTRAP path. deliberately far
/// above [`RECOVER`]: this leg does not heal by catch-up at all — the founder
/// refuses the follower's range, the follower descends, installs a fresh
/// manifest boundary and folds its suffix. measured at ~41s on a loaded
/// 24-core box, so this is several times the honest path.
const RECOVER_PAST_LEASE: Duration = Duration::from_secs(180);

#[test]
fn a_suspended_resident_resumes_following_within_the_deadline_budget() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("suspend-resume");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // an Open room so the founder's later posts need no chat membership.
    cluster.submit(
        0,
        "chat",
        &encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "general".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    cluster.await_committed(
        0,
        "the channel to finalize on the founder",
        CONVERGE,
        || {
            let raw = cluster.query(
                0,
                "chat",
                &encode_query(&ChatQuery::Channel {
                    channel_id: "general".into(),
                }),
            )?;
            matches!(decode_reply(&raw).ok()?, ChatReply::Channel(Some(_))).then_some(())
        },
    );

    // invite + join a fresh identity; grant it RESIDENT standing and wait for
    // the first pre-synced boundary — the follow loop is live from here on.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_admitted(1, CONVERGE);
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // prove the follow loop is live before the freeze.
    cluster.submit(0, "chat", &encode_msg(&post("m-live", "pre-freeze post")));
    resident_sees(&cluster, "m-live", "the pre-freeze adoption", CONVERGE);

    // ---- sleep the laptop: freeze the resident while the chain advances.
    cluster.signal(1, "STOP");
    std::thread::sleep(FREEZE);
    cluster.signal(1, "CONT");

    // ---- the point: a post finalized only after the thaw must land in the
    // resident's local reads inside the deadline-driven healing budget.
    let thawed = Instant::now();
    cluster.submit(0, "chat", &encode_msg(&post("m-thaw", "post-thaw post")));
    resident_sees(&cluster, "m-thaw", "the post-thaw adoption", RECOVER);
    println!(
        "resident healed and adopted the post-thaw boundary in {:?}",
        thawed.elapsed()
    );
    // NOTE: this used to also insist the heal went through the
    // fresh-boundary re-sync — the manifest-proxy retention floor refused a
    // ~25-frame gap even though the journal still held it. the floor is
    // honest now (the journal's own first retained block), so a freeze this
    // short heals by DIRECT frame catch-up and no re-sync is needed. the
    // RangePruned branch keeps its pins where the gap is real: the recovery
    // contract test (range_read_refuses_below_the_retained_floor) and
    // busy_chain_ascension_e2e's restart against a genuinely-outrun window.
    cluster.kill(1);
    cluster.kill(0);
}

/// the same freeze, run PAST the founder's sync-retention lease — the leg the
/// test above cannot reach.
///
/// `FREEZE` is shorter than `SYNC_LEASE_SECS`, so there the founder's prune
/// hold never lapses: it keeps every frame the follower asks for and the
/// follower heals by DIRECT catch-up (the note above says so). Freeze longer
/// than the lease and the hold expires — the follower has stopped asking, so
/// nothing renews it. The founder then prunes past the follower's height,
/// REFUSES its frame range by name, and the only way back is a re-bootstrap
/// at a fresh boundary. Nothing covered that path.
///
/// THE CHAIN MUST BE BUSY or none of it happens. An idle chain seals nop
/// blocks whose root never moves; the checkpoint has nothing to capture and
/// the oplog never prunes, so the floor never arms however long the freeze
/// runs. (Measured: 5810 idle blocks produced 2 checkpoints, both pruning
/// nothing, and the follower sailed back by ordinary catch-up — a green run
/// that proves nothing.) Hence the write pump, as in
/// `busy_chain_ascension_e2e`.
#[test]
fn a_resident_frozen_past_the_sync_lease_rebootstraps_to_the_founders_root() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("past-lease");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );

    // tighten the checkpoint cadence so the oplog prune actually runs while
    // the follower is frozen. every node.toml key is required and emitted, so
    // REWRITE the line — a duplicate TOML key is a founder boot FATAL.
    let founder_toml = cluster.config_file(0);
    let cfg = std::fs::read_to_string(&founder_toml).expect("read founder node.toml");
    assert!(
        cfg.contains("checkpoint_blocks"),
        "generated founder node.toml lost its checkpoint_blocks line"
    );
    let cfg = cfg
        .lines()
        .map(|line| {
            if line.starts_with("checkpoint_blocks") {
                "checkpoint_blocks = 4"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&founder_toml, cfg).expect("write founder node.toml");

    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    cluster.submit(
        0,
        "chat",
        &encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "general".into(),
            post_policy: PostPolicy::Open,
        }),
    );

    // ---- the ceremony runs IDLE: governance ops want a responsive chain and
    // are not the seam under test.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_admitted(1, CONVERGE);
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // prove the follow loop is live before the freeze, exactly as the sibling
    // does — a follower that was never following proves nothing on thaw.
    cluster.submit(0, "chat", &encode_msg(&post("m-live", "pre-freeze post")));
    resident_sees(&cluster, "m-live", "the pre-freeze adoption", CONVERGE);

    // ---- the load starts here, and only here.
    let stop = Arc::new(AtomicBool::new(false));
    let token = noded::admin::read_operator_token(&cluster.workspace(0))
        .expect("founder minted an operator credential");
    let pump = spawn_pump(cluster.http_ports[0], token, Arc::clone(&stop));

    let frozen_at = cluster.status(1)["height"]
        .as_u64()
        .expect("the resident reports a height before the freeze");

    // ---- freeze past the lease. THE FREEZE IS THE ONLY WAIT ON A CLOCK in
    // this test: the lease it has to outlive is measured in wall time, so no
    // event on either node can stand in for it. Everything after is
    // event-driven.
    cluster.signal(1, "STOP");
    std::thread::sleep(Duration::from_secs(sync_lease_secs()) + PAST_LEASE_SLACK);
    cluster.signal(1, "CONT");
    let thawed = Instant::now();

    // ---- the point, part one: the founder REFUSED the thawed follower's
    // range. without this the assertion below could pass on a lane where the
    // floor never armed and the follower merely caught up.
    cluster.wait_marker(0, "pruned_below_retention_floor", RECOVER_PAST_LEASE);

    // ---- the point, part two: it comes back anyway, to the founder's own
    // root, with no operator action. quiet the chain first so both nodes
    // settle on one tip — roots are only comparable at equal height.
    stop.store(true, Ordering::Relaxed);
    pump.join().expect("pump thread joins");

    // NOT `cluster.status`: that helper ASSERTS the node answered, and a
    // resident midway through a re-bootstrap answers `node unresponsive` —
    // which is a "not yet", not a failure. Asserting inside the probe turns
    // the very state this test waits through into a hard error, and it does
    // so on timing, so it passes until it doesn't.
    let settled = |idx: usize| -> Option<(u64, String)> {
        let (code, body) =
            nettest::try_http_json(cluster.http_ports[idx], "GET", "/v1/status", None).ok()?;
        (code == 200).then_some(())?;
        Some((
            body["height"].as_u64()?,
            body["root_hash"].as_str()?.to_owned(),
        ))
    };

    let (height, root) = cluster.await_committed(
        1,
        "the thawed resident back on the founder's height and root",
        RECOVER_PAST_LEASE,
        || {
            let (height, root) = settled(0)?;
            let (resident_height, resident_root) = settled(1)?;
            (resident_height == height && resident_root == root).then_some((height, root))
        },
    );
    assert!(
        height > frozen_at,
        "the chain must have advanced past the frozen height {frozen_at}, ended at {height}"
    );
    println!(
        "resident re-bootstrapped from height {frozen_at} to {height} (root {root}) in {:?}",
        thawed.elapsed()
    );

    cluster.kill(1);
    cluster.kill(0);
}

/// a writer keeping a real op in every block window, so the founder's drain
/// carries genuine apply/index/checkpoint work and its oplog prune actually
/// runs. deliberately self-paced and small: heavier floods stall a chain
/// outright, which is a different failure than the retention seam under test.
/// (the sibling in `busy_chain_ascension_e2e` is the same shape; it is copied
/// rather than hoisted into `common` to keep this a test-local change.)
fn spawn_pump(http_port: u16, token: String, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let filler = "keep the chain busy ".repeat(10); // ~200 B of block weight
        let mut i = 0u64;
        while !stop.load(Ordering::Relaxed) {
            i += 1;
            let payload = encode_msg(&post(&format!("pump-{i}"), &filler));
            let _ = nettest::try_http_bytes_with(
                http_port,
                "POST",
                "/v1/submit",
                "application/json",
                &[(noded::admin::ADMIN_TOKEN_HEADER, &token)],
                &serde_json::to_vec(&serde_json::json!({
                    "target": "chat",
                    "payload": serde_json::from_slice::<serde_json::Value>(&payload)
                        .expect("an encoded ChatMsg is json"),
                }))
                .expect("pump request serializes"),
            );
        }
    })
}

/// poll the RESIDENT's own read surface (node 1 — reads serve from its
/// pre-synced host, never a validator's) until `message_id` is visible.
fn resident_sees(cluster: &NetworkShapeCluster, message_id: &str, what: &str, deadline: Duration) {
    cluster.await_committed(1, what, deadline, || {
        let raw = cluster.query(
            1,
            "chat",
            &encode_query(&ChatQuery::MessagesRange {
                channel_id: "general".into(),
                from_seq: 1,
                limit: 10,
            }),
        )?;
        let ChatReply::Messages(views) = decode_reply(&raw).ok()? else {
            return None;
        };
        views
            .into_iter()
            .any(|v| v.head.message_id == message_id)
            .then_some(())
    });
}

/// an Open-channel post to `general` with a caller-chosen message id.
fn post(id: &str, text: &str) -> ChatMsg {
    ChatMsg::PostMessage {
        channel_id: "general".into(),
        message_id: id.into(),
        blocks: vec![Block::paragraph(text)],
        thread: None,
    }
}
