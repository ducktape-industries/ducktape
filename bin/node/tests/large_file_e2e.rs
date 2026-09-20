//! #215 — files larger than 1 MiB over the REAL submit/consensus path.
//!
//! the duckfs chunk invariant (`verify_chunk_len`) requires every interior
//! chunk of a multi-chunk file to be EXACTLY `CHUNK_SIZE` (1 MiB), so a file
//! larger than 1 MiB forces full-CHUNK_SIZE putblob ops through the ordered lane and the
//! p2p payload gossip. this is exactly the op the old json frame codec
//! expanded ~3.57x past the p2p message cap, tripping commonware's internal
//! size assert on the proposer's gossip task (a panic, not a rejection) —
//! which made every file larger than 1 MiB uncommittable.
//!
//! NOT the in-process `duckfs_resolver` shortcut: a real two-validator
//! cluster of OS processes over real sockets, submitting via rpc, reading
//! back on the NON-submitting node so the bytes provably crossed consensus.
//!
//! the companion property: an op whose frame can never fit the cap is
//! rejected CLEANLY at the submit boundary (rpc ok=false) and both
//! validators keep finalizing afterwards.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::Cluster;
use files::{
    CHUNK_SIZE, Change, Content, EntryInfo, FilesMsg, FilesQuery, FilesReply, Kind,
    decode_reply as files_decode_reply, encode_msg as files_encode_msg, encode_putblob,
    encode_query as files_encode_query, objects::object_id, to_hex,
};
use common::wire::tasks::{TaskMsg, TaskQuery, TaskReply, decode_task_reply, encode_task_msg, encode_task_query};

/// a distinctive, non-uniform byte pattern (251 is prime, so it aligns with no
/// power-of-two boundary — truncation or chunk-order corruption is caught, not
/// masked; and no codec wins by luck on runs of zeros).
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// the lowercase-hex object id of a raw chunk — the digest a `Content::Chunks`
/// commit references and the putblob frame stages.
fn chunk_hex(bytes: &[u8]) -> String {
    to_hex(&object_id(Kind::Chunk, bytes))
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

/// read the whole file in sub-`MAX_READ_BYTES` pages and return the bytes.
fn files_read_all(cluster: &Cluster, idx: usize, path: &str, total: u64) -> Vec<u8> {
    const PAGE: u64 = 512 * 1024;
    let mut out = Vec::with_capacity(total as usize);
    let mut offset = 0;
    while offset < total {
        let len = PAGE.min(total - offset);
        let page = cluster.await_committed(
            idx,
            &format!("read {path} @{offset}+{len} on node {idx}"),
            Duration::from_secs(30),
            || files_read(cluster, idx, path, offset, len),
        );
        assert!(
            !page.is_empty(),
            "read at {offset} returned no bytes before eof (total {total})"
        );
        offset += page.len() as u64;
        out.extend_from_slice(&page);
    }
    out
}

fn task_title(cluster: &Cluster, idx: usize, task_id: &str) -> Option<String> {
    let req = encode_task_query(&TaskQuery::Get {
        task_id: task_id.into(),
    });
    let reply = cluster.query(idx, "tasks", &req)?;
    match decode_task_reply(&reply).ok()? {
        TaskReply::Task(task) => task.map(|t| t.title),
        TaskReply::Tasks(_) | TaskReply::OwnerOpenCount(_) => None,
    }
}

#[test]
fn multi_chunk_file_commits_and_reads_across_the_cluster() {
    // TWO validators: the proposer must gossip the full frame bytes to a real
    // peer, and the read-back on the other node proves the bytes crossed
    // consensus — not a local shortcut.
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.spawn(0);
    cluster.spawn(1);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(30));
    cluster.wait_marker(1, "genesis root_hash=", Duration::from_secs(30));

    // a file spanning two FULL 1 MiB chunks plus an odd tail — the smallest
    // shape that forces the exact-CHUNK_SIZE interior chunks #215 is about.
    let chunk = CHUNK_SIZE as usize;
    let bytes = pattern(2 * chunk + 512 * 1024 + 7);
    let chunks: Vec<&[u8]> = bytes.chunks(chunk).collect();
    assert_eq!(chunks.len(), 3, "two full interior chunks + tail");

    // stage every chunk via putblob through the ordered lane (same-origin
    // submits finalize in seq order, so the blobs are durable before the
    // commit that references them executes). the write must land under
    // /shared (or /home/<owner>): anything else fails the path-authority
    // check and the whole commit deterministically rejects.
    for c in &chunks {
        cluster.submit(0, "files", &encode_putblob(c));
    }
    cluster.submit(
        0,
        "files",
        &files_encode_msg(&FilesMsg::Commit {
            base_snapshot: None,
            message: "large file over consensus".into(),
            changes: vec![Change::Put {
                path: "/shared/big".into(),
                exec: false,
                meta: BTreeMap::new(),
                content: Content::Chunks {
                    size: bytes.len() as u64,
                    chunks: chunks.iter().map(|c| chunk_hex(c)).collect(),
                },
            }],
        }),
    );

    // the SUBMITTING validator applies the commit (narrows a failure: local
    // apply vs cross-node propagation)...
    let mut blocks = cluster.block_feed(0, Duration::from_secs(60));
    let local = loop {
        if let Some(s) = files_stat(&cluster, 0, "/shared/big") {
            break s;
        }
        if blocks.next_block().is_err() {
            for idx in [0, 1] {
                let (code, body) = cluster.http(idx, "GET", "/v1/blocks", None);
                eprintln!("node {idx} /v1/blocks ({code}): {body}");
            }
            panic!(
                "submitting validator never showed the large file;\n{}",
                cluster.all_log_tails(80)
            );
        }
    };
    assert_eq!(local.size, bytes.len() as u64, "committed size matches");

    // ...and the NON-submitting validator sees the committed file at full size
    // (a hand-rolled wait on its block feed: on timeout, dump both explorers +
    // log tails so a cross-node propagation failure is diagnosable from the
    // test output).
    let mut blocks = cluster.block_feed(1, Duration::from_secs(60));
    let stat = loop {
        if let Some(s) = files_stat(&cluster, 1, "/shared/big") {
            break s;
        }
        if blocks.next_block().is_err() {
            for idx in [0, 1] {
                let (code, body) = cluster.http(idx, "GET", "/v1/blocks", None);
                eprintln!("node {idx} /v1/blocks ({code}): {body}");
            }
            panic!(
                "peer validator never showed the large file;\n{}",
                cluster.all_log_tails(80)
            );
        }
    };
    assert_eq!(stat.size, bytes.len() as u64, "committed size matches");

    // ...and BOTH validators serve back the identical bytes.
    for idx in [0, 1] {
        let got = files_read_all(&cluster, idx, "/shared/big", bytes.len() as u64);
        assert_eq!(
            got.len(),
            bytes.len(),
            "node {idx} returns the full byte length"
        );
        assert_eq!(got, bytes, "node {idx} returns byte-identical content");
    }
}

#[test]
fn oversized_op_rejects_cleanly_and_the_cluster_stays_live() {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.spawn(0);
    cluster.spawn(1);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(30));
    cluster.wait_marker(1, "genesis root_hash=", Duration::from_secs(30));

    // an op no frame codec can fit under the cap: double the max chunk. the
    // submit boundary must reject it as a plain rpc error — never accept it
    // onto the wire path where the p2p size assert would kill the proposer.
    let oversized = pattern(2 * CHUNK_SIZE as usize);
    let reply = cluster.try_submit(0, "files", &encode_putblob(&oversized));
    assert_eq!(
        reply["ok"], false,
        "an oversized op must be REJECTED at submit, got: {reply}"
    );

    // the rejection is clean: both validators keep accepting and finalizing.
    cluster.submit(
        0,
        "tasks",
        &encode_task_msg(&TaskMsg::CreateTask {
            task_id: "alive".into(),
            title: "yes".into(),
            owner: None,
        }),
    );
    for idx in [0, 1] {
        cluster.await_committed(
            idx,
            &format!("post-rejection finalization visible on node {idx}"),
            Duration::from_secs(60),
            || (task_title(&cluster, idx, "alive").as_deref() == Some("yes")).then_some(()),
        );
    }
}
