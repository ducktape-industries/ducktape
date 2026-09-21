//! the small mutable consensus state — [`Refs`] — and its canonical codec. this
//! is the ONLY mutable state in duckfs; everything else is an immutable
//! content-addressed object. the module root is `root_bytes` = sha256 over the
//! canonical [`encode_refs`] image of THIS struct, and nothing else.
//!
//! two load-bearing rules pinned here:
//!
//! - **content-only root.** the recovery metadata that wraps the refs image on
//!   disk — the block height and the gc watermark — is per-node bookkeeping and
//!   lives ONLY in the refs-file envelope (`disk::DiskRefs`), never in this
//!   preimage. so the root does not move on an empty block, and two nodes with
//!   identical refs at different heights agree on the root.
//!
//! - **strict, canonical codec.** exactly one byte image encodes any refs value.
//!   [`decode_refs`] enforces it: counts are capped before they are trusted,
//!   pins/staging/watches must arrive in strictly-ascending key order, every
//!   read is bounds-checked, and a decode that does not consume the whole input
//!   is rejected. canonical bytes in, canonical bytes out, or an error — a
//!   colluding install can never smuggle a non-canonical image past the root
//!   check because a non-canonical image simply does not decode.
//!
//! there is no version byte: the frame is a fixed shape (head flag,
//! four counted sections, the source revision, then a retention-root flag) that
//! is never empty, so it self-separates from
//! zero-length input, and layout changes ride the flag-day reset rule (fresh
//! genesis, no migrations) rather than an in-band version.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use sha2::{Digest as _, Sha256};

use crate::Actor;
use crate::codec::{Reader, push_string, push_u32};
use crate::objects::{Kind, ObjectId};
use crate::wire::{
    HISTORY_WINDOW, MAX_PINS, MAX_REFS_IMAGE_BYTES, MAX_STAGING_ENTRIES, MAX_WATCHES,
};

/// a named pin: the snapshot it protects from gc and the actor that created it,
/// kept for attribution and the per-owner pin share; any authority may remove it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinEntry {
    pub snapshot: ObjectId,
    pub owner: Actor,
}

/// a staged (putblob'd) chunk awaiting a commit that references it: who staged
/// it, its byte length, and the block height at which it expires if unreferenced
/// (the deterministic, op-stream-driven staging sweep keys on this).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub owner: Actor,
    pub len: u64,
    pub expires_at: u64,
}

/// the whole mutable consensus state. `head` is the live snapshot; `window` is
/// the bounded commit history (newest-last insertion order — NOT a sorted set);
/// `pins` name-key protected snapshots; `staging` holds putblob'd-but-uncommitted
/// chunks; `watches` is the set of (prefix, module_id) subscriptions.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Refs {
    /// Source-owned revision, preserved even when every named object is removed.
    pub source_revision: u64,
    pub head: Option<ObjectId>,
    pub window: VecDeque<ObjectId>,
    pub pins: BTreeMap<String, PinEntry>,
    pub staging: BTreeMap<ObjectId, Staged>,
    pub watches: BTreeSet<(String, String)>,
    /// Internal copy-on-write catalog, separate from the collaborative head.
    /// Its leaves name module-owned references and snapshot membership counts.
    pub retention_root: Option<ObjectId>,
}

/// the canonical refs image — the exact `root_bytes` preimage and the exact
/// payload the refs-file envelope wraps and the snapshot lane ships. layout
/// (all little-endian; strings are `u64 len ‖ utf-8 bytes`; ids are raw 32 B):
///
/// ```text
/// head    : u8 flag (1 = present) ‖ [32 B] present iff flag == 1
/// window  : u32 count ‖ count × id
/// pins    : u32 count ‖ count × (name ‖ snapshot ‖ owner)   -- name order
/// staging : u32 count ‖ count × (digest ‖ owner ‖ len u64 ‖ expires_at u64)
///                                                            -- digest order
/// watches : u32 count ‖ count × (prefix ‖ module_id)        -- tuple order
/// source_revision : u64
/// retention_root : u8 flag (1 = present) ‖ [32 B] present iff flag == 1
/// ```
pub fn encode_refs(r: &Refs) -> Vec<u8> {
    let mut out = Vec::new();

    // head: present-only, matching the object-model Option convention — a flag
    // byte, and the 32 id bytes iff the flag is 1. so None has exactly one
    // encoding (a lone 0) rather than 0 followed by 32 ambiguous bytes.
    match &r.head {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(id);
        }
        None => out.push(0),
    }

    // window: a sequence in history order; the count is u32 and the order is the
    // deque's own order (decode does NOT re-sort it — order is meaningful).
    push_u32(&mut out, r.window.len() as u32);
    for id in &r.window {
        out.extend_from_slice(id);
    }

    // pins: BTreeMap iterates ascending by name, which is exactly the canonical
    // order the decoder re-checks.
    push_u32(&mut out, r.pins.len() as u32);
    for (name, pin) in &r.pins {
        push_string(&mut out, name);
        out.extend_from_slice(&pin.snapshot);
        pin.owner.encode_into(&mut out);
    }

    // staging: BTreeMap keyed by the 32-byte digest, ascending.
    push_u32(&mut out, r.staging.len() as u32);
    for (digest, staged) in &r.staging {
        out.extend_from_slice(digest);
        staged.owner.encode_into(&mut out);
        out.extend_from_slice(&staged.len.to_le_bytes());
        out.extend_from_slice(&staged.expires_at.to_le_bytes());
    }

    // watches: BTreeSet of (prefix, module_id) tuples, ascending lexicographic.
    push_u32(&mut out, r.watches.len() as u32);
    for (prefix, module_id) in &r.watches {
        push_string(&mut out, prefix);
        push_string(&mut out, module_id);
    }

    out.extend_from_slice(&r.source_revision.to_le_bytes());
    match r.retention_root {
        Some(root) => {
            out.push(1);
            out.extend_from_slice(&root);
        }
        None => out.push(0),
    }
    out
}

/// the byte layout's per-entry costs, one per growable section. the growth
/// gates in `fs.rs` and [`encoded_refs_len`] count with these, so the image cap
/// ([`MAX_REFS_IMAGE_BYTES`]) is enforced without encoding the whole image on
/// every op — and the layout has exactly one description: [`encode_refs`].
pub fn pin_entry_len(name: &str, owner: &Actor) -> usize {
    8 + name.len() + 32 + owner.encoded_len()
}

pub fn staged_entry_len(owner: &Actor) -> usize {
    32 + owner.encoded_len() + 8 + 8
}

pub fn watch_entry_len(prefix: &str, module_id: &str) -> usize {
    8 + prefix.len() + 8 + module_id.len()
}

/// `encode_refs(r).len()` without the allocation — the arithmetic twin of the
/// encoder, kept next to it (a test pins the two equal).
pub fn encoded_refs_len(r: &Refs) -> usize {
    let head = 1 + r.head.map_or(0, |_| 32);
    let window = 4 + 32 * r.window.len();
    let pins = 4 + r
        .pins
        .iter()
        .map(|(name, pin)| pin_entry_len(name, &pin.owner))
        .sum::<usize>();
    let staging = 4 + r
        .staging
        .values()
        .map(|staged| staged_entry_len(&staged.owner))
        .sum::<usize>();
    let watches = 4 + r
        .watches
        .iter()
        .map(|(prefix, module_id)| watch_entry_len(prefix, module_id))
        .sum::<usize>();
    let retention = 1 + r.retention_root.map_or(0, |_| 32);
    head + window + pins + staging + watches + 8 + retention
}

/// strict decode of an [`encode_refs`] image; anything non-canonical rejects.
pub fn decode_refs(bytes: &[u8]) -> Result<Refs, String> {
    // the byte cap first: no honest image is larger (every growth path
    // refuses past it), and a larger one could not be served to a joiner.
    if bytes.len() > MAX_REFS_IMAGE_BYTES {
        return Err("refs image exceeds the byte cap".into());
    }
    let mut r = Reader::new("refs image", bytes);

    let head = if r.boolean()? {
        Some(r.bytes32()?)
    } else {
        None
    };

    let window_count = r.u32()? as usize;
    if window_count > HISTORY_WINDOW {
        return Err("refs window count over cap".into());
    }
    let mut window = VecDeque::new();
    for _ in 0..window_count {
        window.push_back(r.bytes32()?);
    }

    let pin_count = r.u32()? as usize;
    if pin_count > MAX_PINS {
        return Err("refs pin count over cap".into());
    }
    let mut pins: BTreeMap<String, PinEntry> = BTreeMap::new();
    for _ in 0..pin_count {
        let name = r.string()?;
        let snapshot = r.bytes32()?;
        let owner = Actor::decode(&mut r)?;
        // strictly ascending names keep the image canonical: no duplicate and no
        // reordered pair can produce a second valid preimage of the same refs.
        if pins
            .last_key_value()
            .is_some_and(|(last, _)| last.as_str() >= name.as_str())
        {
            return Err("refs pins not strictly ascending".into());
        }
        pins.insert(name, PinEntry { snapshot, owner });
    }

    let staging_count = r.u32()? as usize;
    if staging_count > MAX_STAGING_ENTRIES {
        return Err("refs staging count over cap".into());
    }
    let mut staging: BTreeMap<ObjectId, Staged> = BTreeMap::new();
    for _ in 0..staging_count {
        let digest = r.bytes32()?;
        let owner = Actor::decode(&mut r)?;
        let len = r.u64()?;
        let expires_at = r.u64()?;
        if staging
            .last_key_value()
            .is_some_and(|(last, _)| last >= &digest)
        {
            return Err("refs staging not strictly ascending".into());
        }
        staging.insert(
            digest,
            Staged {
                owner,
                len,
                expires_at,
            },
        );
    }

    let watch_count = r.u32()? as usize;
    if watch_count > MAX_WATCHES {
        return Err("refs watch count over cap".into());
    }
    let mut watches: BTreeSet<(String, String)> = BTreeSet::new();
    let mut last_watch: Option<(String, String)> = None;
    for _ in 0..watch_count {
        let prefix = r.string()?;
        let module_id = r.string()?;
        let entry = (prefix, module_id);
        if last_watch.as_ref().is_some_and(|last| last >= &entry) {
            return Err("refs watches not strictly ascending".into());
        }
        last_watch = Some(entry.clone());
        watches.insert(entry);
    }

    let source_revision = r.u64()?;
    let retention_root = if r.boolean()? {
        Some(r.bytes32()?)
    } else {
        None
    };
    r.finish()?;
    Ok(Refs {
        source_revision,
        head,
        window,
        pins,
        staging,
        watches,
        retention_root,
    })
}

/// the module root preimage: sha256 over the canonical refs image. height and
/// gc_watermark are deliberately absent — see the module docblock.
pub fn root_bytes(r: &Refs) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(encode_refs(r));
    h.finalize().into()
}

/// the wasm-guest's per-block object index (`Fs::commit_block`'s
/// `Pending::object_ids` twin), canonically encoded so it round-trips through
/// the guest's staged-only `__block_objects` state lane. layout (all
/// little-endian; ids raw 32 B; digest-ascending, the `BTreeMap` order):
///
/// ```text
/// u32 count ‖ count × (id[32] ‖ kind-tag u8 ‖ len u64)
/// ```
///
/// this NEVER enters the module root or the wire — the native module keeps this
/// index in-memory across a block, so it is per-node bookkeeping the guest
/// re-derives deterministically every dispatch. it exists only because an
/// adapter guest is rebuilt per dispatch and must reconstruct the block-local
/// index a later same-block op's availability/dedup reads (see
/// `files::guest`). kept canonical (encode iterates the sorted map) so replay is
/// deterministic across nodes.
pub fn encode_block_objects(index: &BTreeMap<ObjectId, (Kind, u64)>) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + index.len() * (32 + 1 + 8));
    push_u32(&mut out, index.len() as u32);
    for (id, (kind, len)) in index {
        out.extend_from_slice(id);
        out.push(kind.tag());
        out.extend_from_slice(&len.to_le_bytes());
    }
    out
}

/// decode a `__block_objects` image [`encode_block_objects`] produced. strict
/// like every duckfs frame (bounds-checked reads, whole-input `finish`, an
/// unknown kind tag rejects); inserting into a `BTreeMap` re-canonicalizes
/// order, so a decode is idempotent under re-encode. not a trust boundary (the
/// guest reads back its own staged write), but strict-decoded for crate
/// consistency and to fail loud on a torn value.
pub fn decode_block_objects(bytes: &[u8]) -> Result<BTreeMap<ObjectId, (Kind, u64)>, String> {
    let mut r = Reader::new("block objects", bytes);
    let count = r.u32()?;
    let mut index = BTreeMap::new();
    for _ in 0..count {
        let id = r.bytes32()?;
        let kind =
            Kind::from_u8(r.u8()?).ok_or_else(|| "block objects unknown kind tag".to_string())?;
        let len = r.u64()?;
        index.insert(id, (kind, len));
    }
    r.finish()?;
    Ok(index)
}

// the cursor codec (push helpers + the strict [`Reader`]) is shared with
// `objects.rs` via `crate::codec` — one grammar, two frames, zero drift.

// pure core — builds under `--no-default-features` too.
#[cfg(test)]
mod block_objects_tests {
    use super::{decode_block_objects, encode_block_objects};
    use crate::objects::{Kind, object_id};
    use std::collections::BTreeMap;

    #[test]
    fn block_objects_round_trip_empty_and_multi_entry() {
        // empty index: a lone count of 0, decodes back to the empty map.
        let empty = BTreeMap::new();
        assert_eq!(
            decode_block_objects(&encode_block_objects(&empty)).unwrap(),
            empty
        );

        // multi-entry across kinds; decode re-canonicalizes into the same map.
        let mut index = BTreeMap::new();
        index.insert(object_id(Kind::Chunk, b"a"), (Kind::Chunk, 1u64));
        index.insert(object_id(Kind::File, b"bb"), (Kind::File, 2u64));
        index.insert(object_id(Kind::Snapshot, b"ccc"), (Kind::Snapshot, 3u64));
        let bytes = encode_block_objects(&index);
        assert_eq!(decode_block_objects(&bytes).unwrap(), index);
    }

    #[test]
    fn block_objects_decode_rejects_unknown_kind_and_trailing_bytes() {
        let mut index = BTreeMap::new();
        index.insert(object_id(Kind::Chunk, b"x"), (Kind::Chunk, 1u64));
        let mut bytes = encode_block_objects(&index);
        // corrupt the kind tag byte (right after the 32-byte id, after the u32 count).
        bytes[4 + 32] = 0xEE;
        assert!(
            decode_block_objects(&bytes)
                .unwrap_err()
                .contains("unknown kind tag")
        );

        // trailing bytes reject at finish.
        let mut trailing = encode_block_objects(&index);
        trailing.push(0);
        assert!(
            decode_block_objects(&trailing)
                .unwrap_err()
                .contains("trailing bytes")
        );
    }
}

#[cfg(test)]
mod refs_image_tests {
    use super::{PinEntry, Refs, Staged, decode_refs, encode_refs, encoded_refs_len};
    use crate::wire::MAX_REFS_IMAGE_BYTES;

    /// the arithmetic twin must never drift from the encoder: the growth gates
    /// count with it, so a byte it misses is a byte past the cap on the wire.
    #[test]
    fn encoded_refs_len_matches_the_encoder() {
        let mut r = Refs {
            head: Some([1; 32]),
            ..Refs::default()
        };
        r.window.push_back([2; 32]);
        r.window.push_back([3; 32]);
        r.pins.insert(
            "release".into(),
            PinEntry {
                snapshot: [4; 32],
                owner: crate::Actor::Key(vec![0xaa, 0xbb]),
            },
        );
        r.staging.insert(
            [5; 32],
            Staged {
                owner: crate::Actor::Key(vec![0xcc, 0xdd]),
                len: 7,
                expires_at: 9,
            },
        );
        r.watches.insert(("/shared".into(), "chat".into()));
        assert_eq!(encoded_refs_len(&r), encode_refs(&r).len());
        let empty = Refs::default();
        assert_eq!(encoded_refs_len(&empty), encode_refs(&empty).len());
    }

    /// an image past the cap is refused before a single field is read.
    #[test]
    fn an_oversized_refs_image_is_refused_by_decode() {
        let oversized = vec![0u8; MAX_REFS_IMAGE_BYTES + 1];
        assert_eq!(
            decode_refs(&oversized).unwrap_err(),
            "refs image exceeds the byte cap"
        );
    }
}
