//! fold / query tests for the derived tag index (`index/tags.rs`).
//! extraction unit tests live next to the extractor; these drive the whole
//! decision core over the map harness exactly like `index.rs`'s own tests
//! (whose tiny harness is deliberately duplicated here rather than shared —
//! the two suites stay independently readable).

use std::collections::BTreeMap;

use super::{
    ChatViewReply, MsgRow, TagRow, fold_op, msg_key, read_row, read_u64, seq_key, serve_view, tags,
};
use crate::{Block, ChatAssigned, ChatMsg, Span, encode_assigned, encode_msg};
use index_guest::{OpRow, OriginTag, apply_to_map};

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

/// the test twin of the module's in-state assignment — see `index.rs`'s
/// `assigned_for` (the harness duplication is deliberate, per the module doc).
fn assigned_for(map: &Map, msg: &ChatMsg) -> Vec<u8> {
    match msg {
        ChatMsg::PostMessage { channel_id, .. } => encode_assigned(&ChatAssigned::Posted {
            seq: read_u64(map, &seq_key(channel_id)) + 1,
            actor: crate::Party::Key(b"jess".to_vec()),
            key_mentions: Vec::new(),
        }),
        ChatMsg::EditMessage {
            channel_id, seq, ..
        } => {
            let row = read_row(map, &msg_key(channel_id, *seq))
                .expect("row reads")
                .expect("edited row exists");
            encode_assigned(&ChatAssigned::Edited {
                rev: row.rev + 1,
                actor: crate::Party::Key(b"jess".to_vec()),
                key_mentions: Vec::new(),
            })
        }
        ChatMsg::AddReaction { .. }
        | ChatMsg::RemoveReaction { .. }
        | ChatMsg::JoinHuddle { .. }
        | ChatMsg::LeaveHuddle { .. } => encode_assigned(&ChatAssigned::Participant {
            actor: crate::Party::Key(b"jess".to_vec()),
            participant: crate::Party::Key(b"jess".to_vec()),
        }),
        _ => encode_assigned(&ChatAssigned::Actor {
            actor: crate::Party::Key(b"jess".to_vec()),
        }),
    }
}

fn op(map: &Map, height: u64, msg: &ChatMsg) -> OpRow {
    OpRow {
        height,
        seq: 0,
        time: 1_000 + height,
        origin: OriginTag::external("jess"),
        payload: encode_msg(msg),
        assigned: assigned_for(map, msg),
    }
}

fn post(channel: &str, id: &str, text: &str) -> ChatMsg {
    ChatMsg::PostMessage {
        channel_id: channel.into(),
        message_id: id.into(),
        blocks: vec![Block::paragraph(text)],
        thread: None,
    }
}

fn edit(channel: &str, seq: u64, text: &str) -> ChatMsg {
    ChatMsg::EditMessage {
        channel_id: channel.into(),
        seq,
        blocks: vec![Block::paragraph(text)],
        base_rev: None,
    }
}

fn delete(channel: &str, seq: u64) -> ChatMsg {
    ChatMsg::DeleteMessage {
        channel_id: channel.into(),
        seq,
    }
}

fn fold(map: &mut Map, height: u64, msg: &ChatMsg) {
    let writes = fold_op(&op(map, height, msg), map).expect("fold");
    apply_to_map(map, writes);
}

fn hits(map: &Map, req: serde_json::Value) -> Vec<MsgRow> {
    let bytes = serve_view(map, &serde_json::to_vec(&req).unwrap()).expect("view");
    match serde_json::from_slice(&bytes).expect("reply decodes") {
        ChatViewReply::Hits(hits) => hits,
        other => panic!("expected hits, got {other:?}"),
    }
}

fn tag_rows(map: &Map, req: serde_json::Value) -> Vec<TagRow> {
    let bytes = serve_view(map, &serde_json::to_vec(&req).unwrap()).expect("view");
    match serde_json::from_slice(&bytes).expect("reply decodes") {
        ChatViewReply::Tags(rows) => rows,
        other => panic!("expected tags, got {other:?}"),
    }
}

fn ids(rows: &[MsgRow]) -> Vec<&str> {
    rows.iter().map(|r| r.message_id.as_str()).collect()
}

// ── fold: post / edit / delete ──────────────────────────────────────────────

#[test]
fn posts_index_tags_and_catalog() {
    let mut map = Map::new();
    fold(&mut map, 1, &post("general", "m1", "shipping #rust today"));
    fold(&mut map, 2, &post("general", "m2", "more #rust and #wasm"));
    fold(&mut map, 3, &post("random", "m3", "#rust elsewhere"));

    // channel-scoped catalog: count desc, then tag asc; last_seq = newest live.
    let rows = tag_rows(&map, serde_json::json!({"tags": {"channel_id": "general"}}));
    assert_eq!(
        rows,
        vec![
            TagRow {
                tag: "rust".into(),
                count: 2,
                last_seq: 2
            },
            TagRow {
                tag: "wasm".into(),
                count: 1,
                last_seq: 2
            },
        ]
    );

    // no channel aggregates counts across channels; last_seq is the max of
    // the per-channel newest (seq spaces are per-channel).
    let rows = tag_rows(&map, serde_json::json!({"tags": {}}));
    assert_eq!(rows[0].tag, "rust");
    assert_eq!(rows[0].count, 3);
    assert_eq!(rows[0].last_seq, 2);

    // tag search: exact label, newest first, channel scope honored.
    let all = hits(&map, serde_json::json!({"tag_search": {"tag": "rust"}}));
    assert_eq!(ids(&all), ["m3", "m2", "m1"]);
    let scoped = hits(
        &map,
        serde_json::json!({"tag_search": {"tag": "rust", "channel_id": "general"}}),
    );
    assert_eq!(ids(&scoped), ["m2", "m1"]);
    // rows carry their tag sets.
    assert_eq!(scoped[0].tags, ["rust", "wasm"]);
}

#[test]
fn tag_search_matches_exact_label_not_prefix() {
    let mut map = Map::new();
    fold(&mut map, 1, &post("g", "m1", "#rust"));
    fold(&mut map, 2, &post("g", "m2", "#rustlang"));

    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "rust"}})
        )),
        ["m1"]
    );
    // the query normalizes: `#Rust` (as clicked in the app) finds `#rust`.
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "#Rust"}})
        )),
        ["m1"]
    );
}

#[test]
fn non_ascii_tags_round_trip() {
    let mut map = Map::new();
    fold(&mut map, 1, &post("g", "m1", "ship #naïve-café today"));

    let rows = tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g"}}));
    assert_eq!(rows[0].tag, "naïve-café");
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "#naïve-café"}})
        )),
        ["m1"]
    );
}

#[test]
fn code_blocks_and_link_spans_do_not_index_tags() {
    let mut map = Map::new();
    fold(
        &mut map,
        1,
        &ChatMsg::PostMessage {
            channel_id: "g".into(),
            message_id: "m1".into(),
            blocks: vec![
                Block::Code {
                    lang: None,
                    text: "#nope in code".into(),
                },
                Block::Paragraph(vec![Span {
                    text: "https://x.com/#nope".into(),
                    marks: vec![crate::Mark::Link("https://x.com/#nope".into())],
                }]),
                Block::paragraph("#yes"),
            ],
            thread: None,
        },
    );

    let rows = tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g"}}));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tag, "yes");
    assert!(hits(&map, serde_json::json!({"tag_search": {"tag": "nope"}})).is_empty());
}

#[test]
fn edit_diffs_old_and_new_tag_sets() {
    let mut map = Map::new();
    fold(&mut map, 1, &post("g", "m1", "#old #keep"));
    fold(&mut map, 2, &edit("g", 1, "#keep #new"));

    assert!(hits(&map, serde_json::json!({"tag_search": {"tag": "old"}})).is_empty());
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "keep"}})
        )),
        ["m1"]
    );
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "new"}})
        )),
        ["m1"]
    );
    // the catalog moved by the diff: `old` is gone at count zero.
    let rows = tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g"}}));
    assert_eq!(
        rows,
        vec![
            TagRow {
                tag: "keep".into(),
                count: 1,
                last_seq: 1
            },
            TagRow {
                tag: "new".into(),
                count: 1,
                last_seq: 1
            },
        ]
    );
}

#[test]
fn delete_removes_postings_and_decrements_catalog() {
    let mut map = Map::new();
    fold(&mut map, 1, &post("g", "m1", "#x first"));
    fold(&mut map, 2, &post("g", "m2", "#x second"));
    fold(&mut map, 3, &delete("g", 2));

    // the NEWEST tagged message died: count decrements AND last_seq falls
    // back to the surviving posting (the catalog stores no last_seq — it is
    // read off the newest live posting, so no stale maximum survives).
    let rows = tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g"}}));
    assert_eq!(
        rows,
        vec![TagRow {
            tag: "x".into(),
            count: 1,
            last_seq: 1
        }]
    );
    assert_eq!(
        ids(&hits(&map, serde_json::json!({"tag_search": {"tag": "x"}}))),
        ["m1"]
    );

    // deleting the last carrier drops the catalog entry entirely.
    fold(&mut map, 4, &delete("g", 1));
    assert!(tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g"}})).is_empty());
    assert!(hits(&map, serde_json::json!({"tag_search": {"tag": "x"}})).is_empty());
}

#[test]
fn same_batch_post_then_edit_folds_tags_through_applied_writes() {
    let mut map = Map::new();
    // one feed batch: the edit's decision reads the post's applied writes.
    fold(&mut map, 1, &post("g", "m1", "#draft words"));
    fold(&mut map, 1, &edit("g", 1, "#final words"));

    assert!(hits(&map, serde_json::json!({"tag_search": {"tag": "draft"}})).is_empty());
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "final"}})
        )),
        ["m1"]
    );
    let rows = tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g"}}));
    assert_eq!(
        rows,
        vec![TagRow {
            tag: "final".into(),
            count: 1,
            last_seq: 1
        }]
    );
}

#[test]
fn sixteen_tag_cap_binds_at_the_fold() {
    let mut map = Map::new();
    let text: String = (0..20).map(|i| format!("#tag{i:02} ")).collect();
    fold(&mut map, 1, &post("g", "m1", &text));

    let rows = tag_rows(
        &map,
        serde_json::json!({"tags": {"channel_id": "g", "limit": 100}}),
    );
    assert_eq!(rows.len(), tags::MAX_TAGS_PER_MESSAGE);
    assert_eq!(
        hits(&map, serde_json::json!({"tag_search": {"tag": "tag15"}})).len(),
        1
    );
    assert!(hits(&map, serde_json::json!({"tag_search": {"tag": "tag16"}})).is_empty());
}

// ── queries: clamps and validation ──────────────────────────────────────────

#[test]
fn limits_default_and_clamp_like_search() {
    let mut map = Map::new();
    for i in 0..25u64 {
        fold(&mut map, 1 + i, &post("g", &format!("m{i}"), "#hot take"));
    }

    // default page = 20, newest first.
    let page = hits(&map, serde_json::json!({"tag_search": {"tag": "hot"}}));
    assert_eq!(page.len(), 20);
    assert_eq!(page[0].message_id, "m24");
    // zero clamps up to one, oversize clamps down to the max (100 ≥ 25 = all).
    assert_eq!(
        hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "hot", "limit": 0}})
        )
        .len(),
        1
    );
    assert_eq!(
        hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "hot", "limit": 100000}})
        )
        .len(),
        25
    );

    // the catalog query clamps the same way.
    let many: String = (0..3).map(|i| format!("#t{i} ")).collect();
    fold(&mut map, 100, &post("g", "mt", &many));
    assert_eq!(
        tag_rows(&map, serde_json::json!({"tags": {"limit": 0}})).len(),
        1
    );
}

#[test]
fn invalid_tag_queries_are_view_errors() {
    let mut map = Map::new();
    fold(&mut map, 1, &post("g", "m1", "#ok"));
    let long = "a".repeat(65);
    for bad in ["", "#", "two words", long.as_str()] {
        let req = serde_json::json!({"tag_search": {"tag": bad}});
        let err = serve_view(&map, &serde_json::to_vec(&req).unwrap()).unwrap_err();
        assert!(
            err.message.contains("not a valid tag"),
            "tag {bad:?} should be a view error, got {err:?}"
        );
    }
}

#[test]
fn slash_channel_ids_do_not_leak_across_tag_scopes() {
    let mut map = Map::new();
    // channel "g/0" nests INSIDE "g"'s key prefixes (`tag/{label}/g/`,
    // `tagcat/g/`), and its posting keys even sort AHEAD of g's own (the
    // sub-channel's '0' < the rseq's leading hex 'f') — the worst case for
    // prefix-structural scoping. its #shared lands at seq 2 in its own
    // channel, so any leak is value-distinguishable from g's seq-1 posting.
    fold(&mut map, 1, &post("g", "m1", "#shared"));
    fold(&mut map, 2, &post("g/0", "m2", "#subonly"));
    fold(&mut map, 3, &post("g/0", "m3", "#shared"));

    // Tags scoped to g: no bogus "0/shared" label off the sub-channel's
    // catalog row, no count leak, and last_seq reads g's OWN newest posting
    // (a structural-prefix leak would report the sub-channel's seq 2).
    assert_eq!(
        tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g"}})),
        vec![TagRow {
            tag: "shared".into(),
            count: 1,
            last_seq: 1
        }]
    );
    // Tags scoped to the sub-channel see exactly its own rows.
    assert_eq!(
        tag_rows(&map, serde_json::json!({"tags": {"channel_id": "g/0"}})),
        vec![
            TagRow {
                tag: "shared".into(),
                count: 1,
                last_seq: 2
            },
            TagRow {
                tag: "subonly".into(),
                count: 1,
                last_seq: 1
            },
        ]
    );

    // TagSearch scopes on the STORED channel id, exactly like Search.
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "shared", "channel_id": "g"}})
        )),
        ["m1"]
    );
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "shared", "channel_id": "g/0"}})
        )),
        ["m3"]
    );

    // no channel scope still aggregates both channels.
    let all = tag_rows(&map, serde_json::json!({"tags": {}}));
    let shared = all.iter().find(|r| r.tag == "shared").expect("aggregated");
    assert_eq!((shared.count, shared.last_seq), (2, 2));
    assert_eq!(
        ids(&hits(
            &map,
            serde_json::json!({"tag_search": {"tag": "shared"}})
        )),
        ["m3", "m1"]
    );
}
