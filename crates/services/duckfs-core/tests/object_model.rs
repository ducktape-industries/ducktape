//! the object model's contract: ids are domain-separated and byte-stable,
//! canonical encodings round-trip, and every decode is strict — trailing bytes,
//! truncation at any cut, out-of-order names/keys, over-cap counts, and
//! non-canonical tag/bool bytes all reject. pure by construction: no harness,
//! no sdk — just sha2 and the crate's object surface, so this test compiles and
//! passes under `duckfs-core` too.

use duckfs_core::objects::*;
use sha2::{Digest as _, Sha256};

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// little-endian `u64 len ‖ bytes` string — the on-wire string shape, mirrored
/// here so the hand-built rejection cases speak real bytes.
fn push_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s);
}

#[test]
fn object_ids_are_domain_separated_and_stable() {
    let body = b"hello".to_vec();
    let chunk_id = object_id(Kind::Chunk, &body);
    let mut pre = vec![0u8]; // Kind::Chunk tag
    pre.extend_from_slice(&body);
    assert_eq!(chunk_id, sha256(&pre), "id = sha256(tag || body)");
    assert_ne!(
        chunk_id,
        object_id(Kind::File, &body),
        "tags must separate domains"
    );
}

#[test]
fn kind_tag_round_trips_and_rejects_unknown() {
    for k in [Kind::Chunk, Kind::File, Kind::Tree, Kind::Snapshot] {
        assert_eq!(Kind::from_u8(k.tag()), Some(k));
    }
    assert_eq!(Kind::from_u8(4), None, "unknown kind tag rejected");
}

#[test]
fn file_tree_snapshot_round_trip() {
    let f = FileObj {
        size: 5,
        chunks: vec![[7u8; 32]],
        meta: [("kind".into(), "skill".into())].into(),
    };
    assert_eq!(FileObj::decode(&f.encode()).unwrap(), f);

    let t = TreeObj {
        entries: [(
            "a.txt".to_string(),
            TreeEntry {
                kind: EntryKind::File,
                id: [1; 32],
                exec: false,
                size: 5,
            },
        )]
        .into(),
    };
    assert_eq!(TreeObj::decode(&t.encode()).unwrap(), t);

    let s = SnapshotObj {
        root: [2; 32],
        parent: None,
        author: duckfs_core::Actor::System,
        consensus_time: 9,
        height: 9,
        message: "m".into(),
    };
    assert_eq!(SnapshotObj::decode(&s.encode()).unwrap(), s);

    // the has_parent branch must round-trip its optional 32-byte id too.
    let s2 = SnapshotObj {
        parent: Some([3; 32]),
        ..s
    };
    assert_eq!(SnapshotObj::decode(&s2.encode()).unwrap(), s2);
}

#[test]
fn multi_entry_tree_round_trips_in_name_order() {
    let mk = |id: u8| TreeEntry {
        kind: EntryKind::File,
        id: [id; 32],
        exec: id.is_multiple_of(2),
        size: id as u64,
    };
    let t = TreeObj {
        entries: [
            ("a".to_string(), mk(1)),
            ("b".to_string(), mk(2)),
            ("c".to_string(), mk(3)),
        ]
        .into(),
    };
    assert_eq!(TreeObj::decode(&t.encode()).unwrap(), t);
}

#[test]
fn strict_decode_rejects() {
    // trailing bytes
    let f = FileObj {
        size: 1,
        chunks: vec![[0; 32]],
        meta: Default::default(),
    };
    let mut b = f.encode();
    b.push(0);
    assert!(FileObj::decode(&b).is_err(), "trailing bytes");

    // truncation at every length is also rejected
    let enc = f.encode();
    for cut in 0..enc.len() {
        assert!(FileObj::decode(&enc[..cut]).is_err(), "truncated at {cut}");
    }

    // tree names must be strictly ascending — hand-encode "b" before "a".
    let mut body = Vec::new();
    body.extend_from_slice(&2u32.to_le_bytes()); // entry_count
    for name in ["b", "a"] {
        push_str(&mut body, name.as_bytes());
        body.push(0); // kind File
        body.extend_from_slice(&[0u8; 32]); // id
        body.push(0); // exec false
        body.extend_from_slice(&5u64.to_le_bytes()); // size
    }
    assert!(
        TreeObj::decode(&body).is_err(),
        "tree names not strictly ascending"
    );

    // a repeated name is not strictly ascending either.
    let mut dup = Vec::new();
    dup.extend_from_slice(&2u32.to_le_bytes());
    for _ in 0..2 {
        push_str(&mut dup, b"a");
        dup.push(0);
        dup.extend_from_slice(&[0u8; 32]);
        dup.push(0);
        dup.extend_from_slice(&0u64.to_le_bytes());
    }
    assert!(TreeObj::decode(&dup).is_err(), "duplicate tree name");
}

#[test]
fn decode_rejects_noncanonical_tag_and_bool_bytes() {
    // unknown entry kind byte (3 is past Symlink=2).
    let mut bad_kind = Vec::new();
    bad_kind.extend_from_slice(&1u32.to_le_bytes());
    push_str(&mut bad_kind, b"a");
    bad_kind.push(3); // unknown kind
    bad_kind.extend_from_slice(&[0u8; 32]);
    bad_kind.push(0);
    bad_kind.extend_from_slice(&0u64.to_le_bytes());
    assert!(TreeObj::decode(&bad_kind).is_err(), "unknown entry kind");

    // exec must be 0 or 1.
    let mut bad_exec = Vec::new();
    bad_exec.extend_from_slice(&1u32.to_le_bytes());
    push_str(&mut bad_exec, b"a");
    bad_exec.push(0);
    bad_exec.extend_from_slice(&[0u8; 32]);
    bad_exec.push(2); // non-canonical bool
    bad_exec.extend_from_slice(&0u64.to_le_bytes());
    assert!(TreeObj::decode(&bad_exec).is_err(), "non-0/1 exec byte");

    // has_parent must be 0 or 1.
    let mut bad_hp = Vec::new();
    bad_hp.extend_from_slice(&[0u8; 32]); // root
    bad_hp.push(2); // non-canonical has_parent
    assert!(
        SnapshotObj::decode(&bad_hp).is_err(),
        "non-0/1 has_parent byte"
    );
}

#[test]
fn decode_enforces_caps_and_ascending_meta() {
    // meta keys must be strictly ascending — "b" before "a".
    let mut unordered = Vec::new();
    unordered.extend_from_slice(&0u64.to_le_bytes()); // size
    unordered.extend_from_slice(&0u32.to_le_bytes()); // chunk_count
    unordered.extend_from_slice(&2u16.to_le_bytes()); // meta_count
    for (k, v) in [("b", "x"), ("a", "y")] {
        push_str(&mut unordered, k.as_bytes());
        push_str(&mut unordered, v.as_bytes());
    }
    assert!(
        FileObj::decode(&unordered).is_err(),
        "meta keys not strictly ascending"
    );

    // meta_count over MAX_META_ENTRIES (16) rejects even with well-formed pairs.
    let mut over_meta = Vec::new();
    over_meta.extend_from_slice(&0u64.to_le_bytes());
    over_meta.extend_from_slice(&0u32.to_le_bytes());
    over_meta.extend_from_slice(&17u16.to_le_bytes());
    for i in 0..17 {
        push_str(&mut over_meta, format!("k{i:02}").as_bytes());
        push_str(&mut over_meta, b"v");
    }
    assert!(
        FileObj::decode(&over_meta).is_err(),
        "meta count over cap (16)"
    );

    // an over-long meta key (65 > MAX_META_KEY_BYTES=64) rejects.
    let mut big_key = Vec::new();
    big_key.extend_from_slice(&0u64.to_le_bytes());
    big_key.extend_from_slice(&0u32.to_le_bytes());
    big_key.extend_from_slice(&1u16.to_le_bytes());
    push_str(&mut big_key, "k".repeat(65).as_bytes());
    push_str(&mut big_key, b"v");
    assert!(FileObj::decode(&big_key).is_err(), "meta key over cap (64)");

    // an over-long tree entry name (256 > MAX_NAME_BYTES=255) rejects.
    let mut big_name = Vec::new();
    big_name.extend_from_slice(&1u32.to_le_bytes());
    push_str(&mut big_name, "n".repeat(256).as_bytes());
    big_name.push(0);
    big_name.extend_from_slice(&[0u8; 32]);
    big_name.push(0);
    big_name.extend_from_slice(&0u64.to_le_bytes());
    assert!(
        TreeObj::decode(&big_name).is_err(),
        "tree name over cap (255)"
    );

    // an over-long snapshot message (4097 > MAX_MESSAGE_BYTES=4096) rejects.
    let mut big_msg = Vec::new();
    big_msg.extend_from_slice(&[0u8; 32]); // root
    big_msg.push(0); // has_parent
    push_str(&mut big_msg, b"author");
    big_msg.extend_from_slice(&0u64.to_le_bytes()); // consensus_time
    big_msg.extend_from_slice(&0u64.to_le_bytes()); // height
    push_str(&mut big_msg, "m".repeat(4097).as_bytes());
    assert!(
        SnapshotObj::decode(&big_msg).is_err(),
        "message over cap (4096)"
    );
}

#[test]
fn non_utf8_string_rejects() {
    // a meta key whose declared bytes are invalid utf-8 must reject.
    let mut body = Vec::new();
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    push_str(&mut body, &[0xff, 0xfe]); // not utf-8
    push_str(&mut body, b"v");
    assert!(FileObj::decode(&body).is_err(), "non-utf8 meta key");
}

#[test]
fn chunk_len_rule() {
    use duckfs_core::CHUNK_SIZE;
    let f = FileObj {
        size: CHUNK_SIZE + 1,
        chunks: vec![[0; 32], [1; 32]],
        meta: Default::default(),
    };
    assert!(verify_chunk_len(&f, 0, CHUNK_SIZE).is_ok());
    assert!(verify_chunk_len(&f, 1, 1).is_ok());
    assert!(
        verify_chunk_len(&f, 1, 0).is_err(),
        "empty-chunk spoof caught by length"
    );
    assert!(verify_chunk_len(&f, 2, 1).is_err(), "index out of range");

    // a single-chunk file exactly CHUNK_SIZE long: its one chunk is full.
    let full = FileObj {
        size: CHUNK_SIZE,
        chunks: vec![[0; 32]],
        meta: Default::default(),
    };
    assert!(verify_chunk_len(&full, 0, CHUNK_SIZE).is_ok());

    // size smaller than (n-1)*CHUNK_SIZE is an inconsistent size/chunks pair.
    let inconsistent = FileObj {
        size: 10,
        chunks: vec![[0; 32], [1; 32]],
        meta: Default::default(),
    };
    assert!(
        verify_chunk_len(&inconsistent, 1, 10).is_err(),
        "last-chunk remainder underflows"
    );
}

/// golden equality: each object kind encodes to exactly the hand-built byte
/// image. object bodies are id preimages and cursors cross the wire, so the
/// byte layout is a contract that must survive any internal codec refactor —
/// this test pins it independently of the production encoder.
#[test]
fn encodings_match_the_hand_built_bytes() {
    // FileObj: size u64 ‖ chunk count u32 ‖ ids ‖ meta count u16 ‖ k/v strings.
    let f = FileObj {
        size: 5,
        chunks: vec![[7u8; 32]],
        meta: [("kind".into(), "skill".into())].into(),
    };
    let mut want = 5u64.to_le_bytes().to_vec();
    want.extend_from_slice(&1u32.to_le_bytes());
    want.extend_from_slice(&[7u8; 32]);
    want.extend_from_slice(&1u16.to_le_bytes());
    push_str(&mut want, b"kind");
    push_str(&mut want, b"skill");
    assert_eq!(f.encode(), want, "FileObj byte layout drifted");

    // TreeObj: entry count u32 ‖ (name ‖ kind u8 ‖ id ‖ exec u8 ‖ size u64)*.
    let t = TreeObj {
        entries: [(
            "a.txt".to_string(),
            TreeEntry {
                kind: EntryKind::File,
                id: [1; 32],
                exec: true,
                size: 5,
            },
        )]
        .into(),
    };
    let mut want = 1u32.to_le_bytes().to_vec();
    push_str(&mut want, b"a.txt");
    want.push(0); // EntryKind::File tag
    want.extend_from_slice(&[1u8; 32]);
    want.push(1); // exec = true
    want.extend_from_slice(&5u64.to_le_bytes());
    assert_eq!(t.encode(), want, "TreeObj byte layout drifted");

    // SnapshotObj: root ‖ parent flag(+id) ‖ author ‖ time u64 ‖ height u64 ‖
    // message.
    let s = SnapshotObj {
        root: [2; 32],
        parent: Some([3; 32]),
        author: duckfs_core::Actor::Key(vec![0xaa]),
        consensus_time: 7,
        height: 9,
        message: "m".into(),
    };
    let mut want = [2u8; 32].to_vec();
    want.push(1); // has_parent
    want.extend_from_slice(&[3u8; 32]);
    want.push(1); // Key actor
    push_str(&mut want, &[0xaa]);
    want.extend_from_slice(&7u64.to_le_bytes());
    want.extend_from_slice(&9u64.to_le_bytes());
    push_str(&mut want, b"m");
    assert_eq!(s.encode(), want, "SnapshotObj byte layout drifted");
}
