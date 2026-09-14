//! The folds the screen makes off what it reads, pinned where they moved from
//! (the desktop app's `backend/explorer.rs` and `backend/search.rs`).

use explorer_view::host::{
    ExplorerHit, Leg, Payload, PayloadField, TraceHop, explorer_trace, explorer_window,
    fold_search, height_label, hex, proposer_label,
};

fn hops() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "module": "chat", "origin": "external",
            "emitted_msgs": 1, "emitted_events": 0,
        }),
        serde_json::json!({
            "module": "attribution", "origin": "module:chat",
            "emitted_msgs": 0, "emitted_events": 2,
        }),
    ]
}

/// EVERY COUNT IN THE TRACE NAMES WHAT IT COUNTS. This rendered
/// `chat(+0m/+0e)`, a private shorthand nothing on the screen expanded — `m`
/// and `e` are not words — and then one joined `a · b → c · d` string. Each
/// hop is its own labelled row now; both units appear once singular and once
/// plural, so a hand-rolled `{n} messages` that skips the `plural` seam fails.
#[test]
fn the_dispatch_trace_is_one_labelled_hop_per_module() {
    let trace = explorer_trace(Some(&hops()));
    assert_eq!(
        trace,
        vec![
            TraceHop {
                module: "chat".into(),
                emitted_msgs: 1,
                emitted_events: 0
            },
            TraceHop {
                module: "attribution".into(),
                emitted_msgs: 0,
                emitted_events: 2
            },
        ]
    );
    assert_eq!(trace[0].emitted(), "1 message, 0 events");
    assert_eq!(trace[1].emitted(), "0 messages, 2 events");
    assert_eq!(explorer_trace(None), Vec::new());
}

/// NO ROW MAY CONTRADICT THE SENTENCE OVER IT, which is a claim about the
/// DATA: `/v1/blocks` is not uniformly filtered. Three of its four row writers
/// drop an op-less block; the fourth, `boundary_block_row`, writes a follower's
/// ascension tip with `hash: ""` and no ops — a blank hash column and `0 ops`
/// directly under a subtitle saying every row carried operations, opening to an
/// empty pane.
#[test]
fn the_window_lists_only_the_blocks_that_carried_operations() {
    let served = [
        serde_json::json!({
            "height": 41, "hash": "", "commit_hash": "aa11bb22cc33dd44", "ops": [],
        }),
        serde_json::json!({
            "height": 42, "hash": "ee55ff66aa77bb88", "commit_hash": "cc99dd00ee11ff22",
            "ops": [{
                "proposer": "abc123def456789a", "disposition": "applied", "target": "chat",
                "op_hash": "0f1e2d3c4b5a6978", "payload": "hi", "operations": hops(),
            }],
        }),
    ];

    let window = explorer_window(&served);

    assert_eq!(
        window
            .blocks
            .iter()
            .map(|block| (block.height, block.op_count))
            .collect::<Vec<_>>(),
        vec![(42, 1)],
        "a block carrying no operations was listed under a subtitle that says \
         every row carried some"
    );
    assert!(
        window.ops.iter().all(|op| op.height == 42),
        "an op was attributed to a block the list does not hold"
    );
}

/// THE DIGESTS CROSS WHOLE AND BARE. The view adds the `0x`; a digest cut here
/// is one no screen can ever recover. A JSON payload crosses as labelled
/// fields — never as a JSON string the screen would print raw — and anything
/// else as the text it was.
#[test]
fn every_published_digest_crosses_whole_and_a_json_payload_reads_as_fields() {
    let rows = [serde_json::json!({
        "height": 7,
        "hash": "aa".repeat(32),
        "commit_hash": "bb".repeat(32),
        "ops": [
            {
                "proposer": "cc".repeat(32), "target": "files", "disposition": "applied",
                "op_hash": "dd".repeat(32),
                "payload": "{\"put\":{\"path\":\"/shared/a.png\"}}", "operations": []
            },
            {
                "proposer": "cc".repeat(32), "target": "chat", "disposition": "applied",
                "op_hash": "dd".repeat(32),
                "payload": "plain prose, not a document", "operations": []
            }
        ]
    })];

    // the window lists newest first: ops arrive [files, chat] and reverse.
    let window = explorer_window(&rows);

    assert_eq!(window.blocks[0].hash, "aa".repeat(32));
    assert_eq!(window.blocks[0].commit, "bb".repeat(32));
    assert_eq!(window.ops[0].proposer, "cc".repeat(32));
    assert_eq!(
        window.ops[1].op_hash,
        "dd".repeat(32),
        "the op hash is the blob key — the card carries it whole"
    );
    assert_eq!(
        window.ops[1].payload,
        Payload::Fields(vec![PayloadField {
            name: "put.path".into(),
            value: "/shared/a.png".into()
        }]),
        "a JSON payload reads as labelled fields"
    );
    assert_eq!(window.ops[1].verb(), "put");
    assert_eq!(
        window.ops[0].payload,
        Payload::Text("plain prose, not a document".into()),
        "a non-JSON payload stays the text it was"
    );
    assert_eq!(window.ops[0].verb(), "");
    assert_eq!(window.ops[0].disposition, "Applied");
    assert!(window.ops[0].applied);
}

/// A PROPOSER READS AS WORDS. A frame-authored key is a `0x` digest; the
/// labels `project_root_op` stamps on the rest are spelled out, and `system`
/// is already a word.
#[test]
fn a_proposer_reads_as_a_digest_or_as_words() {
    assert_eq!(proposer_label("ab12cd34"), "0xab12cd34");
    assert_eq!(proposer_label("module:chat"), "module chat");
    assert_eq!(proposer_label("acct:7"), "account 7");
    assert_eq!(proposer_label("system"), "system");
}

/// A DIGEST INSIDE A PAYLOAD IS A DIGEST. Module messages carry theirs as
/// `Vec<u8>` (forge's `new_oid`, runs' `recipe_hash`, the registry's
/// `code_hash`), and serde prints those as decimal arrays — a wall of
/// three-digit numbers where a hash belongs, in the same card as two hashes
/// written in hex. Short arrays are left alone: they are counts, not keys.
#[test]
fn payload_byte_arrays_read_as_hex_beside_the_hashes_they_belong_with() {
    let oid: Vec<u8> = (1..=20).collect();
    let rows = [serde_json::json!({
        "height": 7, "hash": "aa".repeat(32), "commit_hash": "bb".repeat(32),
        "ops": [{
            "proposer": "cc".repeat(32), "target": "forge", "disposition": "applied",
            "op_hash": "dd".repeat(32),
            "payload": serde_json::to_string(&serde_json::json!({
                "push": { "new_oid": oid, "counts": [1, 2, 3] }
            })).expect("payload encodes"),
            "operations": []
        }]
    })];

    let window = explorer_window(&rows);

    assert_eq!(
        window.ops[0].payload,
        Payload::Fields(vec![
            PayloadField {
                name: "push.counts".into(),
                value: "1, 2, 3".into()
            },
            PayloadField {
                name: "push.new_oid".into(),
                value: "0x0102030405060708090a0b0c0d0e0f1011121314".into()
            },
        ]),
        "the oid reads as one hex key; a short list of numbers is still a list"
    );
}

/// EVERY LEAF IS A FIELD, NAMED BY ITS PATH: nested objects flatten to dotted
/// names, an array of objects indexes into the path, and an empty container
/// or a null reads as a dash rather than as `{}`/`[]`/`null`.
#[test]
fn a_nested_payload_flattens_to_named_leaves() {
    let rows = [serde_json::json!({
        "height": 7, "hash": "aa".repeat(32), "commit_hash": "bb".repeat(32),
        "ops": [{
            "proposer": "system", "target": "tasks", "disposition": "rejected",
            "op_hash": "dd".repeat(32),
            "payload": serde_json::to_string(&serde_json::json!({
                "create": {
                    "title": "Ship it", "assignee": null, "tags": [],
                    "steps": [{ "name": "build", "done": true }]
                }
            })).expect("payload encodes"),
            "operations": []
        }]
    })];

    let window = explorer_window(&rows);

    let fields: Vec<(&str, &str)> = match &window.ops[0].payload {
        Payload::Fields(fields) => fields
            .iter()
            .map(|field| (field.name.as_str(), field.value.as_str()))
            .collect(),
        other => panic!("fields, got {other:?}"),
    };
    assert_eq!(
        fields,
        [
            ("create.assignee", "—"),
            ("create.steps.0.done", "true"),
            ("create.steps.0.name", "build"),
            ("create.tags", "—"),
            ("create.title", "Ship it"),
        ]
    );
    assert_eq!(window.ops[0].disposition, "Rejected");
    assert!(!window.ops[0].applied);
}

fn hit(kind: &str) -> ExplorerHit {
    ExplorerHit {
        kind: kind.into(),
        ..ExplorerHit::default()
    }
}

fn answered(kind: &'static str, label: &'static str) -> Leg {
    Leg {
        kind,
        label,
        hits: Some(vec![hit(kind)]),
    }
}

fn silent(kind: &'static str, label: &'static str) -> Leg {
    Leg {
        kind,
        label,
        hits: None,
    }
}

/// A SOURCE THAT DID NOT ANSWER IS NOT A SOURCE WITH NOTHING TO SAY. Every leg
/// fails softly, so a search that reached the node and lost three of its six
/// sources would otherwise render a confident count, a chip strip reading 0 for
/// kinds it never read, and — when the survivors were empty — "Nothing matched
/// that query in this workspace". The strip's own contract is "a count of 0
/// means nothing matched, never no loader", so a source that never ran keeps no
/// chip at all and is named in `partial` instead.
#[test]
fn a_search_that_lost_a_source_names_it_and_keeps_no_chip_for_it() {
    const SOURCES: [(&str, &str); 6] = [
        ("message", "Messages"),
        ("page", "Pages"),
        ("code", "Code"),
        ("file", "Files"),
        ("task", "Tasks"),
        ("run", "Runs"),
    ];

    // every source answered: every chip, no sentence, screen order kept
    let whole = fold_search(
        SOURCES
            .iter()
            .map(|(kind, label)| answered(kind, label))
            .collect(),
    );
    assert_eq!(whole.partial, "");
    assert_eq!(
        whole.kinds.iter().map(|chip| chip.kind.as_str()).collect::<Vec<_>>(),
        SOURCES.map(|(kind, _)| kind)
    );
    assert!(whole.kinds.iter().all(|chip| chip.count == 1));
    assert_eq!(
        whole.hits.iter().map(|hit| hit.kind.as_str()).collect::<Vec<_>>(),
        SOURCES.map(|(kind, _)| kind),
        "the fold keeps the order the screen shows"
    );

    // each source in turn is the one that did not answer
    for (quiet, quiet_label) in SOURCES {
        let folded = fold_search(
            SOURCES
                .iter()
                .map(|(kind, label)| match *kind == quiet {
                    true => silent(kind, label),
                    false => answered(kind, label),
                })
                .collect(),
        );
        assert_eq!(
            folded.partial,
            format!("{quiet_label} did not answer — these results are incomplete."),
            "the screen must name the source it did not read"
        );
        assert!(
            folded.kinds.iter().all(|chip| chip.kind != quiet),
            "{quiet_label} was refused, so it keeps no chip"
        );
        assert!(
            folded.hits.iter().all(|hit| hit.kind != quiet),
            "a refused source contributes no rows"
        );
    }

    // CARDINALITY: two at once, both named, in the order the strip lists them.
    let two = fold_search(
        SOURCES
            .iter()
            .map(|(kind, label)| match *kind == "message" || *kind == "page" {
                true => silent(kind, label),
                false => answered(kind, label),
            })
            .collect(),
    );
    assert_eq!(
        two.partial,
        "Messages, Pages did not answer — these results are incomplete."
    );
}

/// `0x` MARKS HEX, SO IT GOES ON NOTHING ELSE, and a height the node has not
/// reported reads as a dash rather than as zero.
#[test]
fn the_display_forms_say_what_they_are() {
    assert_eq!(hex("ab12cd34"), "0xab12cd34");
    assert_eq!(hex("system"), "system");
    assert_eq!(hex(""), "");
    assert_eq!(height_label(84_912), "block 84,912");
    assert_eq!(height_label(-1), "block —");
}
