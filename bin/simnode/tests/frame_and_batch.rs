//! the signed-frame lane and the multi-op batch lane against the sim: authorship
//! that is CRYPTOGRAPHIC (a verified ed25519 signer) rather than the trusted-
//! client origin string, and the host's `submit_block` batch engine — N ops in
//! ONE block with per-op isolation. these are node semantics the app never
//! surfaces (no console composes a frame or a batch), so neither fleet live-QA
//! nor the TS scenario lane can reach them; the sim verifies EXACTLY as the real
//! daemon does (the same `node::decode_frame`), and its simple direct-host lane
//! is where the batch engine's abort-all-and-replay member isolation is pinned
//! e2e for the first time.

mod harness;

use commonware_cryptography::Signer as _;
use harness::{Sim, create, create_channel};
use sdk::Msg;
use simnode::module_contracts::files;

type Ed = commonware_cryptography::ed25519::PrivateKey;

/// a chat op as a frame's inner `Msg` — the payload bytes are the module `*Msg`
/// json, exactly what the frameless `/v1/submit` lane would encode.
fn chat_frame_op(payload: serde_json::Value) -> Msg {
    Msg {
        target: "chat".into(),
        payload: serde_json::to_vec(&payload).expect("chat payload serializes"),
    }
}

/// a files Commit carrying one Mkdir under `/shared` (public, so any actor has
/// write authority — see `paths::check_authority`). the commit AUTHOR is
/// origin-derived, never taken from the payload, so this is the op whose
/// authorship the frame vs frameless lanes are compared on. no base64 — a Mkdir
/// change needs no inline content.
fn files_mkdir_commit(path: &str, message: &str) -> Msg {
    Msg {
        target: "files".into(),
        payload: files::encode_msg(&files::FilesMsg::Commit {
            base_snapshot: None,
            message: message.into(),
            changes: vec![files::Change::Mkdir { path: path.into() }],
        }),
    }
}

/// the `ext:`-prefixed actor string a raw external key renders as (the cross-
/// module authorship convention — see `sdk::Origin::actor_string`).
fn ext(bytes: &[u8]) -> String {
    format!("ext:{}", noded::hex_bytes(bytes))
}

// ── E3 — the signed-frame lane ──────────────────────────

/// a real signed frame commits under the frame's VERIFIED signer — authorship
/// no field of the request could have claimed, because the origin is inside the
/// signed preimage. the sim verifies with the SAME `node::decode_frame` every
/// validator uses.
#[test]
fn a_signed_frame_commits_under_its_verified_signer() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);

    let signer = Ed::from_seed(7);
    let frame = node::encode_frame(
        &signer,
        1,
        &chat_frame_op(create_channel("general", "General")),
    );
    let (code, receipt) = sim.submit_frame(&frame);
    assert_eq!(code, 200, "a valid frame commits: {receipt}");
    assert_eq!(receipt["height"], 1, "the frame's op landed in block 1");
    // the receipt addresses the op PAYLOAD, exactly as the frameless lane does.
    assert_eq!(
        receipt["op_hash"].as_str().map(str::len),
        Some(64),
        "the frame lane returns the same receipt shape: {receipt}"
    );

    // the committed op's authorship IS the signer's key: the block row's
    // proposer is the verified signer's public key (the explorer's authenticated
    // author field), which no part of the request could have named.
    let blocks = sim.request("GET", "/v1/blocks", None).1;
    let last = blocks["blocks"]
        .as_array()
        .and_then(|b| b.last())
        .expect("one block");
    assert_eq!(
        last["ops"][0]["proposer"],
        noded::hex_bytes(signer.public_key().as_ref()),
        "the block row author IS the verified signer: {last}"
    );
    assert_eq!(last["ops"][0]["target"], "chat");
}

/// a frame whose payload was tampered no longer verifies — the http gate refuses
/// it with the codec's verbatim reason, and NO block is minted (the actor never
/// sees it).
#[test]
fn a_tampered_frame_is_refused_with_no_block() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);

    let signer = Ed::from_seed(7);
    let mut frame = node::encode_frame(
        &signer,
        1,
        &chat_frame_op(create_channel("general", "General")),
    );
    // flip one PAYLOAD byte: the trailing bytes are the ed25519 proof (64), so
    // the payload's last byte sits at len - 65. the proof binds the whole
    // preimage (scheme, origin, seq, target, payload), so it no longer
    // verifies — and the tamper stays INSIDE the payload, so it is the proof
    // check that refuses, not the frame parser.
    let last = frame.len() - 65;
    frame[last] ^= 0x01;

    let (code, body) = sim.submit_frame(&frame);
    assert_eq!(code, 400, "a tampered frame is refused: {body}");
    let err = body["error"].as_str().expect("a verbatim refusal");
    assert!(
        err.contains("frame proof does not bind this op to its origin"),
        "the refusal names the cause: {err}"
    );
    // no block: the gate stopped it before the actor.
    assert_eq!(sim.status()["height"], 0, "a refused frame mints no block");
}

// ── C8 — user-signed files commits over the frame lane ──

/// a files commit submitted as a signed frame is authored by the VERIFIED signer
/// key; the frameless lane authors by the caller STRING instead. this is the
/// honest, sim-reachable half of #536: the files module derives the commit
/// author from the op ORIGIN (`env.origin.actor_string()`), never the payload,
/// so the frame lane makes that author a cryptographic identity.
#[test]
fn a_files_commit_over_a_frame_is_authored_by_the_verified_key() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);

    // a user-signed commit: the signer key authors it.
    let signer = Ed::from_seed(11);
    let frame = node::encode_frame(&signer, 1, &files_mkdir_commit("/shared/proj", "signed"));
    let (code, receipt) = sim.submit_frame(&frame);
    assert_eq!(code, 200, "the files frame commits: {receipt}");

    // CONTRAST: the frameless lane authors by the trusted-client string. "alice"
    // becomes `ext:<hex of the utf-8 bytes>`, NOT a key — unverified authorship.
    sim.submit_ok(
        "files",
        {
            // a NEW path so the per-path CAS does not fire against /shared/proj.
            let Msg { payload, .. } = files_mkdir_commit("/shared/other", "frameless");
            serde_json::from_slice::<serde_json::Value>(&payload).expect("commit json")
        },
        Some("alice"),
    );

    let history = sim.query("files", serde_json::json!({ "history": { "limit": 10 } }));
    let snaps = history["history"].as_array().expect("history array");
    let key_author = ext(signer.public_key().as_ref());
    let string_author = ext(b"alice");
    assert!(
        snaps.iter().any(|s| s["author"]
            == serde_json::json!(files::Actor::Key(signer.public_key().as_ref().to_vec()))),
        "the frame commit is authored by the VERIFIED key {key_author}: {history}"
    );
    assert!(
        snaps
            .iter()
            .any(|s| s["author"] == serde_json::json!(files::Actor::Key(b"alice".to_vec()))),
        "the frameless commit is authored by the caller string {string_author}: {history}"
    );
    // and the two authorship kinds are genuinely different: a key is not a name.
    assert_ne!(key_author, string_author);
}

/// a BYTE-IDENTICAL frame resubmit does not double-commit — but on the sim it is
/// the files module's own optimistic-concurrency CAS that stops it, NOT a
/// consensus swallow. this pins where the sim's honest reach ends: the #536
/// byte-identical swallow lives in the ordered validator's `OrderedNode`
/// exactly-once digest gate (keyed by `frame_id` = sha256(frame bytes)), which
/// drops the duplicate BEFORE it reaches the host — so on the real node the
/// files CAS never runs and no rejection surfaces. the sim has no such gate, so
/// the duplicate frame reaches the host and the files per-path CAS refuses it
/// (`base = None` = empty tree, but the path already exists at head). both lanes
/// converge on "no second commit"; only the mechanism and the surfaced result
/// differ.
#[test]
fn a_byte_identical_frame_resubmit_is_stopped_by_the_files_cas_not_a_consensus_swallow() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);

    let signer = Ed::from_seed(13);
    let frame = node::encode_frame(&signer, 1, &files_mkdir_commit("/shared/proj", "once"));

    let (code, receipt) = sim.submit_frame(&frame);
    assert_eq!(code, 200, "the first commit lands: {receipt}");
    assert_eq!(sim.status()["height"], 1);

    // the exact same frame bytes again — the validator would swallow this at the
    // FrameId gate; the sim routes it to the host, where the files CAS refuses.
    let (code, body) = sim.submit_frame(&frame);
    assert_eq!(
        code, 400,
        "the resubmit is refused, not double-committed: {body}"
    );
    let err = body["error"].as_str().expect("a rejection reason");
    assert!(
        err.contains("conflict") && err.contains("changed since base"),
        "the files CAS names the conflict (not a consensus swallow): {err}"
    );
    // the resubmit's op did not double-commit — the files CAS refused it — but
    // it JOURNALS its rejected block now (validator parity: a rejected single op
    // rides the drain and seals its own height), so the height advanced to 2. the
    // state was NOT double-committed: only the honest no-op is journaled.
    assert_eq!(
        sim.status()["height"],
        2,
        "the rejected resubmit sealed its own block"
    );
}

// ── E2 — multi-op batch blocks ──────────────────────────

/// an ops-array `/sim/peer-block` commits N members as ONE block: the height
/// advances by exactly one, and the block index carries a single row with N ops.
#[test]
fn an_ops_array_commits_n_members_in_one_block() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &[]);

    let (code, reply) = sim.peer_batch(serde_json::json!([
        { "target": "chat", "payload": create_channel("alpha", "Alpha") },
        { "target": "chat", "payload": create_channel("beta", "Beta") },
        { "target": "chat", "payload": create_channel("gamma", "Gamma") },
    ]));
    assert_eq!(code, 200, "the batch committed: {reply}");

    // ONE block for three ops: height +1, three applied members.
    assert_eq!(reply["height"], 1, "the batch is a single block: {reply}");
    assert_eq!(sim.status()["height"], 1, "height advanced by exactly one");
    let members = reply["members"].as_array().expect("members array");
    assert_eq!(members.len(), 3, "three member verdicts: {reply}");
    assert!(
        members.iter().all(|m| m["disposition"] == "applied"),
        "every member applied: {reply}"
    );

    // the durable block index shows ONE row aggregating the three ops.
    let blocks = sim.request("GET", "/v1/blocks", None).1;
    let last = blocks["blocks"]
        .as_array()
        .and_then(|b| b.last())
        .expect("one block");
    assert_eq!(last["height"], 1);
    assert_eq!(
        last["ops"].as_array().map(Vec::len),
        Some(3),
        "one block row carrying three ops: {last}"
    );
}

/// MEMBER ISOLATION: member 2 of 3 is genuinely rejectable (it re-creates the
/// channel member 1 staged this same block — read-your-writes across members).
/// the block commits with members 1+3 applied and member 2 reported rejected,
/// and member 2's write leaves NO trace — pinning the host's abort-all-and-
/// replay isolation e2e.
#[test]
fn a_rejected_batch_member_is_isolated_and_the_rest_commit() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &[]);

    let (code, reply) = sim.peer_batch(serde_json::json!([
        { "target": "chat", "payload": create_channel("alpha", "Alpha") },
        // member 2 reads member 1's STAGED alpha and rejects it as a duplicate.
        { "target": "chat", "payload": create_channel("alpha", "Alpha again") },
        { "target": "chat", "payload": create_channel("beta", "Beta") },
    ]));
    assert_eq!(
        code, 200,
        "the batch commits despite a rejected member: {reply}"
    );

    let members = reply["members"].as_array().expect("members array");
    assert_eq!(members[0]["disposition"], "applied", "member 1: {reply}");
    assert_eq!(members[2]["disposition"], "applied", "member 3: {reply}");
    assert_eq!(
        members[1]["disposition"], "rejected",
        "member 2 isolated: {reply}"
    );
    assert!(
        members[1]["rejection"]
            .as_str()
            .unwrap_or_default()
            .contains("channel already exists"),
        "the rejection names the real module refusal: {reply}"
    );

    // ONE block even with the rejected member, and it advanced the height once.
    assert_eq!(reply["height"], 1, "the batch is a single block: {reply}");
    assert_eq!(sim.status()["height"], 1);

    // member 2 left NO trace. the point read says this more sharply than the
    // old count did: alpha's committed record still carries member 1's name, so
    // the rejected duplicate neither added a channel NOR overwrote one.
    let alpha = sim.channel("alpha").expect("member 1's alpha committed");
    assert_eq!(
        alpha["name"], "Alpha",
        "member 2's 'Alpha again' left no trace on alpha: {alpha}"
    );
    assert!(sim.channel("beta").is_some(), "member 3's beta committed");
}

/// DETERMINISM: the same N ops as ONE batch block vs as N single blocks reach an
/// identical LOGICAL state (every key resolves to the same value) but a DIFFERENT
/// authenticated root and root-hash. this is the reality worth pinning: kv's
/// substrate is a commonware qmdb with a SEQUENTIAL merkle strategy, so the root
/// commits to the commit-BOUNDARY structure (one commit vs three), not just the
/// key→value map — folding N writes into one block is NOT root-equivalent to N
/// single blocks even for the identical writes. so batching is observable in the
/// root-hash: a joiner must agree on block structure, not merely on final values.
#[test]
fn one_batch_and_n_single_blocks_reach_the_same_values_but_different_roots() {
    // identical genesis on both sims: same valset seed, same module set. kv is
    // registered only under --with-valset.
    let valset = "11".repeat(32);
    let dir_a = tempfile::tempdir().expect("storage dir");
    let dir_b = tempfile::tempdir().expect("storage dir");
    let sim_a = Sim::spawn(dir_a.path(), &["--with-valset", &valset]);
    let sim_b = Sim::spawn(dir_b.path(), &["--with-valset", &valset]);

    let set = |k: &[u8], v: &[u8]| serde_json::json!({ "set": { "key": k.to_vec(), "value": v.to_vec() } });
    let writes: [(&[u8], &[u8]); 3] = [(b"k1", b"v1"), (b"k2", b"v2"), (b"k3", b"v3")];

    // sim A: the three kv sets as ONE batch block.
    let (code, batch) = sim_a.peer_batch(serde_json::json!([
        { "target": "kv", "payload": set(writes[0].0, writes[0].1) },
        { "target": "kv", "payload": set(writes[1].0, writes[1].1) },
        { "target": "kv", "payload": set(writes[2].0, writes[2].1) },
    ]));
    assert_eq!(code, 200, "batch: {batch}");

    // sim B: the SAME three sets as three single blocks.
    for (k, v) in writes {
        sim_b.peer_block("kv", set(k, v), "peer");
    }

    let a = sim_a.status();
    let b = sim_b.status();
    // block structure DIFFERS: one aggregated block vs three.
    assert_eq!(a["height"], 1, "the batch is one block");
    assert_eq!(b["height"], 3, "the singles are three blocks");

    // the LOGICAL state is identical — every key resolves to the same value on
    // both sims (the committed key→value map does not depend on block structure).
    for (k, v) in writes {
        let want = serde_json::json!(v.to_vec());
        assert_eq!(
            sim_a.query("kv", serde_json::json!({ "get": { "key": k.to_vec() } }))["value"],
            want,
            "sim A value for {k:?}"
        );
        assert_eq!(
            sim_b.query("kv", serde_json::json!({ "get": { "key": k.to_vec() } }))["value"],
            want,
            "sim B value for {k:?}"
        );
    }

    // but the authenticated ROOT and root-hash DIFFER: the qmdb sequential merkle
    // commits to the commit-boundary structure, so one block ≠ three blocks even
    // for identical writes. (all OTHER module roots stay genesis-identical, so
    // the root-hash gap is entirely the kv root gap — a clean isolation of the
    // effect.)
    assert_ne!(
        kv_root(&a),
        kv_root(&b),
        "kv roots must differ: block structure is authenticated:\nA {a}\nB {b}"
    );
    assert_ne!(
        a["root_hash"], b["root_hash"],
        "the root-hash reflects the kv root gap"
    );
}

/// one module's committed root from a `/v1/status` projection.
fn module_root(status: &serde_json::Value, id: &str) -> String {
    status["modules"]
        .as_array()
        .expect("modules array")
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} module registered"))["root"]
        .as_str()
        .expect("root hex")
        .to_string()
}

/// the "kv" module's committed root — the original single-module projection.
fn kv_root(status: &serde_json::Value) -> String {
    module_root(status, "kv")
}

// ── E4 — node-key seeding ───────────────────────────────

/// `--node-key` fabricates a mesh identity `status().public_key` serves back —
/// what a consensus op naming a node key (huddle membership) references. no mesh
/// routes behind it.
#[test]
fn node_key_seeds_status_public_key() {
    let storage = tempfile::tempdir().expect("storage dir");
    let key = "ab".repeat(32); // 64 hex, 32 bytes
    let sim = Sim::spawn(storage.path(), &["--node-key", &key]);
    assert_eq!(
        sim.status()["public_key"],
        key,
        "the seeded key is served verbatim"
    );
}

/// without the flag the key stays empty — "no peer-routed features here", the
/// default every other sim run keeps.
#[test]
fn without_node_key_public_key_stays_empty() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &[]);
    assert_eq!(sim.status()["public_key"], "", "no key seeded → empty");
}

/// a malformed `--node-key` fails LOUD at startup rather than seeding junk a
/// client would try to route to. spawned directly (the shared harness asserts a
/// clean startup, which this deliberately is not).
#[test]
fn a_malformed_node_key_fails_loud_at_startup() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ducktape-simnode"))
        .args(["--node-key", "not-hex", "--listen", "127.0.0.1:0"])
        .output()
        .expect("spawn simnode");
    assert!(
        !out.status.success(),
        "a malformed --node-key must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--node-key"),
        "the error names the flag: {err}"
    );
}

// ── E5 — multi-module batch-vs-singles conformance sweep ──

fn kv_set(key: &[u8], value: &[u8]) -> serde_json::Value {
    serde_json::json!({ "set": { "key": key.to_vec(), "value": value.to_vec() } })
}
fn create_page(id: &str) -> serde_json::Value {
    serde_json::json!({ "create_page": { "page_id": id, "title": id } })
}
fn create_task(id: &str) -> serde_json::Value {
    serde_json::json!({ "task": { "create_task": { "task_id": id, "title": id } } })
}

/// drop the block-dependent `keys` (created_at, updated_at) from each object of a
/// reply array, leaving the block-invariant logical identity to compare.
fn strip(array: &serde_json::Value, keys: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        array
            .as_array()
            .expect("reply array")
            .iter()
            .map(|item| {
                let mut item = item.clone();
                if let Some(object) = item.as_object_mut() {
                    for key in keys {
                        object.remove(*key);
                    }
                }
                item
            })
            .collect(),
    )
}

/// PR #546's kv root-divergence finding, generalized into standing insurance: the
/// SAME script run once as ONE N-member batch block and once as N single blocks
/// reaches a converging LOGICAL state, while the authenticated module roots divide
/// by WHAT each root commits to (investigated against the module sources):
///
///   - qmdb-backed modules (kv, pages) commit to the commit-BOUNDARY structure via
///     their commonware sequential-merkle op log, so ONE block ≠ N blocks even for
///     the identical writes — their roots DIFFER. this is the finding, generalized
///     past kv to a second qmdb module.
///   - a plain, content-only module with no embedded block coordinate (the
///     merged gateway's `.duck` handle plane) commits to the key→value map
///     alone — its root is BYTE-IDENTICAL across the two block shapes.
///   - the plain modules that stamp the block's `consensus_time` into their records
///     (tasks, inbox) also DIFFER, but for a reason ORTHOGONAL to qmdb: the batch's
///     one block carries one timestamp, the N singles carry N. their LOGICAL
///     identity still converges once that block-dependent stamp is stripped — so
///     "plain vs qmdb" is NOT the invariant; "content-only AND block-coordinate-
///     free" is.
///
/// the insurance runs both ways: a future module that starts authenticating block
/// structure (or stamps a block coordinate) trips duckdns's byte-identity; one
/// that stops authenticating it trips kv/pages.
#[test]
fn a_multi_module_script_converges_logically_while_qmdb_roots_split_on_block_structure() {
    // identical genesis: same valset seed, same module set (kv is under the flag).
    let valset = "11".repeat(32);
    let dir_a = tempfile::tempdir().expect("storage dir");
    let dir_b = tempfile::tempdir().expect("storage dir");
    let sim_a = Sim::spawn(dir_a.path(), &["--with-valset", &valset]);
    let sim_b = Sim::spawn(dir_b.path(), &["--with-valset", &valset]);

    // the gateway handle account: identity's Create founds account 1 for the
    // 32-byte origin, and set_handle then reads it (across members in the
    // batch, across blocks in the singles). both runs found it deterministically.
    let key = "n".repeat(32);

    // the shared script — identical ops AND origins, so the ONLY difference between
    // the two runs is block structure. (target, payload, origin)
    let script: Vec<(&str, serde_json::Value, String)> = vec![
        ("identity", create("bob"), key.clone()),
        (
            "gateway",
            serde_json::json!({ "set_handle": { "handle": "bob" } }),
            key.clone(),
        ),
        ("kv", kv_set(b"k1", b"v1"), "peer".into()),
        ("kv", kv_set(b"k2", b"v2"), "peer".into()),
        ("pages", create_page("p1"), "peer".into()),
        ("pages", create_page("p2"), "peer".into()),
        ("tasks", create_task("t1"), "peer".into()),
        ("tasks", create_task("t2"), "peer".into()),
        (
            "inbox",
            serde_json::json!({ "mark_read": { "account": 1, "up_to_seq": 0 } }),
            key.clone(),
        ),
        (
            "inbox",
            serde_json::json!({ "clear": { "account": 1, "up_to_seq": 0 } }),
            key.clone(),
        ),
    ];
    let n = script.len() as u64;

    // run A: the whole script as ONE batch block.
    let batch: Vec<serde_json::Value> = script
        .iter()
        .map(|(target, payload, origin)| {
            serde_json::json!({ "target": target, "payload": payload, "origin": origin })
        })
        .collect();
    let (code, reply) = sim_a.peer_batch(serde_json::json!(batch));
    assert_eq!(code, 200, "batch: {reply}");
    assert!(
        reply["members"]
            .as_array()
            .expect("members")
            .iter()
            .all(|m| m["disposition"] == "applied"),
        "every member applied: {reply}"
    );

    // run B: the SAME ops as N single blocks, in the same order.
    for (target, payload, origin) in &script {
        sim_b.peer_block(target, payload.clone(), origin);
    }

    let a = sim_a.status();
    let b = sim_b.status();
    assert_eq!(a["height"], 1, "the batch is one block");
    assert_eq!(b["height"], n, "the singles are {n} blocks");
    assert_ne!(
        a["root_hash"], b["root_hash"],
        "the root-hash reflects the diverging roots"
    );

    // qmdb-backed modules authenticate the commit boundary → roots DIFFER.
    for id in ["kv", "pages"] {
        assert_ne!(
            module_root(&a, id),
            module_root(&b, id),
            "{id} is qmdb-backed: its root commits to block structure, so 1 block != {n} blocks"
        );
    }

    // a plain, content-only, block-coordinate-free module → BYTE-IDENTICAL root.
    // (a future duckdns that went qmdb-backed or stamped a block coordinate fails
    // HERE — the standing insurance.)
    assert_eq!(
        module_root(&a, "gateway"),
        module_root(&b, "gateway"),
        "duckdns commits to content alone — its root is batch-invariant"
    );

    // Tasks stamps consensus_time, independently of the qmdb commit boundary.
    assert_ne!(
        module_root(&a, "tasks"),
        module_root(&b, "tasks"),
        "tasks authenticates the block's consensus_time (orthogonal to qmdb)"
    );

    // LOGICAL STATE CONVERGES. the time-free modules converge fully — every key
    // resolves to the same value, the page list is byte-identical.
    for k in [b"k1".as_slice(), b"k2".as_slice()] {
        let query = serde_json::json!({ "get": { "key": k.to_vec() } });
        assert_eq!(
            sim_a.query("kv", query.clone())["value"],
            sim_b.query("kv", query)["value"],
            "kv value for {k:?} converges"
        );
    }
    // pages answers point reads only since the read-model cutover (ListPages
    // moved to the index guest), so convergence is asserted per page the script
    // created — which names them instead of trusting a list's ordering.
    for page_id in ["p1", "p2"] {
        let query =
            serde_json::json!({ "get_page": { "page_id": page_id, "after": null, "limit": 0 } });
        assert_eq!(
            sim_a.query("pages", query.clone()),
            sim_b.query("pages", query),
            "page {page_id} converges"
        );
    }

    // the timestamp-stamping modules converge once the block-dependent stamp is
    // stripped: the SAME entities exist in both runs, only their created_at differs.
    let tasks_a = strip(
        &sim_a.query(
            "tasks",
            serde_json::json!({ "task": { "list": { "limit": 256 } } }),
        )["task"]["tasks"],
        &["created_at", "updated_at"],
    );
    let tasks_b = strip(
        &sim_b.query(
            "tasks",
            serde_json::json!({ "task": { "list": { "limit": 256 } } }),
        )["task"]["tasks"],
        &["created_at", "updated_at"],
    );
    assert_eq!(
        tasks_a, tasks_b,
        "the same tasks exist in both runs (only the stamped time differs)"
    );
    // Empty inbox maintenance is independent of block structure.
    assert_eq!(module_root(&a, "inbox"), module_root(&b, "inbox"));
}
