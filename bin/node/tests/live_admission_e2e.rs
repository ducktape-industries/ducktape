//! network-shape live admission: a fresh identity produced by `join` can start
//! immediately, park as a read-only resident, and promote once a running
//! member admits it through governance. ONE epoch cutover seats the key —
//! the chain pauses at that cutover awaiting the new member's votes — and
//! the parked node seats itself IN-PROCESS: a warm resident from its own
//! folded state (no fetch from the halted members), a cold direct admission
//! from the frozen boundary it syncs first. `promoted: validator at epoch …;
//! seating in-process` marks the hand-off; the same process is the
//! validator from there.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::NetworkShapeCluster;
use common::wire::tasks::{
    TaskMsg, TaskQuery, TaskReply, decode_task_reply, encode_task_msg, encode_task_query,
};
use files::{
    Change, Content, EntryInfo, FilesMsg, FilesQuery, FilesReply, Kind, RefsInfo,
    decode_reply as files_decode_reply, encode_msg as files_encode_msg, encode_putblob,
    encode_query as files_encode_query, objects::object_id, to_hex,
};

const CONVERGE: Duration = Duration::from_secs(180);

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

fn task_title(cluster: &NetworkShapeCluster, idx: usize, task_id: &str) -> Option<String> {
    let req = encode_task_query(&TaskQuery::Get {
        task_id: task_id.into(),
    });
    let reply = cluster.query(idx, "tasks", &req)?;
    match decode_task_reply(&reply) {
        Ok(TaskReply::Task(task)) => task.map(|t| t.title),
        _ => None,
    }
}

#[test]
fn network_shape_joiner_parks_until_promote() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("live-admission");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    // network-shape nodes never print the dev-demo `converged root_hash=`; the
    // founder is up and finalizing once its rpc surface is listening (genesis
    // is already crossed by then), which is all `invite`/`promote` need.
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // In the manual flow, the pubkey travels out-of-band
    // and no first-contact intro happens — the tokened flavor has its own e2e
    // (join_request_e2e).
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(
        friend_key.len(),
        64,
        "join should print the friend's public key hex"
    );

    cluster.spawn(1);
    cluster.wait_marker(1, "joiner mode:", Duration::from_secs(60));
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));

    let (ok, out) = cluster.run_promote(&friend_key);
    assert!(ok, "promote failed:\n{out}");
    assert!(out.contains("admitted"), "unexpected verb output:\n{out}");

    // direct admission: ONE cutover seats the friend; it syncs the frozen
    // boundary and seats there, in-process. (the staged resident flow has
    // its own leg below.)
    cluster.wait_marker(0, "cutover complete: epoch 1", CONVERGE);
    cluster.wait_marker(1, "admitted at epoch 1", CONVERGE);
    cluster.wait_marker(1, "synced root_hash=", CONVERGE);
    cluster.wait_marker(1, "promoted: validator at epoch 1", CONVERGE);

    // the grant's mesh admission landed on a FRESH generation index: the
    // old failure mode — commonware warn-dropping a same-index re-track,
    // leaving the joiner bounced at the door until the cutover — is
    // disproven directly by the absence of its warn on the founder.
    let founder_log = std::fs::read_to_string(cluster.log_path(0)).expect("founder log");
    assert!(
        !founder_log.contains("peer set already exists"),
        "a mesh track was silently rejected on the founder"
    );
    assert!(
        !founder_log.contains("index must monotonically increase"),
        "a mesh track regressed the index order on the founder"
    );
}

/// the SEAT leg the markers above stop short of: after `promoted: validator
/// at epoch` the same process must actually be a validator — the seated
/// engine votes the halted quorum back to life, the serve lanes answer, and
/// writes finalize into the widened set. every other promote leg in this
/// suite latches the `promoted:` marker and stops, so a node that seats and
/// then wedges (the field failure class the retired exec-reboot flow had:
/// a promoted node dying in its post-reboot dialogue while the founder sat
/// halted) would be invisible to CI without the liveness assertions below.
#[test]
fn promoted_resident_seats_in_process_and_serves() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("promote-reboot");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // resident standing first — the field node parked as a resident (staged
    // admission) before its promote, so the seat starts from a warm,
    // boundary-following node exactly as it did in the field.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    cluster.spawn(1);
    cluster.wait_marker(1, "joiner mode:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    let (ok, out) = cluster.run_promote(&friend_key);
    assert!(ok, "promote failed:\n{out}");
    cluster.wait_marker(1, "promoted: validator at epoch", CONVERGE);

    // the network the seat lands in is LIVE end to end: a write
    // finalized through the founder becomes readable from the promoted
    // friend's own surface…
    cluster.submit(0, "tasks", &task_create("post-promote-founder", "landed"));
    cluster.await_committed(
        1,
        "the promoted friend to serve the founder's write",
        CONVERGE,
        || task_title(&cluster, 1, "post-promote-founder").filter(|t| t == "landed"),
    );
    // …and the promoted friend's own ordered lane finalizes into the widened
    // quorum (a halted founder can never land this).
    cluster.submit(1, "tasks", &task_create("post-promote-friend", "landed"));
    cluster.await_committed(
        0,
        "the founder to serve the friend's write",
        CONVERGE,
        || task_title(&cluster, 0, "post-promote-friend").filter(|t| t == "landed"),
    );
}

// ---- duckfs joiner proof: full object possession over the REAL wire ---------
//
// the in-process `duckfs_resolver` test (statesync/tests) proves the resolver
// moves bytes at the module level. THIS proves it end to end over the real mesh
// transport: a founder holds non-trivial duckfs state (inline files, a putblob-
// staged chunk-object file, and a pin), a fresh node joins through the real
// `join`/`promote` ceremony, and its `sync_all_modules` pass loops GetObjects to
// FULL object possession over the p2p statesync lane before the joiner reads a
// file back BYTE-IDENTICAL. (`synced root_hash=` latches only after the resolver
// reports `possession_complete`, so the promoted read is proof bytes crossed.)

fn df_put_inline(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Inline {
            b64: STANDARD.encode(bytes),
        },
    }
}

fn df_chunk_hex(bytes: &[u8]) -> String {
    to_hex(&object_id(Kind::Chunk, bytes))
}

/// a distinctive, non-uniform byte pattern (251 is prime — a truncated or
/// corrupt sync is caught, not masked by a run of a repeated byte).
fn df_pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn df_stat(cluster: &NetworkShapeCluster, idx: usize, path: &str) -> Option<EntryInfo> {
    let reply = cluster.query(
        idx,
        "files",
        &files_encode_query(&FilesQuery::Stat {
            path: path.into(),
            snapshot: None,
        }),
    )?;
    match files_decode_reply(&reply).ok()? {
        FilesReply::Stat(info) => info,
        _ => None,
    }
}

fn df_read(
    cluster: &NetworkShapeCluster,
    idx: usize,
    path: &str,
    offset: u64,
    len: u64,
) -> Option<Vec<u8>> {
    let reply = cluster.query(
        idx,
        "files",
        &files_encode_query(&FilesQuery::Read {
            path: path.into(),
            snapshot: None,
            offset,
            len,
        }),
    )?;
    match files_decode_reply(&reply).ok()? {
        FilesReply::Read { b64, .. } => STANDARD.decode(b64.as_bytes()).ok(),
        _ => None,
    }
}

fn df_refs(cluster: &NetworkShapeCluster, idx: usize) -> Option<RefsInfo> {
    let reply = cluster.query(idx, "files", &files_encode_query(&FilesQuery::Refs {}))?;
    match files_decode_reply(&reply).ok()? {
        FilesReply::Refs(info) => Some(info),
        _ => None,
    }
}

fn df_head(cluster: &NetworkShapeCluster, idx: usize) -> Option<String> {
    df_refs(cluster, idx)?.head
}

#[test]
fn network_shape_joiner_rebuilds_duckfs_over_the_wire() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("duckfs-joiner");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // ---- seed non-trivial duckfs state on the founder BEFORE the join, so the
    //      boundary the friend syncs carries it -------------------------------
    // (1) two inline files in nested dirs, one commit off the empty tree.
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: None,
            message: "seed inline".into(),
            changes: vec![
                df_put_inline("/shared/a", b"alpha"),
                df_put_inline("/shared/dir/b", b"beta"),
            ],
        }),
    );
    cluster.await_committed(0, "founder inline files to finalize", CONVERGE, || {
        df_stat(&cluster, 0, "/shared/a").map(|_| ())
    });
    let s1 = cluster.await_committed(0, "founder head after inline commit", CONVERGE, || {
        df_head(&cluster, 0)
    });

    // (2) a file whose bytes are STAGED as a chunk object via putblob and
    // referenced by digest in a Chunks commit — the odb-object path. 128 KiB is
    // a real, multi-page odb object that keeps THIS test about admission, not
    // payload size; the full-CHUNK_SIZE multi-chunk graph over the op path is
    // large_file_e2e's job (#215: the binary frame codec carries a 1 MiB chunk
    // whole). same-origin submits finalize in seq order.
    let chunk = df_pattern(128 * 1024);
    let chunk_size = chunk.len() as u64;
    cluster.submit(0, "files", &encode_putblob(&chunk));
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: Some(s1.clone()),
            message: "seed chunked".into(),
            changes: vec![Change::Put {
                path: "/shared/big".into(),
                exec: false,
                meta: BTreeMap::new(),
                content: Content::Chunks {
                    size: chunk_size,
                    chunks: vec![df_chunk_hex(&chunk)],
                },
            }],
        }),
    );
    cluster.await_committed(0, "founder chunked file to finalize", CONVERGE, || {
        df_stat(&cluster, 0, "/shared/big")
            .filter(|e| e.size == chunk_size)
            .map(|_| ())
    });
    let s2 = cluster.await_committed(0, "founder head after chunked commit", CONVERGE, || {
        df_head(&cluster, 0)
    });

    // (3) pin the head — a gc root the joiner must reconstruct too.
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Pin {
            snapshot: s2.clone(),
            name: "release".into(),
        }),
    );
    cluster.await_committed(0, "founder pin to finalize", CONVERGE, || {
        df_refs(&cluster, 0)
            .filter(|r| r.pins.contains_key("release"))
            .map(|_| ())
    });
    // the source bytes the joiner must reconstruct byte-for-byte.
    let src_chunk =
        df_read(&cluster, 0, "/shared/big", 0, chunk_size).expect("founder read chunked");
    assert_eq!(src_chunk, chunk, "founder holds the seeded bytes");

    // ---- invite -> park -> promote the friend (real join verb, real mesh) ----
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    cluster.spawn(1);
    cluster.wait_marker(1, "joiner mode:", Duration::from_secs(60));
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));

    let (ok, out) = cluster.run_promote(&friend_key);
    assert!(ok, "promote failed:\n{out}");
    assert!(
        out.contains("admitted"),
        "unexpected promote output:\n{out}"
    );

    cluster.wait_marker(0, "cutover complete: epoch 1", CONVERGE);
    cluster.wait_marker(1, "admitted at epoch 1", CONVERGE);
    // `synced root_hash=` latches only AFTER `sync_all_modules` -> the duckfs
    // resolver reaches FULL object possession over the real p2p statesync lane
    // (it loops GetObjects until `possession_complete`). this marker alone is the
    // production proof that every duckfs object crossed the wire.
    cluster.wait_marker(1, "synced root_hash=", CONVERGE);
    cluster.wait_marker(1, "promoted: validator at epoch 1", CONVERGE);

    // ---- THE property: the promoted joiner reads the founder's files back
    // BYTE-IDENTICAL. it holds them only because the possession loop moved every
    // chunk / file / tree / snapshot object over the wire and its post-promotion
    // reboot recovered them from disk; an empty odb errors on Read.
    let joined_chunk =
        cluster.await_committed(1, "joiner to read the chunked file", CONVERGE, || {
            df_read(&cluster, 1, "/shared/big", 0, chunk_size)
        });
    assert_eq!(
        joined_chunk, chunk,
        "joiner rebuilt the chunked file byte-identical over the wire"
    );
    assert_eq!(
        df_read(&cluster, 1, "/shared/a", 0, 64).as_deref(),
        Some(b"alpha".as_ref()),
        "joiner rebuilt inline /shared/a"
    );
    assert_eq!(
        df_read(&cluster, 1, "/shared/dir/b", 0, 64).as_deref(),
        Some(b"beta".as_ref()),
        "joiner rebuilt nested inline /shared/dir/b"
    );
    // head and pin (the refs image) match the source's exactly.
    let refs = df_refs(&cluster, 1).expect("joiner refs");
    assert_eq!(
        refs.head.as_deref(),
        Some(s2.as_str()),
        "joiner head matches the source"
    );
    assert_eq!(
        refs.pins.get("release").map(String::as_str),
        Some(s2.as_str()),
        "joiner pin matches the source"
    );
}

/// the STAGED admission flow end-to-end: invite → resident (mesh + pre-sync,
/// NO quorum seat) → promote → validator. the payoff assertions are the
/// quorum ones the one-step flow could never make:
///
///   1. while the friend holds resident standing, the valset's VALIDATOR set
///      still names only the founder — committed state proves the tier split;
///   2. the resident SERVES: local reads (rpc + http) answer from its own
///      pre-synced boundary, a write lands through the submit relay (and the
///      resident reads its own write back), and a value the founder finalizes
///      becomes readable through the resident's surface (the continuous
///      follow);
///   3. the chain keeps finalizing with the resident KILLED (under the old
///      one-step flow the friend would already hold a quorum seat here, and a
///      2-member quorum with one member down is a stall);
///   4. a restarted resident parks straight back into resident mode (the
///      pre-sync writes no checkpoint manifest — a reboot is clean) and serves
///      again;
///   5. `resident remove` REVOKES standing through the same ceremony: committed
///      residents empty, the node falls back to a parked joiner, and a second
///      run is an honest no-op;
///   6. `resident accept` re-grants, and the resident resumes the follow (a
///      post-re-grant write becomes readable through its surface);
///   7. `promote` then seats a WARM validator through the normal path.
///
#[test]
fn staged_admission_resident_presyncs_then_promotes_warm() {
    use valset::{ValsetQuery, ValsetReply};

    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("staged-admission");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // every predicate reads committed state, so each re-read rides the
    // founder's block wake; CONVERGE only names the failure.
    let poll =
        |cluster: &NetworkShapeCluster, what: &str, mut pred: Box<dyn FnMut() -> bool + '_>| {
            cluster.await_committed(0, what, CONVERGE, || pred().then_some(()));
        };

    // A TASKS WRITE BEFORE ANYONE JOINS: the resident below never sees
    // this block as a frame — it arrives inside the synced boundary, with the
    // op feed that carried it long gone. The join-seam op-row backfill (spec
    // §7) is the only reason it can ever be in the resident's own feed.
    cluster.submit(0, "tasks", &task_create("pre-join", "written"));
    poll(
        &cluster,
        "the pre-join write to finalize on the founder",
        Box::new(|| task_title(&cluster, 0, "pre-join").is_some_and(|t| t == "written")),
    );

    // ---- invite → park → resident grant ------------------------------------
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    cluster.spawn(1);
    cluster.wait_marker(1, "joiner mode:", Duration::from_secs(60));

    let (ok, text) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{text}");
    assert!(
        text.contains("granted resident standing"),
        "unexpected resident accept output:\n{text}"
    );

    // the grant's boundary admits the resident to the mesh; the parked node
    // then pre-syncs.
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // (1) the tier split in COMMITTED state: validators = founder only,
    //     residents = the friend.
    let validators = cluster
        .query(0, "valset", &valset::encode_query(&ValsetQuery::Validators))
        .and_then(|raw| valset::decode_reply(&raw).ok())
        .map(|r| match r {
            ValsetReply::Validators(v) => v,
            other => panic!("expected Validators, got {other:?}"),
        })
        .expect("valset validators readable");
    assert_eq!(
        validators.len(),
        1,
        "the quorum still seats ONLY the founder"
    );
    let residents = cluster
        .query(0, "valset", &valset::encode_query(&ValsetQuery::Residents))
        .and_then(|raw| valset::decode_reply(&raw).ok())
        .map(|r| match r {
            ValsetReply::Residents(v) => v,
            other => panic!("expected Residents, got {other:?}"),
        })
        .expect("valset residents readable");
    assert_eq!(
        residents,
        vec![common::unhex(&friend_key)],
        "the friend holds resident standing"
    );

    // (2) the SERVING resident: the same local read surfaces a validator
    //     binds, answered from the resident's own pre-synced boundary.
    //     rpc status names the served boundary…
    poll(
        &cluster,
        "the resident to serve rpc status",
        Box::new(|| {
            let st = cluster.rpc(1, serde_json::json!({ "cmd": "status" }));
            st["ok"] == serde_json::json!(true)
                && st["status"]["height"].as_u64().is_some_and(|h| h > 0)
        }),
    );
    //     …module reads answer from the RESIDENT's surface (the tier split is
    //     visible through the resident itself, not just the founder)…
    poll(
        &cluster,
        "the resident to serve valset reads",
        Box::new(|| {
            cluster
                .query(1, "valset", &valset::encode_query(&ValsetQuery::Residents))
                .and_then(|raw| valset::decode_reply(&raw).ok())
                .is_some_and(|r| {
                    matches!(
                    r,
                    ValsetReply::Residents(v) if v == vec![common::unhex(&friend_key)]
                        )
                })
        }),
    );
    //     …the http app surface answers its status route from the same host…
    {
        let (status, body) = nettest::http_text(cluster.http_ports[1], "GET", "/v1/status");
        assert_eq!(status, 200, "resident /v1/status must answer 200:\n{body}");
        assert!(
            body.contains("\"height\""),
            "resident /v1/status carries a height:\n{body}"
        );
    }
    //     …a write LANDS through the submit relay: the resident signs with its
    //     own key, ships the frame to the validator, and the rpc reply holds
    //     until the frame finalizes (ok == Applied)…
    let landed = cluster.rpc(
        1,
        serde_json::json!({
            "cmd": "submit",
            "target": "tasks",
            "payload_hex": common::hex(&task_create("resident-writes", "landed")),
        }),
    );
    assert_eq!(
        landed["ok"],
        serde_json::json!(true),
        "the resident submit should relay + finalize (ok == Applied): {landed}"
    );
    //     …and the resident READS ITS OWN WRITE once its follow arm crosses
    //     the boundary that carries it…
    poll(
        &cluster,
        "the resident to serve its own relayed write",
        Box::new(|| task_title(&cluster, 1, "resident-writes").is_some_and(|t| t == "landed")),
    );
    //     …and the follow is CONTINUOUS: a value the founder finalizes now
    //     becomes readable through the resident within a few boundaries.
    cluster.submit(0, "tasks", &task_create("resident-follow", "fresh"));
    poll(
        &cluster,
        "the resident to serve the followed write",
        Box::new(|| task_title(&cluster, 1, "resident-follow").is_some_and(|t| t == "fresh")),
    );
    //     …and the DERIVED tier follows the boundary too: the explorer
    //     records the followed boundary (an honest boundary row — verified
    //     height + root-hash, frame-derived fields empty)…
    poll(
        &cluster,
        "the resident explorer to record a followed boundary",
        Box::new(|| {
            let (status, body) =
                common::http_request(cluster.http_ports[1], "GET", "/v1/blocks", None);
            status == 200
                && body["blocks"].as_array().is_some_and(|rows| {
                    rows.iter().any(|b| {
                        b["hash"] == serde_json::json!("")
                            && b["height"].as_u64().is_some_and(|h| h > 0)
                            && !b["commit_hash"].as_str().unwrap_or_default().is_empty()
                    })
                })
        }),
    );
    //     …and /v1/index/* answers from healthy read models WITH pre-join
    //     history. the ascension heal still stamps a floor at the boundary,
    //     but the op-row backfill then walks the source's rows below it and
    //     CLEARS that floor (indexable spec §7) — so the honest end state is
    //     no floor at all, or the SOURCE's own floor inherited when the
    //     source itself joined late. what must never happen again is a floor
    //     sitting at the joiner's own boundary with an empty feed beneath it.
    //     polled: a heal drops the watermark FIRST (crash-safety by
    //     re-trigger), so a read racing an in-flight heal legitimately sees 0
    //     for a moment, and the floor clears only after the fold drains.
    poll(
        &cluster,
        "the resident index to report folding watermarks over a backfilled feed",
        Box::new(|| {
            let (status, index_status) =
                common::http_request(cluster.http_ports[1], "GET", "/v1/index/status", None);
            let watermark = index_status["modules"]["tasks"].as_u64().unwrap_or(0);
            let floor = index_status["backfilled"]["tasks"].as_u64();
            // the founder is the sync source and has folded from genesis, so it
            // has no floor of its own to compose in: the joiner's clears outright.
            // STRONGER than it was under `directory`: that tenant had no index
            // guest, so `explorer.rs:340-356` cleared the floor for free
            // (`folds == false`); `tasks` folds through a real mapper, so
            // `floor.is_none()` now depends on the fold reaching the
            // backfilled rows.
            status == 200
                && index_status["poisoned"] == serde_json::json!(false)
                && watermark > 0
                && floor.is_none()
        }),
    );
    //     and the op feed BELOW that boundary is really there — the whole
    //     point of clearing the floor. The pre-join write finalized before
    //     this node existed, so nothing but the backfill can have put it in
    //     THIS node's `/v1/index/tasks/ops`.
    let (status, ops) = common::http_request(
        cluster.http_ports[1],
        "GET",
        "/v1/index/tasks/ops?limit=100",
        None,
    );
    assert_eq!(status, 200, "resident op feed: {ops}");
    // the row carries the submitted payload verbatim (`index.rs:222-226`), so
    // the predicate reads through `encode_task_msg`'s `WorkMsg::Task` envelope.
    assert!(
        ops["ops"].as_array().is_some_and(|rows| rows.iter().any(
            |r| r["payload"]["task"]["create_task"]["task_id"] == serde_json::json!("pre-join")
        )),
        "a cleared floor promises pre-boundary rows are really there: {ops}"
    );

    // (3) quorum untouched: kill the resident; the founder keeps finalizing.
    //     ONE wait, on the op this suite cares about. this used to carry a
    //     SECOND wait — "let the resident apply one more block, which syncs
    //     that seal" — because a `Record::Seal` was a plain WAL append while a
    //     `Backing::Store` tenant (`tasks` is one) had already committed the
    //     block durably, so a SIGKILL between the two left the store at a root
    //     no retained seal vouched for and recovery fail-stopped `Error::Torn`.
    //     the seal now fsyncs where it is written, so the tip is durable the
    //     moment it is sealed and the second wait is gone. it staying gone is
    //     the process-level regression proof: restore the window and this kill
    //     tears again, exactly as it did at heights 43 and 94.
    cluster.submit(0, "tasks", &task_create("pre-kill-quiesce", "folded"));
    poll(
        &cluster,
        "the resident to fold the last op before the kill",
        Box::new(|| task_title(&cluster, 1, "pre-kill-quiesce").is_some_and(|t| t == "folded")),
    );
    cluster.kill(1);
    cluster.submit(0, "tasks", &task_create("resident-down-liveness", "alive"));
    poll(
        &cluster,
        "a finalized op with the resident down",
        Box::new(|| task_title(&cluster, 0, "resident-down-liveness").is_some()),
    );

    // (4) a restarted resident parks straight back into resident mode — the
    //     pre-sync left NO checkpoint manifest behind. (the config-time
    //     joiner banner may not reprint: the first run's recovery-journal
    //     files flip the cheap boot probe, and the runtime then re-decides
    //     from the real store — the resident marker alone is the proof.)
    //     it then SERVES again from a fresh pre-sync.
    cluster.spawn(1);
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);
    poll(
        &cluster,
        "the restarted resident to serve reads again",
        Box::new(|| {
            cluster.rpc(1, serde_json::json!({ "cmd": "status" }))["ok"] == serde_json::json!(true)
        }),
    );

    // (5) resident remove: the ceremony verb revokes standing. committed
    //     state clears, and the resident — whose respawned log is fresh, so
    //     the parked marker is unambiguously post-revoke — falls back to a
    //     parked joiner at the boundary whose manifest drops it.
    let (ok, out) = cluster.run_membership_verb("resident remove", &friend_key);
    assert!(ok, "resident remove failed:\n{out}");
    assert!(
        out.contains("revoked resident standing"),
        "unexpected resident remove output:\n{out}"
    );
    poll(
        &cluster,
        "the revoke to clear resident standing",
        Box::new(|| {
            cluster
                .query(0, "valset", &valset::encode_query(&ValsetQuery::Residents))
                .and_then(|raw| valset::decode_reply(&raw).ok())
                .is_some_and(|r| matches!(r, ValsetReply::Residents(v) if v.is_empty()))
        }),
    );
    cluster.wait_marker(1, "joining: awaiting redemption", CONVERGE);
    //     a second run is an honest no-op — the inverted guard, end to end.
    let (ok, out) = cluster.run_membership_verb("resident remove", &friend_key);
    assert!(ok, "resident remove (no standing) failed:\n{out}");
    assert!(
        out.contains("holds no resident standing"),
        "unexpected no-op resident remove output:\n{out}"
    );

    // (6) re-grant: resident accept restores standing and the resident resumes
    //     the follow — a write finalized AFTER the re-grant becomes readable
    //     through the resident's own surface (stale serves can't fake this:
    //     the revoked node never synced a boundary carrying this key).
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "re-grant resident accept failed:\n{out}");
    assert!(
        out.contains("granted resident standing"),
        "unexpected re-grant output:\n{out}"
    );
    poll(
        &cluster,
        "the re-grant to restore resident standing",
        Box::new(|| {
            cluster
                .query(0, "valset", &valset::encode_query(&ValsetQuery::Residents))
                .and_then(|raw| valset::decode_reply(&raw).ok())
                .is_some_and(|r| {
                    matches!(
                    r,
                    ValsetReply::Residents(v) if v == vec![common::unhex(&friend_key)]
                        )
                })
        }),
    );
    cluster.submit(0, "tasks", &task_create("post-revoke-follow", "back"));
    poll(
        &cluster,
        "the re-granted resident to resume the follow",
        Box::new(|| task_title(&cluster, 1, "post-revoke-follow").is_some_and(|t| t == "back")),
    );

    // (7) promote: the warm resident becomes a validator through the
    //     in-process seat — its own fold observes the cutover that seats
    //     it, it checkpoints its OWN folded state, and the same process
    //     continues as the validator (no re-sync: at a quorum-widening
    //     cutover the founder halts awaiting this very node, so there is
    //     nothing to fetch FROM); valset Join clears its resident standing.
    let (ok, out) = cluster.run_promote(&friend_key);
    assert!(ok, "promote failed:\n{out}");
    assert!(
        out.contains("admitted"),
        "unexpected promote output:\n{out}"
    );
    // no `admitted at epoch` marker here: that line is the COLD path's
    // manifest fetch, and a warm seat deliberately fetches nothing — its
    // own fold carries it straight to `promoted:`.
    cluster.wait_marker(1, "promoted: validator at epoch", CONVERGE);
    let residents = cluster
        .query(0, "valset", &valset::encode_query(&ValsetQuery::Residents))
        .and_then(|raw| valset::decode_reply(&raw).ok())
        .map(|r| match r {
            ValsetReply::Residents(v) => v,
            other => panic!("expected Residents, got {other:?}"),
        })
        .expect("valset residents readable");
    assert!(
        residents.is_empty(),
        "promotion must clear resident standing (got {residents:?})"
    );

    // (8) restart the still-seated promoted validator. Non-genesis keys now
    //     resolve every boot from the latest committed manifest, so the same
    //     resolver must return a promotion baton while the key remains seated.
    cluster.kill(1);
    cluster.spawn(1);
    cluster.wait_marker(1, "promoted: validator at epoch", CONVERGE);
    cluster.submit(0, "tasks", &task_create("validator-role-restart", "voting"));
    poll(
        &cluster,
        "the restarted validator to restore quorum and serve the new write",
        Box::new(|| {
            task_title(&cluster, 1, "validator-role-restart").is_some_and(|t| t == "voting")
        }),
    );

    // (9) remove the promoted validator. The two-member electorate needs one
    //     ballot from each node. The friend votes first so the surviving
    //     founder can execute and observe the result: the cutover halts the
    //     friend's process at that committed boundary, closing its local RPC.
    let (ok, out) = cluster.run_membership_verb_as(1, "member remove", &friend_key);
    assert!(ok, "friend member remove ballot failed:\n{out}");
    assert!(
        out.contains("waiting on other voters"),
        "the first removal ballot should await the founder:\n{out}"
    );
    let (ok, out) = cluster.run_membership_verb("member remove", &friend_key);
    assert!(ok, "founder member remove ballot failed:\n{out}");
    assert!(out.contains("removed"), "unexpected removal output:\n{out}");
    cluster.wait_marker(1, "demoted from the validator set; halting", CONVERGE);
    cluster.wait_exit(1, CONVERGE);
    poll(
        &cluster,
        "the removal to leave only the founder validator",
        Box::new(|| {
            cluster
                .query(0, "valset", &valset::encode_query(&ValsetQuery::Validators))
                .and_then(|raw| valset::decode_reply(&raw).ok())
                .is_some_and(|r| matches!(r, ValsetReply::Validators(v) if v.len() == 1))
        }),
    );

    // (10) re-grant the stopped key as a resident, then restart its actual
    //     process with the validator-seating checkpoint left by promotion.
    //     The latest committed standing, not that stale checkpoint role, must
    //     select the resident path and resume the follow.
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "post-removal resident accept failed:\n{out}");
    assert!(
        out.contains("granted resident standing"),
        "unexpected post-removal resident grant:\n{out}"
    );
    poll(
        &cluster,
        "the removed validator to regain resident standing",
        Box::new(|| {
            cluster
                .query(0, "valset", &valset::encode_query(&ValsetQuery::Residents))
                .and_then(|raw| valset::decode_reply(&raw).ok())
                .is_some_and(|r| {
                    matches!(
                        r,
                        ValsetReply::Residents(v) if v == vec![common::unhex(&friend_key)]
                    )
                })
        }),
    );
    cluster.spawn(1);
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);
    cluster.submit(
        0,
        "tasks",
        &task_create("validator-to-resident-restart", "followed"),
    );
    poll(
        &cluster,
        "the restarted resident to follow a new write",
        Box::new(|| {
            task_title(&cluster, 1, "validator-to-resident-restart")
                .is_some_and(|t| t == "followed")
        }),
    );
}
