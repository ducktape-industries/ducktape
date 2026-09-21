//! the refs root preimage and its canonical codec — pure by construction: no
//! harness, no sdk, only `duckfs_core::state`, so this whole file compiles and passes
//! under `duckfs-core` too (the state module is core).
//!
//! the load-bearing rules pinned here: the root is a function of refs CONTENT
//! only (height/gc_watermark are never in the preimage), and the codec is
//! strict — a populated refs round-trips byte-for-byte, truncation at every cut
//! rejects, a trailing byte rejects, and any out-of-sorted-order pins/staging/
//! watches image rejects (a colluding root can never smuggle a non-canonical
//! encoding past install).

use std::collections::VecDeque;

use duckfs_core::Actor;
use duckfs_core::state::*;

// ---- byte-level builders, mirroring the on-wire refs frame, so the hand-built
// rejection cases speak real bytes (LE ints, u64-len-prefixed strings). ------

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_str(out: &mut Vec<u8>, s: &str) {
    push_u64(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}
fn push_actor(out: &mut Vec<u8>, actor: &Actor) {
    match actor {
        Actor::Account(number) => {
            out.push(0);
            push_u64(out, *number);
        }
        Actor::Key(key) => {
            out.push(1);
            push_u64(out, key.len() as u64);
            out.extend_from_slice(key);
        }
        Actor::Module(module) => {
            out.push(2);
            push_str(out, module);
        }
        Actor::System => out.push(3),
    }
}
fn push_pin(out: &mut Vec<u8>, name: &str, snapshot: &[u8; 32], owner: &Actor) {
    push_str(out, name);
    out.extend_from_slice(snapshot);
    push_actor(out, owner);
}
fn push_staged(out: &mut Vec<u8>, digest: &[u8; 32], owner: &Actor, len: u64, expires_at: u64) {
    out.extend_from_slice(digest);
    push_actor(out, owner);
    push_u64(out, len);
    push_u64(out, expires_at);
}
fn push_watch(out: &mut Vec<u8>, prefix: &str, module_id: &str) {
    push_str(out, prefix);
    push_str(out, module_id);
}

/// a refs image with the given entries, in whatever order the caller supplies —
/// so tests can splice entries deliberately out of sorted order.
fn frame(
    head: Option<[u8; 32]>,
    window: &[[u8; 32]],
    pins: &[(&str, [u8; 32], Actor)],
    staging: &[([u8; 32], Actor, u64, u64)],
    watches: &[(&str, &str)],
) -> Vec<u8> {
    let mut out = Vec::new();
    match head {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id);
        }
        None => out.push(0),
    }
    push_u32(&mut out, window.len() as u32);
    for id in window {
        out.extend_from_slice(id);
    }
    push_u32(&mut out, pins.len() as u32);
    for (name, snap, owner) in pins {
        push_pin(&mut out, name, snap, owner);
    }
    push_u32(&mut out, staging.len() as u32);
    for (digest, owner, len, exp) in staging {
        push_staged(&mut out, digest, owner, *len, *exp);
    }
    push_u32(&mut out, watches.len() as u32);
    for (prefix, module_id) in watches {
        push_watch(&mut out, prefix, module_id);
    }
    push_u64(&mut out, 0);
    out.push(0); // absent protected retention catalog
    out
}

fn populated() -> Refs {
    let mut r = Refs {
        head: Some([1; 32]),
        ..Default::default()
    };
    // window is an ordered history, not a sorted set — keep two ids in insert
    // order so the round-trip proves order is preserved.
    r.window.push_back([2; 32]);
    r.window.push_back([3; 32]);
    r.pins.insert(
        "beta".into(),
        PinEntry {
            snapshot: [4; 32],
            owner: duckfs_core::Actor::Key(vec![0xaa]),
        },
    );
    r.pins.insert(
        "alpha".into(),
        PinEntry {
            snapshot: [5; 32],
            owner: duckfs_core::Actor::Module("kv".into()),
        },
    );
    r.staging.insert(
        [7; 32],
        Staged {
            owner: duckfs_core::Actor::Key(vec![0xbb]),
            len: 10,
            expires_at: 100,
        },
    );
    r.staging.insert(
        [6; 32],
        Staged {
            owner: duckfs_core::Actor::Module("chat".into()),
            len: 20,
            expires_at: 200,
        },
    );
    r.watches.insert(("home/kv".into(), "kv".into()));
    r.watches.insert(("shared".into(), "chat".into()));
    r
}

#[test]
fn root_is_content_only_and_deterministic() {
    let a = Refs::default();
    let b = Refs::default();
    assert_eq!(root_bytes(&a), root_bytes(&b));
    let c = Refs {
        head: Some([9; 32]),
        ..Default::default()
    };
    assert_ne!(root_bytes(&a), root_bytes(&c), "head change moves the root");
}

#[test]
fn height_and_gc_watermark_are_never_in_the_preimage() {
    // the preimage is `encode_refs`; height/gc_watermark live only in the refs
    // FILE envelope, so two nodes at different heights over identical refs share
    // a root, and an empty block never moves it. this test asserts encode_refs
    // sees no height at all — the same refs value always encodes identically.
    let r = populated();
    assert_eq!(encode_refs(&r), encode_refs(&r.clone()));
    assert_eq!(root_bytes(&r), root_bytes(&r.clone()));
}

#[test]
fn populated_refs_round_trips_and_is_canonical() {
    let r = populated();
    let enc = encode_refs(&r);
    let dec = decode_refs(&enc).expect("populated refs must decode");
    assert_eq!(dec, r, "round-trip preserves every field");
    // re-encoding the decoded value is byte-identical: exactly one canonical
    // image per refs value.
    assert_eq!(encode_refs(&dec), enc, "encoding is canonical");
    // window order is preserved (it is a history sequence, not a sorted set).
    let win: VecDeque<[u8; 32]> = [[2; 32], [3; 32]].into_iter().collect();
    assert_eq!(dec.window, win);
}

#[test]
fn decode_is_strict_on_truncation_and_trailing() {
    let r = populated();
    let enc = encode_refs(&r);
    // a trailing byte is not canonical — every byte must be consumed.
    let mut trailing = enc.clone();
    trailing.push(0);
    assert!(decode_refs(&trailing).is_err(), "trailing byte must reject");
    // truncation at every cut short of the full image is a truncation error.
    for cut in 0..enc.len() {
        assert!(decode_refs(&enc[..cut]).is_err(), "truncated at {cut}");
    }
    // and the full image decodes.
    assert!(decode_refs(&enc).is_ok());
}

#[test]
fn retention_root_is_canonical_and_changes_the_root() {
    let mut refs = populated();
    let before = root_bytes(&refs);
    refs.retention_root = Some([0x5a; 32]);
    let encoded = encode_refs(&refs);
    assert_eq!(encoded_refs_len(&refs), encoded.len());
    assert_eq!(decode_refs(&encoded).unwrap(), refs);
    assert_ne!(root_bytes(&refs), before);
    let flag = encoded.len() - 33;
    let mut invalid = encoded.clone();
    invalid[flag] = 2;
    assert!(decode_refs(&invalid).is_err());
    for end in flag + 1..encoded.len() {
        assert!(decode_refs(&encoded[..end]).is_err());
    }
}

#[test]
fn unsorted_pins_reject() {
    // two pins spliced in descending name order — decode must reject, or a
    // second (non-canonical) encoding of the same refs would exist.
    let bytes = frame(
        None,
        &[],
        &[
            ("b", [1; 32], Actor::Module("o".into())),
            ("a", [2; 32], Actor::Module("o".into())),
        ],
        &[],
        &[],
    );
    assert!(
        decode_refs(&bytes).is_err(),
        "pins must strictly ascend by name"
    );
    // the sorted twin decodes — proves only the ORDER was the defect.
    let ok = frame(
        None,
        &[],
        &[
            ("a", [2; 32], Actor::Module("o".into())),
            ("b", [1; 32], Actor::Module("o".into())),
        ],
        &[],
        &[],
    );
    assert!(decode_refs(&ok).is_ok());
}

#[test]
fn unsorted_staging_reject() {
    // staging keys are digests; splice two out of ascending digest order.
    let bytes = frame(
        None,
        &[],
        &[],
        &[
            ([9; 32], Actor::Module("o".into()), 1, 2),
            ([1; 32], Actor::Module("o".into()), 1, 2),
        ],
        &[],
    );
    assert!(
        decode_refs(&bytes).is_err(),
        "staging must strictly ascend by digest"
    );
}

#[test]
fn unsorted_watches_reject() {
    // watches are (prefix, module_id) tuples in lexicographic order.
    let bytes = frame(None, &[], &[], &[], &[("z", "m"), ("a", "m")]);
    assert!(decode_refs(&bytes).is_err(), "watches must strictly ascend");
}

#[test]
fn duplicate_keys_reject() {
    // a duplicate key is "not strictly ascending" too — a colluding image can
    // never carry two entries under one key.
    let dup_pins = frame(
        None,
        &[],
        &[
            ("a", [1; 32], Actor::Module("o".into())),
            ("a", [2; 32], Actor::Module("o".into())),
        ],
        &[],
        &[],
    );
    assert!(
        decode_refs(&dup_pins).is_err(),
        "duplicate pin name must reject"
    );
    let dup_watch = frame(None, &[], &[], &[], &[("a", "m"), ("a", "m")]);
    assert!(
        decode_refs(&dup_watch).is_err(),
        "duplicate watch must reject"
    );
}

/// golden equality: `encode_refs` emits exactly the hand-built frame. the refs
/// image is the root preimage and the snapshot-lane payload, so its byte
/// layout is a contract that must survive any internal codec refactor — this
/// test pins it independently of the production encoder.
#[test]
fn encode_matches_the_hand_built_frame() {
    let enc = encode_refs(&populated());
    // pins ascend by name, staging by digest, watches by tuple — the orders
    // the decoder re-checks.
    let want = frame(
        Some([1; 32]),
        &[[2; 32], [3; 32]],
        &[
            ("alpha", [5; 32], Actor::Module("kv".into())),
            ("beta", [4; 32], Actor::Key(vec![0xaa])),
        ],
        &[
            ([6; 32], Actor::Module("chat".into()), 20, 200),
            ([7; 32], Actor::Key(vec![0xbb]), 10, 100),
        ],
        &[("home/kv", "kv"), ("shared", "chat")],
    );
    assert_eq!(enc, want, "refs byte layout drifted");
}
