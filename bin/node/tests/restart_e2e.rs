//! a network that survives reboots — the real-socket proof for restart
//! recovery. one OS-process solo validator (a network of one: simplex with
//! participants = {self} is live) is crashed with SIGKILL and later shut down
//! gracefully; each respawn over the SAME storage dir must recover the
//! composed root-hash WITHOUT re-running genesis, resume the finalized
//! boundary, and keep finalizing new ops over its reopened consensus journal
//! (the anti-equivocation record makes a fresh-journal respawn unsafe, so
//! liveness after respawn is the property that proves the journal reopened).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::Cluster;
use files::{
    Change, Content, EntryInfo, FilesMsg, FilesQuery, FilesReply, Kind, RefsInfo,
    decode_reply as files_decode_reply, encode_msg as files_encode_msg, encode_putblob,
    encode_query as files_encode_query, objects::object_id, to_hex,
};
use common::wire::tasks::{TaskMsg, TaskQuery, TaskReply, decode_task_reply, encode_task_msg, encode_task_query};

/// a create for `task_id`; NOT an upsert — `tasks` refuses a duplicate id
/// (`task_board.rs:77-80`). That rejection is ISOLATED, not fatal: the op's
/// stage rolls back and it is recorded `Rejected` while the block still seals
/// (`host/src/lib.rs:237`, `:283`). So a duplicate never applies and never
/// announces itself — `write_and_confirm` spins to its timeout, or passes
/// VACUOUSLY when the re-created title happens to match the surviving one.
/// Every call site here has to carry a fresh id — including across a restart,
/// where the board comes back from the reopened store with the earlier ids
/// still on it.
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

/// submit a tasks write via rpc and wait until it is readable back — the
/// end-to-end "the engine is live and finalizing" probe.
fn write_and_confirm(cluster: &Cluster, idx: usize, task_id: &str, title: &str) {
    cluster.submit(idx, "tasks", &task_create(task_id, title));
    cluster.await_committed(
        idx,
        &format!("tasks {task_id}={title} to finalize"),
        Duration::from_secs(30),
        || (task_title(cluster, idx, task_id).as_deref() == Some(title)).then_some(()),
    );
}

/// the ops in an explorer row that name `target`. a block is a BATCH and a row
/// lists every member op, so the tenant's op is NOT reliably `ops[0]`: the dev
/// shape's boot seed submits one `tasks` op at seq 0
/// (`validator/engine.rs:273-283`) and it batches with whatever else is
/// pending, which lands it ahead of the first submitted write about half the
/// time. scanning the row is the only stable read.
///
/// the seed rides THIS SAME tenant, so `target` alone never separates it from a
/// write — anything that must name a specific write matches the op's `payload`
/// preview instead (`carries_write` below).
fn ops_targeting<'a>(row: &'a serde_json::Value, target: &str) -> Vec<&'a serde_json::Value> {
    row["ops"]
        .as_array()
        .expect("row ops is an array")
        .iter()
        .filter(|op| op["target"] == target)
        .collect()
}

/// does any row carry the `tasks` write that created `task_id`? picked by
/// IDENTITY, not by count: the boot seed's own `tasks` op is indistinguishable
/// from a write by target, so counting tenant-carrying rows would let
/// [seed, write#1] satisfy a "both writes landed" wait while write#2's row is
/// still missing. the row carries the op payload verbatim
/// (`noded::payload_preview` truncates only past 1024 chars), and the quoted id
/// appears in it as `"task_id":"<id>"`, so it cannot collide with a title.
fn carries_write(rows: &[(u64, serde_json::Value)], task_id: &str) -> bool {
    let quoted = format!("\"task_id\":\"{task_id}\"");
    rows.iter().any(|(_, row)| {
        ops_targeting(row, "tasks")
            .iter()
            .any(|op| op["payload"].as_str().is_some_and(|p| p.contains(&quoted)))
    })
}

/// the explorer rows /v1/blocks currently serves, keyed by height. only real
/// ops appear (heartbeat nops never get a row), so this is exactly the set
/// restart recovery must preserve.
fn block_rows(cluster: &Cluster, idx: usize) -> Vec<(u64, serde_json::Value)> {
    let (code, body) = cluster.http(idx, "GET", "/v1/blocks", None);
    assert_eq!(code, 200, "explorer blocks fetch failed: {body}");
    body["blocks"]
        .as_array()
        .expect("blocks is an array")
        .iter()
        .map(|b| (b["height"].as_u64().expect("row height"), b.clone()))
        .collect()
}

#[test]
fn solo_validator_survives_crash_and_graceful_restart() {
    // a network of ONE: the sharpest recovery case — no peer holds the state,
    // so local recovery is the only path back.
    let mut cluster = Cluster::new(&[0], &[0]);
    // pin the periodic checkpoint far out so every sealed block stays in the
    // journal-replay window: the explorer-continuity assertions below must
    // exercise the boot fold's row REBUILD deterministically, not race the
    // default cadence (a checkpoint landing right after a write would shrink
    // the replay suffix and leave the rows to the index's fsync timing).
    cluster.extra_toml.push("checkpoint_blocks = 100000".into());
    cluster.spawn(0);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(30));

    // real state across the substrates this checkpoint has to cover. `tasks`
    // is Backing::Store: its state comes back from the REOPENED qmdb store,
    // not from a checkpoint snapshot (crates/noded/src/compose.rs:244-246),
    // so what this suite proves is the store lane plus the explorer-row
    // rebuild and the op-blob re-stage. the Map/snapshot-install limb is
    // covered at kernel level by
    // crates/kernel/recovery/tests/restart_replay.rs:231-239.
    write_and_confirm(&cluster, 0, "who", "ducktape");
    write_and_confirm(&cluster, 0, "where", "a-worktree");
    let before = cluster.status(0);
    let root_hash_before = before["root_hash"]
        .as_str()
        .expect("status root_hash")
        .to_string();
    let height_before = before["height"].as_u64().expect("status height");

    // snapshot the drain-built explorer rows before the crash: both writes'
    // rows must be visible (the row lands in the same drain pass that
    // finalized the op, but the confirm reads canonical state — poll the
    // tiny gap away).
    let rows_before = cluster.await_committed(
        0,
        "both drain rows visible in /v1/blocks",
        Duration::from_secs(30),
        || {
            let rows = block_rows(&cluster, 0);
            let both_writes_have_rows =
                carries_write(&rows, "who") && carries_write(&rows, "where");
            both_writes_have_rows.then_some(rows)
        },
    );

    // ---- crash: SIGKILL, no goodbye ------------------------------------
    // SYNCHRONIZED on a sealed boundary, and that is load-bearing. `tasks` is
    // Backing::Store: it commits the block's writes to its OWN disk during
    // apply (`wasm-host/src/lib.rs:1326`), and the block's `Record::Seal` is
    // only appended AFTER that. a SIGKILL inside that window leaves the store
    // at a root no journaled seal vouches for, and Store returns `None` from
    // `durable_commit_height` (`wasm-host/src/lib.rs:978-981`) so
    // `trailing::seed_trailing_claims` can never floor it — recovery
    // fail-stops with `Error::Torn`. `directory` (Backing::Map) could never be
    // in that state: it is reinstalled from the checkpoint snapshot, so it is
    // at-pre by construction. the node-level gap is tracked separately; this
    // suite tests RECOVERY, not that window. so wait for the node to apply
    // PAST the block that carried the last write — idle views finalize empty
    // proposals, so a solo validator advances on its own, and that next
    // pre-apply is what makes the pending seal durable.
    cluster.await_committed(
        0,
        "the node to apply past the block that carried the last write",
        Duration::from_secs(30),
        || {
            let sealed_past_the_writes =
                cluster.status(0)["height"].as_u64().expect("status height") > height_before;
            sealed_past_the_writes.then_some(())
        },
    );
    cluster.kill(0);

    // respawn over the SAME storage dir. the node must RECOVER, not re-run
    // genesis: the greppable marker flips from `genesis root_hash=` to
    // `recovered root_hash=`.
    cluster.spawn(0);
    let recovered = cluster.wait_marker(0, "recovered root_hash=", Duration::from_secs(30));
    let recovered_hash = recovered.split_whitespace().next().expect("recovered hash");
    assert_eq!(
        recovered_hash, root_hash_before,
        "recovered root-hash must be byte-identical to the pre-crash boundary"
    );
    assert!(
        cluster.marker(0, "genesis root_hash=").is_none(),
        "a restart must not re-run genesis"
    );

    // the recovered boundary answers status identically... (height is
    // monotone, not frozen: idle views finalize empty proposals that drain
    // as deterministic rejections, so a view may seal between the status
    // read and the kill — state-identical, height +n.)
    let after =
        cluster.await_committed(0, "status after recovery", Duration::from_secs(30), || {
            let s = cluster.status(0);
            (s["root_hash"] == before["root_hash"]).then_some(s)
        });
    assert!(after["height"].as_u64().expect("height") >= height_before);
    assert_eq!(task_title(&cluster, 0, "who").as_deref(), Some("ducktape"));
    assert_eq!(
        task_title(&cluster, 0, "where").as_deref(),
        Some("a-worktree")
    );

    // the explorer survives the crash window: the SIGKILL almost certainly
    // beat the index's periodic fsync, so the boot fold must have REBUILT the
    // lost rows from the journaled frames — byte-identical to what the drain
    // wrote (same shared row seam), never merely "some row at that height".
    let rows_after = cluster.await_committed(
        0,
        "explorer rows to reappear after recovery",
        Duration::from_secs(30),
        || {
            let rows = block_rows(&cluster, 0);
            (rows.len() >= rows_before.len()).then_some(rows)
        },
    );
    // the rebuilt fold row matches the drain row on block coordinates and every
    // op's identity; the only difference is the per-op dispatch TRACE
    // (`ops[].operations`), which is EMPTY on the fold — recovery carries the
    // block-level aggregate trace, not per-member, so a replayed op cannot
    // reproduce its own fan-out. a documented rebuild-only degradation; compare
    // with the traces stripped.
    let strip_traces = |r: &serde_json::Value| {
        let mut r = r.clone();
        if let Some(ops) = r["ops"].as_array_mut() {
            for op in ops {
                op["operations"] = serde_json::json!([]);
            }
        }
        r
    };
    for (height, row) in &rows_before {
        let recovered = rows_after
            .iter()
            .find(|(h, _)| h == height)
            .map(|(_, r)| r)
            .unwrap_or_else(|| panic!("explorer row at height {height} lost across the crash"));
        assert_eq!(
            strip_traces(recovered),
            strip_traces(row),
            "rebuilt explorer row at height {height} must equal the drain's row \
             (modulo the fold's empty per-op dispatch trace)"
        );
    }
    // and the rebuilt rows' op payloads are dereferencable again — the blob
    // store is in-memory, so ONLY the fold's re-staging can answer this.
    let filtered = rows_before
        .iter()
        .flat_map(|(_, r)| ops_targeting(r, "tasks"))
        .collect::<Vec<_>>();
    assert!(
        !filtered.is_empty(),
        "no explorer rows targeted `tasks` — the poll above and this filter must name the same tenant"
    );
    for op in filtered {
        let op_hash = op["op_hash"].as_str().expect("row op_hash");
        let (code, _) = cluster.http(0, "GET", &format!("/v1/files/blob/{op_hash}"), None);
        assert_eq!(
            code, 200,
            "op payload blob must re-stage during the boot fold"
        );
    }

    // ...and the engine is LIVE again over its reopened journal: new ops
    // finalize (a same-epoch respawn that lost its vote state would refuse
    // or double-vote; one that never resumed would park this forever).
    write_and_confirm(&cluster, 0, "after-crash", "still-here");

    // ---- graceful: rpc shutdown (final checkpoint + journal barrier) ----
    let reply = cluster.rpc(0, serde_json::json!({ "cmd": "shutdown" }));
    assert_eq!(reply["ok"], true, "shutdown rpc failed: {reply}");
    cluster.wait_exit(0, Duration::from_secs(15));

    cluster.spawn(0);
    cluster.wait_marker(0, "recovered root_hash=", Duration::from_secs(30));
    assert!(
        cluster.marker(0, "genesis root_hash=").is_none(),
        "the second restart must not re-run genesis either"
    );
    // everything from BOTH earlier lives is present...
    cluster.await_committed(
        0,
        "post-shutdown state to answer",
        Duration::from_secs(30),
        || (task_title(&cluster, 0, "after-crash").as_deref() == Some("still-here")).then_some(()),
    );
    assert_eq!(task_title(&cluster, 0, "who").as_deref(), Some("ducktape"));
    // ...and the network keeps running as scheduled.
    write_and_confirm(&cluster, 0, "after-shutdown", "and-again");
}

/// the SIGTERM/SIGINT graceful-checkpoint arm — the fix's ACTUAL production
/// trigger. the desktop shell SIGTERMs the daemon on app quit; before the fix
/// that tore the node down mid-block and could brick a solo genesis node (the
/// disk substrate left ahead of the last in-memory checkpoint). now a SIGTERM
/// is one more branch of the run loop that runs the SAME graceful sequence as an
/// rpc `Shutdown` (a final manifest + journal barrier) and exits 0. this drives
/// it over a REAL OS signal on a REAL process: SIGTERM a live solo node, assert
/// it logs the signal arm and exits on its own, restart, and assert recovery to
/// the byte-identical tip with no brick. (restart_e2e is pre-existing-flaky
/// under load, so the timeouts are deliberately generous.)
#[test]
fn solo_validator_survives_sigterm_restart() {
    let mut cluster = Cluster::new(&[0], &[0]);
    cluster.spawn(0);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(30));

    // land real state the checkpoint must carry across the signal.
    write_and_confirm(&cluster, 0, "quit", "gracefully");
    write_and_confirm(&cluster, 0, "via", "sigterm");
    let before = cluster.status(0);
    let root_hash_before = before["root_hash"]
        .as_str()
        .expect("status root_hash")
        .to_string();
    let height_before = before["height"].as_u64().expect("status height");

    // ---- SIGTERM: the desktop-quit signal, not a SIGKILL --------------------
    cluster.term(0);
    // the signal arm announces itself, runs the graceful checkpoint, exits 0.
    cluster.wait_marker(
        0,
        "SIGTERM/SIGINT — graceful checkpoint then exit",
        Duration::from_secs(30),
    );
    cluster.wait_exit(0, Duration::from_secs(30));

    // ---- restart: must RECOVER (not re-run genesis) to the exact tip --------
    cluster.spawn(0);
    let recovered = cluster.wait_marker(0, "recovered root_hash=", Duration::from_secs(30));
    let recovered_hash = recovered.split_whitespace().next().expect("recovered hash");
    assert_eq!(
        recovered_hash, root_hash_before,
        "recovered root-hash after a graceful SIGTERM must be byte-identical to the pre-signal boundary"
    );
    assert!(
        cluster.marker(0, "genesis root_hash=").is_none(),
        "a SIGTERM restart must not re-run genesis (no brick)"
    );

    let after = cluster.await_committed(
        0,
        "status after sigterm recovery",
        Duration::from_secs(30),
        || {
            let s = cluster.status(0);
            (s["root_hash"] == before["root_hash"]).then_some(s)
        },
    );
    assert!(after["height"].as_u64().expect("height") >= height_before);
    // both pre-signal writes survived the graceful checkpoint.
    assert_eq!(
        task_title(&cluster, 0, "quit").as_deref(),
        Some("gracefully")
    );
    assert_eq!(task_title(&cluster, 0, "via").as_deref(), Some("sigterm"));
    // and the engine is LIVE again over its reopened journal.
    write_and_confirm(&cluster, 0, "after-sigterm", "still-here");
}

// ---- duckfs restart proof --------------------------------------------------
//
// duckfs (the `files` module) is a DISK-COHORT module: unlike the in-memory
// canonical-bytes cohort (the modules the checkpoint stores bytes for), it
// commits its content-addressed objects and its refs file to disk PER BLOCK
// (the task-6 durability ordering: flush objects -> fsync dirs -> save refs ->
// adopt), and recovers them itself via
// `Files::open` rather than from the checkpoint snapshot (it is `ResolverBacked`,
// so the checkpoint stores no bytes for it — exactly like the qmdb modules). the
// property this proves is the one the OLD cas module failed: committed file
// BYTES survive a hard SIGKILL and are readable again after recovery.

/// a b64 inline-file change (the module chunks + hashes the bytes into odb
/// objects at commit time).
fn put_inline(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Inline {
            b64: STANDARD.encode(bytes),
        },
    }
}

/// the lowercase-hex object id of a raw chunk — the digest a `Content::Chunks`
/// commit references and the putblob frame stages.
fn chunk_hex(bytes: &[u8]) -> String {
    to_hex(&object_id(Kind::Chunk, bytes))
}

/// a distinctive, non-uniform byte pattern of length `len` (251 is prime, so it
/// aligns with no power-of-two boundary — a truncated or corrupt recovery is
/// caught, not masked).
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn files_stat(cluster: &Cluster, idx: usize, path: &str) -> Option<EntryInfo> {
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

fn files_read(cluster: &Cluster, idx: usize, path: &str, offset: u64, len: u64) -> Option<Vec<u8>> {
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

fn files_refs(cluster: &Cluster, idx: usize) -> Option<RefsInfo> {
    let reply = cluster.query(idx, "files", &files_encode_query(&FilesQuery::Refs {}))?;
    match files_decode_reply(&reply).ok()? {
        FilesReply::Refs(info) => Some(info),
        _ => None,
    }
}

/// the committed head snapshot id (the base a follow-up commit threads and the
/// pin target), once the node has one.
fn files_head(cluster: &Cluster, idx: usize) -> Option<String> {
    files_refs(cluster, idx)?.head
}

#[test]
fn solo_validator_duckfs_bytes_survive_crash() {
    // a network of ONE — no peer holds the duckfs objects, so local disk
    // recovery is the only path back to the bytes.
    let mut cluster = Cluster::new(&[0], &[0]);
    // CHECKPOINT CADENCE is load-bearing here. duckfs commits to disk per block
    // while the in-memory cohort only persists at a checkpoint; recovery's
    // selective replay heals a disk substrate at most ONE torn block ahead of
    // the last checkpoint. pinning a checkpoint every block keeps duckfs and the
    // checkpoint in lockstep, so a SIGKILL always lands in that single-torn-block
    // regime (a far-pinned checkpoint would leave duckfs many blocks ahead of a
    // genesis manifest, which replay-from-genesis cannot reconcile by root alone).
    cluster.extra_toml.push("checkpoint_blocks = 1".into());
    cluster.spawn(0);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(30));

    // ---- seed non-trivial duckfs state --------------------------------------
    // (1) two inline files in nested dirs, one commit off the empty tree.
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: None,
            message: "seed inline".into(),
            changes: vec![
                put_inline("/shared/a", b"alpha"),
                put_inline("/shared/dir/b", b"beta"),
            ],
        }),
    );
    cluster.await_committed(
        0,
        "inline duckfs files to finalize",
        Duration::from_secs(30),
        || files_stat(&cluster, 0, "/shared/a").map(|_| ()),
    );
    let s1 = cluster.await_committed(
        0,
        "head after the inline commit",
        Duration::from_secs(30),
        || files_head(&cluster, 0),
    );

    // (2) a file whose bytes are STAGED as a chunk object via putblob and then
    // referenced by digest in a `Content::Chunks` commit — the odb-object byte
    // path. 128 KiB is a real, multi-page odb object that keeps THIS test about
    // restart recovery, not payload size; the full-CHUNK_SIZE multi-chunk graph
    // over the op path is large_file_e2e's job (#215: the binary frame codec
    // carries a 1 MiB chunk whole). same-origin submits finalize in seq order,
    // so the putblob is durable before the commit that references it executes.
    let chunk = pattern(128 * 1024);
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
                    chunks: vec![chunk_hex(&chunk)],
                },
            }],
        }),
    );
    cluster.await_committed(
        0,
        "chunked duckfs file to finalize",
        Duration::from_secs(30),
        || {
            files_stat(&cluster, 0, "/shared/big")
                .filter(|e| e.size == chunk_size)
                .map(|_| ())
        },
    );
    let s2 = cluster.await_committed(
        0,
        "head after the chunked commit",
        Duration::from_secs(30),
        || files_head(&cluster, 0),
    );

    // (3) pin the head — a gc root that must survive the crash intact.
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Pin {
            snapshot: s2.clone(),
            name: "release".into(),
        }),
    );
    cluster.await_committed(0, "duckfs pin to finalize", Duration::from_secs(30), || {
        files_refs(&cluster, 0)
            .filter(|r| r.pins.contains_key("release"))
            .map(|_| ())
    });

    // the bytes read back BEFORE the crash — the exact baseline recovery must
    // reproduce.
    let chunk_before = files_read(&cluster, 0, "/shared/big", 0, chunk_size).expect("read chunked");
    assert_eq!(chunk_before, chunk, "pre-crash chunked file bytes");
    assert_eq!(
        files_read(&cluster, 0, "/shared/a", 0, 64).as_deref(),
        Some(b"alpha".as_ref())
    );
    let root_hash_before = cluster.status(0)["root_hash"]
        .as_str()
        .expect("status root_hash")
        .to_string();

    // ---- crash: SIGKILL, no goodbye -----------------------------------------
    cluster.kill(0);
    cluster.spawn(0);
    let recovered = cluster.wait_marker(0, "recovered root_hash=", Duration::from_secs(30));
    let recovered_hash = recovered.split_whitespace().next().expect("recovered hash");
    assert_eq!(
        recovered_hash, root_hash_before,
        "recovered root-hash must be byte-identical to the pre-crash boundary (duckfs root included)"
    );
    assert!(
        cluster.marker(0, "genesis root_hash=").is_none(),
        "a duckfs restart must not re-run genesis"
    );

    // ---- THE property: the committed duckfs BYTES are readable after recovery.
    // the odb objects + the refs envelope came back from disk via `Files::open`;
    // an empty or torn odb would error on Read, and a lost refs file would drop
    // the head/pin. this is exactly what the old cas module could not survive.
    let chunk_after = cluster.await_committed(
        0,
        "chunked duckfs bytes after recovery",
        Duration::from_secs(30),
        || files_read(&cluster, 0, "/shared/big", 0, chunk_size),
    );
    assert_eq!(
        chunk_after, chunk,
        "chunked file bytes survived SIGKILL byte-identical"
    );
    assert_eq!(
        files_read(&cluster, 0, "/shared/a", 0, 64).as_deref(),
        Some(b"alpha".as_ref()),
        "inline file /shared/a survived"
    );
    assert_eq!(
        files_read(&cluster, 0, "/shared/dir/b", 0, 64).as_deref(),
        Some(b"beta".as_ref()),
        "nested inline file /shared/dir/b survived"
    );
    // the head and the pin (gc root) recovered to a CONSISTENT root — the refs
    // file is the atomic commit point, so recovering it means the whole tree is
    // consistent (the deterministic object-flush-vs-refs-rename torn point is
    // fault-injected at the module level in files/tests/disk.rs; here a real
    // SIGKILL exercises the crash-window boundary end to end).
    let refs = files_refs(&cluster, 0).expect("refs after recovery");
    assert_eq!(
        refs.head.as_deref(),
        Some(s2.as_str()),
        "duckfs head survived"
    );
    assert_eq!(
        refs.pins.get("release").map(String::as_str),
        Some(s2.as_str()),
        "duckfs pin survived"
    );

    // ...and the engine is LIVE again over its reopened journal: a fresh duckfs
    // commit finalizes, so recovery both restored the bytes AND resumed consensus.
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: Some(s2.clone()),
            message: "after crash".into(),
            changes: vec![put_inline("/shared/c", b"gamma")],
        }),
    );
    cluster.await_committed(
        0,
        "post-crash duckfs commit to finalize",
        Duration::from_secs(30),
        || files_stat(&cluster, 0, "/shared/c").map(|_| ()),
    );
}

#[test]
fn solo_validator_duckfs_survives_multi_block_history_crash_at_default_cadence() {
    // the sibling to `solo_validator_duckfs_bytes_survive_crash`, at the SHIPPED
    // checkpoint cadence instead of the `checkpoint_blocks = 1` workaround. duckfs
    // commits to disk per block while the checkpoint persists only periodically
    // (DEFAULT_CHECKPOINT_BLOCKS = 32), so several commits since genesis leave the
    // disk substrate MANY blocks ahead of the single genesis checkpoint. before
    // the recovery forward-scan fix this SIGKILL bricked boot with `Error::Torn`;
    // now recovery seeds each disk module's durable floor from the LATEST sealed
    // post-root it exactly matches and replays only strictly above it. no
    // `checkpoint_blocks` override — the default cadence IS the point.
    let mut cluster = Cluster::new(&[0], &[0]);
    cluster.spawn(0);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(30));

    // THREE sequential duckfs commits — three per-block-durable disk blocks past
    // the genesis checkpoint (the multi-block history the review flagged). paths
    // sit under `/shared/**`, the publicly-writable root (see files/src/paths.rs).
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: None,
            message: "h1".into(),
            changes: vec![put_inline("/shared/hist/a", b"one")],
        }),
    );
    let s1 = cluster.await_committed(0, "commit 1 head", Duration::from_secs(30), || {
        files_stat(&cluster, 0, "/shared/hist/a").and_then(|_| files_head(&cluster, 0))
    });
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: Some(s1.clone()),
            message: "h2".into(),
            changes: vec![put_inline("/shared/hist/b", b"two")],
        }),
    );
    let s2 = cluster.await_committed(0, "commit 2 head", Duration::from_secs(30), || {
        files_stat(&cluster, 0, "/shared/hist/b").and_then(|_| files_head(&cluster, 0))
    });
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: Some(s2.clone()),
            message: "h3".into(),
            changes: vec![put_inline("/shared/hist/c", b"three")],
        }),
    );
    cluster.await_committed(0, "commit 3 to finalize", Duration::from_secs(30), || {
        files_stat(&cluster, 0, "/shared/hist/c").map(|_| ())
    });

    let root_hash_before = cluster.status(0)["root_hash"]
        .as_str()
        .expect("status root_hash")
        .to_string();

    // ---- crash: SIGKILL, no goodbye — the disk is 3 blocks past the checkpoint.
    cluster.kill(0);
    cluster.spawn(0);
    let recovered = cluster.wait_marker(0, "recovered root_hash=", Duration::from_secs(30));
    let recovered_hash = recovered.split_whitespace().next().expect("recovered hash");
    assert_eq!(
        recovered_hash, root_hash_before,
        "a disk substrate several blocks ahead of the checkpoint must recover \
         byte-identically at the DEFAULT cadence (no checkpoint_blocks=1 crutch)"
    );
    assert!(
        cluster.marker(0, "genesis root_hash=").is_none(),
        "recovery must not re-run genesis"
    );

    // every historical block's bytes came back from disk.
    assert_eq!(
        files_read(&cluster, 0, "/shared/hist/a", 0, 64).as_deref(),
        Some(b"one".as_ref())
    );
    assert_eq!(
        files_read(&cluster, 0, "/shared/hist/b", 0, 64).as_deref(),
        Some(b"two".as_ref())
    );
    assert_eq!(
        files_read(&cluster, 0, "/shared/hist/c", 0, 64).as_deref(),
        Some(b"three".as_ref())
    );

    // and consensus resumed over the reopened journal.
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: files_head(&cluster, 0),
            message: "post".into(),
            changes: vec![put_inline("/shared/hist/d", b"four")],
        }),
    );
    cluster.await_committed(
        0,
        "post-crash commit to finalize",
        Duration::from_secs(30),
        || files_stat(&cluster, 0, "/shared/hist/d").map(|_| ()),
    );
}
