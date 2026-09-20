//! the replication layer — an [`OrderedNode`] wraps a [`host::Host`] with an
//! [`Orderer`] seam so N validators apply an AGREED TOTAL ORDER of signed op
//! frames identically. a locally-originated msg is NOT applied on submission;
//! it is proposed into the order and applied via `host.submit` ONLY when the
//! order delivers it — in the identical sequence on every validator — so even
//! an order-DEPENDENT qmdb root converges (the ordering-seam section below
//! carries the full rationale). the crate is runtime-agnostic — it spawns
//! nothing and depends on no async runtime; the real commonware simplex
//! orderer is the drop-in behind the same [`Orderer`] trait.

use host::{BlockContext, Host, MemberOutcome};
use sdk::{Event, Msg, StateRoot};

pub mod log_file;
pub mod resource_limits;
pub mod signed_req;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("host error: {0}")]
    Host(#[from] sdk::Error),
    /// a node-local block-boundary fault (see [`host::FatalError`]): this node's
    /// registry is indeterminate relative to its peers. the process must stop
    /// applying blocks — every lane surfaces this instead of continuing.
    #[error("{0}")]
    Fatal(host::FatalError),
    /// the recovery [`BlockSink`] failed to persist a record. as fatal as a
    /// boundary fault: continuing to apply blocks without durability would let
    /// a later restart silently lose finalized state, so the ordered lane
    /// surfaces this and the caller fail-stops.
    #[error("recovery journal: {0}")]
    Journal(String),
    /// the order delivered a block at a height this process has already
    /// journaled. the composed qmdb root is op-log-order-dependent, so
    /// applying out of order forks this node against every peer — and so does
    /// silently dropping the block. as fatal as a boundary fault: the ordering
    /// seam itself is broken, and the drain stops rather than compose a state
    /// no validator agreed on.
    #[error("delivered height {height} is at or below the applied height {applied}")]
    OutOfOrder { height: u64, applied: u64 },
    /// the block at `height` designates module code whose bytes this node
    /// cannot yet resolve, so the drain is PAUSED there — not broken. the
    /// frame is still at the front of the deferred queue and `applied` blocks
    /// settled ahead of it; the next drain retries, and the retry IS the
    /// fetch pump. a caller that fail-stops on this halts a chain that was
    /// only waiting: at n=3 the quorum is all three.
    #[error("module code for block {height} is not resolvable yet: {reason}")]
    CodeStalled {
        height: u64,
        applied: usize,
        reason: String,
    },
    /// this node's orderer is a follower — it holds no consensus proposal
    /// rights, so nothing it submits can enter the agreed order. loud so a
    /// miswired write path fails at the seam instead of silently vanishing;
    /// a resident's writes relay to a validator instead.
    #[error("this node holds no consensus proposal rights")]
    NotAParticipant,
}

impl From<host::SubmitError> for Error {
    fn from(e: host::SubmitError) -> Self {
        match e {
            // a deterministic rejection keeps its module-error shape.
            host::SubmitError::Rejected(e) => Error::Host(e),
            // a boundary fault keeps its fatality — callers match on this to
            // fail-stop rather than treating it as one bad op.
            host::SubmitError::Fatal(f) => Error::Fatal(f),
        }
    }
}

// ============================================================================
// the ordering seam — an AGREED TOTAL ORDER over opaque op frames.
// ============================================================================
//
// ## why an agreed order (not gossip)
//
// a gossip path (apply a locally-originated msg IMMEDIATELY — the echo — then
// fan it out to peers) converges only for order-INdependent module roots (a
// state-based `directory` root). a qmdb root is op-log/MMR-order-DEPENDENT:
// the same SET of ops in different orders yields a different root, so the
// instant two validators apply in different orders their root-hash FORKS.
//
// the fix is an AGREED TOTAL ORDER. a locally-originated msg is **NOT** applied
// on submission — that optimistic echo is exactly what forks the chain the
// moment another validator's op orders first. instead the msg is proposed into
// the order via [`Orderer::submit`], and it is applied via `host.submit` ONLY
// when [`Orderer::poll_delivered`] delivers it — in the identical sequence on
// every validator, including its originator. so even an order-dependent qmdb
// root converges.
//
// ## precondition (the honest gap vs real BFT)
//
// [`RoundOrderer`] converges because every validator accumulates the IDENTICAL
// SET of frames before draining a round, then applies a deterministic,
// node-independent total order over that set. the harness guarantees the
// identical-set precondition by handing every node the same op-set (in different
// arrival orders); a real simplex finalization stream guarantees it for free
// (every honest node observes the same finalized-view sequence). the simplex
// `Orderer` is the drop-in behind this same trait — its `submit` is store.put +
// enqueue-digest, its `poll_delivered` non-blocking-drains the finalization
// stream. that is why `submit` is async here even though the deterministic body
// never suspends.

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use keyscheme::KeyScheme;
use sdk::Origin;

/// the signing domain for op frames. domain-separated so an op signature can
/// never double as a consensus vote, an endpoint advertisement, or any other
/// signed artifact in the system. the ONE codec: a scheme tag, then
/// length-prefixed `(origin, seq, target, payload)` and nothing else. a frame
/// carries EXACTLY ONE op — there is no envelope continuation section, so a
/// frame cannot append a second op that dispatches under a caller-chosen
/// `Origin::Module` (see `no_continuation_lane.rs`). PUBLIC so an external
/// signer (a wallet, a passkey page) signs the exact namespace the decoder
/// verifies against.
pub const FRAME_NS: &[u8] = b"ducktape:op-frame:v1";

/// the content address of an encoded frame — sha256 over the exact bytes the
/// orderer carries. computed identically at submit and at drain, so a caller
/// holding the id [`OrderedNode::submit`] returned can recognize its own op in
/// [`OrderedNode::take_drained`]. the matching is internal to this seam:
/// nothing requires it to equal the consensus lane's content digest.
pub type FrameId = [u8; 32];

/// how many recently-journaled blocks the replay window remembers — the
/// protocol constant every validator applies, so the verdict on a re-finalized
/// batch is the same everywhere.
///
/// a finalized batch that already applied can be re-proposed by anyone holding
/// its bytes: consensus votes on availability alone, so it finalizes again at
/// a NEW height and its members' signed ops execute a second time. this window
/// is what refuses it. the bound is a memory/coverage trade: 4096 entries is
/// ~160 KiB and, at the ~1 block/s an idle chain heartbeats, a bit over an
/// hour of history — long enough to cover an epoch cutover, a restart, and the
/// journal suffix a checkpoint retains.
///
/// the window is a property of PERSISTED STATE, not of uptime, and it has to
/// stay that way: every boundary a node can start from carries it — the
/// checkpoint (`recovery::Manifest::applied_frames`) and the synced boundary
/// (`statesync::Manifest::applied_frames`) — so a cold seat and a validator
/// that has been up for a month enforce the same depth. a node that started
/// with an empty one would apply, for its first `REPLAY_WINDOW_HEIGHTS`
/// blocks, a re-proposed batch its peers refuse: one batch, two roots.
///
/// a batch older than the window can still replay. a per-origin nonce
/// enforced in replicated state is the unbounded successor and it is not this
/// seam: there is no host-owned replicated store (`host::global_root`
/// composes module roots and nothing else), and the node's own submit
/// sequence does not survive a state-sync re-bootstrap — `bin/node` seats a
/// re-bootstrapped identity at `next_seq = 1`, so a strictly-increasing
/// per-origin check would refuse its every subsequent op.
pub const REPLAY_WINDOW_HEIGHTS: usize = 4096;

/// THE replay-window verdict: `batch` already sealed at a height `window`
/// remembers. one rule, read by the live drain and by recovery's trailing
/// roll-forward over the same restored window — two copies could disagree on
/// a replayed batch, and that disagreement is a fork.
pub fn in_replay_window<'a>(
    window: impl IntoIterator<Item = &'a (u64, FrameId)>,
    batch: &FrameId,
) -> bool {
    window.into_iter().any(|(_, sealed)| sealed == batch)
}

/// how often a standing code-swap stall re-warns: attempt 1, then every Nth.
/// the drain retries every tick, so an unconditional warn would evict the
/// 4096-line ring in minutes — taking the evidence around the stall with it.
const CODE_STALL_WARN_EVERY: u64 = 64;

/// compute a frame's [`FrameId`] from its exact encoded bytes. public so a
/// boot-time observer holding journaled frame bytes derives the SAME id the
/// live drain reported (one definition — never a re-derivation drifting).
pub fn frame_id(bytes: &[u8]) -> FrameId {
    use commonware_cryptography::{Hasher as _, Sha256};
    let mut hasher = Sha256::default();
    hasher.update(bytes);
    let (_, digest) = hasher.finalize();
    let mut id = [0u8; 32];
    id.copy_from_slice(digest.as_ref());
    id
}

// a wire frame: the ordered unit. carries the submitter's public key
// (`origin`) under a declared SCHEME (the first byte — ed25519 for nodes and
// device keys, secp256k1 for a wallet, secp256r1 for a passkey), a per-origin
// monotonic `seq` (so two intentionally identical msgs are still DISTINCT
// frames — the order key must be tie-free), and a PROOF binding (scheme,
// origin, seq, target, payload, required_blob) to the origin key: after
// [`decode_frame`] verifies it, `Origin::External(pubkey)` is AUTHENTICATED
// AUTHORSHIP a module (e.g. governance voting) may rely on — no validator can
// forge another identity's op. the agreed order is the byte-lexicographic
// sort of these frames: correctness needs ONLY that the sort be a
// deterministic, node-independent total order over distinct frames (it is) —
// NOT that it be `(origin, seq)`-monotonic. replay of a byte-identical frame
// is deduplicated by the consensus lane's exactly-once digest gate; per-origin
// nonce enforcement IN STATE is the planned successor.
//
// the encoding is BINARY, not json: the frame is exactly the signed preimage
// (length-prefixed fields, see [`frame_preimage`]) with the scheme's proof
// bytes appended (64 for ed25519, 65 for a wallet, an assertion envelope for
// a passkey). json rendered the `Vec<u8>` payload as a decimal array (~3.57x
// expansion), which pushed any op past ~290 KiB of content over the p2p
// message cap — and commonware's `Sender::send` ASSERTS on that cap, so a
// full-CHUNK_SIZE duckfs putblob panicked the proposer's gossip task instead
// of rejecting (#215). the wire bytes are NOT consensus state (only the
// root-hash must match across nodes), but every validator must speak the same
// codec — changing it is a flag-day, fine while the network rebuilds anyway.

/// hard cap on ONE encoded op frame, enforced as a CLEAN deterministic
/// rejection at the submit boundary ([`OrderedNode::submit`]) — an over-cap
/// frame must never reach the p2p wire, whose sender asserts on its message
/// cap (a panic on the proposer's gossip task, not an error). sized for the
/// largest honest op — a duckfs putblob carrying one full 1 MiB chunk plus
/// the frame envelope (origin 32 + sig 64 + target + four u64 length
/// prefixes, ~200 bytes) — with 16 KiB of headroom. bin/node's p2p
/// `MAX_MESSAGE_SIZE` must stay above this plus the fetch-lane envelope; a
/// compile-time assert there pins the relationship.
pub const MAX_FRAME_BYTES: usize = (1 << 20) + (16 << 10);

/// the widest `target` a submitter may count on inside [`MAX_PAYLOAD_BYTES`]:
/// a module id is a handful of ASCII bytes, so this is headroom, not a limit
/// the decoder enforces.
pub const MAX_TARGET_BYTES: usize = 64;

/// the idle-chain heartbeat filler's target — a module that deliberately does
/// not exist, so the nop rejects identically on every validator and leaves no
/// state. it is a member like any other on the wire, so the drain counts it
/// apart from real rejections: a rejected member with any OTHER target is an
/// op that died. the heartbeat SUBMITS with this exact target and noded's
/// projection hides a block whose only op is it (both re-export this
/// constant), so the submit, the count and the filter can never drift.
pub const NOP_TARGET: &str = "consensus.nop";

/// the bytes [`encode_frame`] wraps around a payload: scheme tag 1, origin
/// length prefix 8 + 32-byte ed25519 pubkey, seq 8, target length prefix 8 +
/// up to [`MAX_TARGET_BYTES`] of target, payload length prefix 8,
/// one-byte absent-blob tag, and 64-byte signature. The
/// `max_payload_frame_fits_the_cap_exactly` test pins this arithmetic.
const ED25519_FRAME_ENVELOPE_BYTES: usize = 1 + 8 + 32 + 8 + 8 + MAX_TARGET_BYTES + 8 + 1 + 64;

/// the largest payload a device-signed op ([`encode_frame`]) can carry and
/// still fit [`MAX_FRAME_BYTES`] — the cap a client checks BEFORE signing.
/// the envelope budgets a full [`MAX_TARGET_BYTES`] of target, so this is
/// exact only at the widest target; under a shorter one it is conservative by
/// that target's slack (a 5-byte target leaves 59 bytes a client refuses and
/// the node would have taken). A required blob adds 32 bytes to this envelope,
/// so its payload allowance is `MAX_PAYLOAD_BYTES - 32`.
pub const MAX_PAYLOAD_BYTES: usize = MAX_FRAME_BYTES - ED25519_FRAME_ENVELOPE_BYTES;

/// [`MAX_FRAME_BYTES`] as a hex string: two digits per byte.
pub const MAX_FRAME_HEX_BYTES: usize = 2 * MAX_FRAME_BYTES;

/// read a little-endian u64 off the front of `buf`.
fn take_u64(buf: &mut &[u8]) -> Option<u64> {
    let (head, rest) = buf.split_at_checked(8)?;
    *buf = rest;
    Some(u64::from_le_bytes(head.try_into().expect("split of 8")))
}

/// read a u64 length prefix, then that many bytes, off the front of `buf`.
/// the length is checked against the remaining buffer BEFORE any use, so a
/// forged prefix can never drive allocation or slicing past the input.
fn take_slice<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = usize::try_from(take_u64(buf)?).ok()?;
    let (head, rest) = buf.split_at_checked(len)?;
    *buf = rest;
    Some(head)
}

/// the signed preimage AND the frame's wire prefix: the scheme tag, then
/// length-prefixed fields so no two (seq, target, payload) triples can
/// collide across a moving boundary. a frame is exactly these bytes with the
/// scheme's proof appended, so [`decode_frame`] verifies against the received
/// prefix without rebuilding anything. PUBLIC so a wallet or passkey client
/// signs the exact bytes the decoder verifies — never a reconstruction.
pub fn frame_preimage(scheme: KeyScheme, origin: &[u8], seq: u64, msg: &Msg) -> Vec<u8> {
    frame_preimage_with_blob(scheme, origin, seq, msg, None)
}

/// Canonical signed frame prefix. After the payload, tag 0 means no required
/// blob; tag 1 carries exactly one 32-byte digest. Both are inside the proof.
pub fn frame_preimage_with_blob(
    scheme: KeyScheme,
    origin: &[u8],
    seq: u64,
    msg: &Msg,
    required_blob: Option<[u8; 32]>,
) -> Vec<u8> {
    let target = msg.target.as_bytes();
    let mut out = Vec::with_capacity(
        2 + 8 * 4
            + origin.len()
            + target.len()
            + msg.payload.len()
            + required_blob.map_or(0, |_| 32),
    );
    out.push(scheme.tag());
    out.extend_from_slice(&(origin.len() as u64).to_le_bytes());
    out.extend_from_slice(origin);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(target.len() as u64).to_le_bytes());
    out.extend_from_slice(target);
    out.extend_from_slice(&(msg.payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&msg.payload);
    match required_blob {
        None => out.push(0),
        Some(digest) => {
            out.push(1);
            out.extend_from_slice(&digest);
        }
    }
    out
}

/// frame and SIGN a locally-originated msg for the ordered lane with an
/// ed25519 key (a node's or a device's). the signer's public key becomes the
/// frame's origin under tag 0; the frame bytes are the signed preimage with
/// the 64-byte signature appended. other schemes sign [`frame_preimage`]
/// externally and append their own proof.
pub fn encode_frame(signer: &PrivateKey, seq: u64, msg: &Msg) -> Vec<u8> {
    encode_frame_with_blob(signer, seq, msg, None)
}

/// Sign one operation and its optional required blob as a single frame.
pub fn encode_frame_with_blob(
    signer: &PrivateKey,
    seq: u64,
    msg: &Msg,
    required_blob: Option<[u8; 32]>,
) -> Vec<u8> {
    let origin = signer.public_key();
    let mut frame =
        frame_preimage_with_blob(KeyScheme::Ed25519, origin.as_ref(), seq, msg, required_blob);
    let sig = signer.sign(FRAME_NS, &frame);
    frame.extend_from_slice(sig.as_ref());
    frame
}

/// decode a delivered frame back to `(Origin, Msg)`, VERIFYING the proof
/// first under the frame's declared scheme. rejects deterministically on: an
/// unknown scheme tag, a parse failure, TRAILING BYTES after the declared
/// fields and their proof (exactly one valid encoding per frame — this is what makes
/// an appended continuation section unrepresentable; every scheme's proof is
/// self-delimiting so the boundary is the preimage's own end), an origin
/// malformed for its scheme — which INCLUDES a secp key spelled any way but
/// the canonical 33-byte compressed SEC1 form, so one private key can never
/// enter a block as two distinct origins — or a proof that does not bind the
/// whole preimage. the ordered drain treats any rejection as a deterministic no-op:
/// every honest validator rejects the identical forged frame identically.
/// the verified `origin` becomes the block's `Origin::External(pubkey)` — raw
/// key bytes, scheme not surfaced (a key's bytes cannot collide across
/// schemes without a discrete log on the other curve); the `seq` is
/// ordering/replay metadata, not surfaced.
pub fn decode_frame(bytes: &[u8]) -> Result<(Origin, Msg), Error> {
    decode_frame_with_blob(bytes).map(|(origin, msg, _)| (origin, msg))
}

/// Verify the canonical frame and return its signed required-blob digest.
/// Unknown tags, missing metadata, truncated digests and surplus proof bytes
/// are rejected. The ordinary decoder uses this same codec and drops metadata.
pub fn decode_frame_with_blob(bytes: &[u8]) -> Result<(Origin, Msg, Option<[u8; 32]>), Error> {
    let parse_err = || Error::Host(sdk::Error::module("frame_decode", "frame does not parse"));
    let mut buf = bytes;
    let (tag, rest) = buf.split_first().ok_or_else(parse_err)?;
    buf = rest;
    let scheme = KeyScheme::from_tag(*tag).ok_or_else(|| {
        Error::Host(sdk::Error::module(
            "frame_decode",
            format!("frame scheme tag {tag} is unknown"),
        ))
    })?;
    let origin = take_slice(&mut buf).ok_or_else(parse_err)?;
    // seq is ordering/replay metadata, consumed but not surfaced.
    let Some(_seq) = take_u64(&mut buf) else {
        return Err(parse_err());
    };
    let target = std::str::from_utf8(take_slice(&mut buf).ok_or_else(parse_err)?)
        .map_err(|_| parse_err())?;
    let payload = take_slice(&mut buf).ok_or_else(parse_err)?;
    let (blob_tag, rest) = buf.split_first().ok_or_else(parse_err)?;
    buf = rest;
    let required_blob = match blob_tag {
        0 => None,
        1 => {
            let (digest, rest) = buf.split_at_checked(32).ok_or_else(parse_err)?;
            buf = rest;
            Some(digest.try_into().expect("split of 32"))
        }
        _ => return Err(parse_err()),
    };
    let preimage_len = bytes.len() - buf.len();
    if !scheme.pubkey_wellformed(origin) {
        return Err(Error::Host(sdk::Error::module(
            "frame_origin_malformed",
            "frame origin is malformed for its scheme",
        )));
    }
    if !scheme.verify(origin, FRAME_NS, &bytes[..preimage_len], buf) {
        return Err(Error::Host(sdk::Error::module(
            "frame_proof_unverified",
            "frame proof does not bind this op to its origin",
        )));
    }
    Ok((
        Origin::External(origin.to_vec()),
        Msg {
            target: target.to_string(),
            payload: payload.to_vec(),
        },
        required_blob,
    ))
}

/// a frame's `(origin, seq)` submitter coordinates, without verifying the
/// signature — recovery metadata only (a restarted node scans its retained
/// frames to advance its local sequence past everything it may have framed).
/// `None` for bytes that are not a frame.
pub fn frame_origin_seq(bytes: &[u8]) -> Option<(Vec<u8>, u64)> {
    let (tag, mut buf) = bytes.split_first()?;
    KeyScheme::from_tag(*tag)?;
    let origin = take_slice(&mut buf)?;
    let seq = take_u64(&mut buf)?;
    Some((origin.to_vec(), seq))
}

/// decode ONE batch member into the [`host::BlockOp`] the block applies.
///
/// stamps `frame` with the member's content id HERE, from the ONE definition,
/// so live drain, recovery replay, and suffix catch-up cannot each stamp (or
/// forget) it differently at the call site.
pub fn decode_member(bytes: &[u8]) -> Result<host::BlockOp, Error> {
    let (origin, msg) = decode_frame(bytes)?;
    Ok(host::BlockOp {
        origin,
        msg,
        frame: frame_id(bytes),
    })
}

// ============================================================================
// the batch super-frame codec — pack N signed op-frames into ONE ordered unit.
// ============================================================================
//
// a block now carries a BATCH of op-frames. the batch super-frame is an
// UNSIGNED CONTAINER: `varint(N)` then, per member, `varint(len) || bytes`. its
// members are the existing signed op-frames, UNCHANGED — the signature and
// authenticated authorship live on each member, never on the container. the
// container's own content address is `frame_id(&batch_bytes)`; the orderer
// orders these containers, and [`OrderedNode::drain_delivered`] decodes one into
// its members and applies them as ONE block at ONE height under ONE root-hash.

/// hard cap on ONE encoded batch super-frame — the packing target for
/// [`OrderedNode::flush_batch`]. equal to [`MAX_FRAME_BYTES`]: a single member
/// (itself `<= MAX_FRAME_BYTES`) is NEVER split, so a one-member batch plus the
/// tiny length envelope can edge just over this — the real mesh 2 MiB p2p
/// message cap gives the envelope headroom over this packing target.
pub const MAX_BATCH_BYTES: usize = MAX_FRAME_BYTES;

/// hard cap on how many MEMBERS one batch super-frame carries. the byte cap
/// alone does not bound this: the smallest signed op frame is ~155 bytes, so
/// `MAX_BATCH_BYTES` (1 MiB + 16 KiB) fits ~6.8k members in one block. Each
/// member is one isolation unit in `Host::apply_block`, and one that stages
/// then fails replays every accepted member before it, so the block's
/// re-execution grows with the member count.
pub const MAX_BATCH_MEMBERS: usize = 1024;

/// hard cap on how many frames [`OrderedNode`] holds in CUSTODY at once
/// (`outstanding`, a superset of `pending_batch`). intake is unauthenticated —
/// an open HTTP route and any mesh peer's relay both land here — and custody
/// drains only as members apply at `MAX_BATCH_MEMBERS` per block, so without a
/// cap one flood holds the process's memory until it dies. eight blocks' worth
/// of backlog: enough that an honest burst never sees a refusal, small enough
/// that the flood stops at a bounded queue.
pub const MAX_CUSTODY_FRAMES: usize = 8 * MAX_BATCH_MEMBERS;

/// hard cap on the BYTES held in custody. the count cap alone does not bound
/// memory: `MAX_CUSTODY_FRAMES` max-size frames would be 8 GiB, and each is
/// held twice (once in `pending_batch`, once in `outstanding`), so the real
/// footprint is twice this number.
pub const MAX_CUSTODY_BYTES: usize = 32 << 20;

/// hard cap on how many frames ONE origin key holds in custody. the total caps
/// alone let one key fill the whole mempool and starve every other submitter;
/// this is the fairness bound. two full batches per origin: an honest burst
/// routinely exceeds one block's worth of members, and it still takes four
/// distinct keys to reach [`MAX_CUSTODY_FRAMES`].
pub const MAX_CUSTODY_FRAMES_PER_ORIGIN: usize = 2 * MAX_BATCH_MEMBERS;

/// hard cap on how many batches ONE [`OrderedNode::flush_batch`] proposes.
/// each batch costs a disk pin plus an orderer proposal on the loop that also
/// serves `/v1/query`, so a full mempool flushed in one turn stalls every
/// other lane; the remainder stays enqueued (in FIFO order) for the next turn.
pub const MAX_FLUSH_BATCHES_PER_TURN: usize = 8;

/// encoded length of `n` as canonical unsigned LEB128.
fn varint_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}

/// append `n` to `out` as canonical unsigned LEB128 (minimal, no overlong tail).
fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// read a canonical unsigned LEB128 off the front of `buf`, advancing it.
/// rejects an overlong encoding (a non-minimal trailing-zero group) and any
/// value that would overflow a u64 — a forged length can never drive
/// allocation or slicing past the input.
fn get_varint(buf: &mut &[u8]) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let (&byte, rest) = buf.split_first()?;
        *buf = rest;
        if shift >= 64 {
            return None; // an 11th group cannot fit a u64.
        }
        let low = (byte & 0x7f) as u64;
        // the top group carries only bit 63; anything above it overflows.
        if shift == 63 && low > 1 {
            return None;
        }
        result |= low << shift;
        if byte & 0x80 == 0 {
            // canonical: a multi-byte encoding whose final group is zero is
            // overlong — the value could have been written shorter.
            if byte == 0 && shift != 0 {
                return None;
            }
            return Some(result);
        }
        shift += 7;
    }
}

/// encode member frames into ONE batch super-frame: `varint(N)` then, per
/// member, `varint(len) || bytes`. member order is PRESERVED (the applied
/// order). infallible — the members are opaque bytes.
pub fn encode_batch(members: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, members.len() as u64);
    for m in members {
        put_varint(&mut out, m.len() as u64);
        out.extend_from_slice(m);
    }
    out
}

/// decode a batch super-frame back to its member frames. CANONICAL: rejects a
/// trailing-byte suffix, an overlong varint, and any member length that
/// overruns the buffer or exceeds [`MAX_FRAME_BYTES`]. a corrupt blob is an
/// `Err` — the drain treats a whole undecodable batch as one Rejected block.
pub fn decode_batch(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let corrupt = || {
        Error::Host(sdk::Error::module(
            "batch_decode",
            "batch super-frame does not parse",
        ))
    };
    let mut buf = bytes;
    let n = usize::try_from(get_varint(&mut buf).ok_or_else(corrupt)?).map_err(|_| corrupt())?;
    // every member costs at least one length byte, so N can never exceed the
    // bytes that remain — cap the pre-allocation so a forged count cannot OOM.
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(n.min(buf.len()));
    for _ in 0..n {
        let len =
            usize::try_from(get_varint(&mut buf).ok_or_else(corrupt)?).map_err(|_| corrupt())?;
        // a member can never be larger than a single frame's cap.
        if len > MAX_FRAME_BYTES {
            return Err(corrupt());
        }
        let (head, rest) = buf.split_at_checked(len).ok_or_else(corrupt)?;
        out.push(head.to_vec());
        buf = rest;
    }
    if !buf.is_empty() {
        // a trailing suffix after the declared members is not canonical.
        return Err(corrupt());
    }
    Ok(out)
}

#[cfg(test)]
mod batch_codec_tests {
    use super::*;

    #[test]
    fn batch_roundtrips_zero_one_and_many_members() {
        let cases: Vec<Vec<Vec<u8>>> = vec![
            vec![],
            vec![b"solo".to_vec()],
            vec![
                b"".to_vec(), // an empty member is legal bytes.
                b"one".to_vec(),
                vec![7u8; 300], // >127 bytes: a multi-byte length varint.
                b"last".to_vec(),
            ],
        ];
        for members in cases {
            let enc = encode_batch(&members);
            let dec = decode_batch(&enc).expect("batch roundtrips");
            assert_eq!(dec, members, "decoded members match, in order");
        }
    }

    #[test]
    fn batch_rejects_trailing_bytes() {
        let mut enc = encode_batch(&[b"x".to_vec()]);
        enc.push(0xff); // one stray byte past the last member.
        assert!(
            decode_batch(&enc).is_err(),
            "a trailing suffix is not a canonical batch"
        );
    }

    #[test]
    fn batch_rejects_length_overrunning_buffer() {
        // N=1 declaring a 100-byte member, but only 3 bytes follow.
        let mut bytes = Vec::new();
        put_varint(&mut bytes, 1);
        put_varint(&mut bytes, 100);
        bytes.extend_from_slice(b"abc");
        assert!(
            decode_batch(&bytes).is_err(),
            "a member length past the buffer end must reject"
        );
    }

    #[test]
    fn batch_rejects_overlong_varint() {
        // the count N written overlong as [0x80, 0x00] (canonical 0 is [0x00]).
        assert!(
            decode_batch(&[0x80, 0x00]).is_err(),
            "an overlong varint is not canonical"
        );
    }
}

/// total-order broadcast over opaque op frames. `submit` proposes a frame into
/// the agreed sequence (it does NOT apply anything locally); `poll_delivered`
/// yields the SAME sequence, in the SAME order, on EVERY validator. domain-
/// agnostic — it orders `Vec<u8>`, never `Msg` (the simplex port slots in behind
/// this exact shape; that is why `submit` is async).
pub trait Orderer {
    /// propose an opaque frame into the agreed order. no local apply.
    fn submit(&mut self, frame: Vec<u8>) -> impl std::future::Future<Output = Result<(), Error>>;
    /// the newly-ordered frames since the last call, in agreed order (may be
    /// empty), each paired with its agreed VIEW/height — the block coordinate the
    /// host stamps into `Env` (identical on every validator). non-blocking.
    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)>;
}

/// the deterministic agreed-order impl: accumulate a round's proposed frames,
/// then on `poll_delivered` yield them SORTED by a deterministic, node-
/// independent key (the frame bytes themselves, which lead with origin+seq).
/// every validator that accumulated the identical SET yields the byte-identical
/// SEQUENCE — so order-dependent roots converge. (this is the "sort a round's
/// accumulated ops by a deterministic key" total order; real simplex is the
/// drop-in.)
#[derive(Default)]
pub struct RoundOrderer {
    pending: Vec<Vec<u8>>,
    /// the next agreed view to stamp. monotonic across rounds, assigned per frame
    /// in delivered order — deterministic because the delivery order is (the same
    /// node-independent sort on every validator).
    next_view: u64,
}

impl RoundOrderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// resume stamping views at `next_view` — the harness analog of a real
    /// engine reopening its journal and continuing its view counter, so a
    /// resumed node's new frames land ABOVE its applied floor.
    pub fn resume_at(next_view: u64) -> Self {
        Self {
            pending: Vec::new(),
            next_view,
        }
    }
}

impl Orderer for RoundOrderer {
    async fn submit(&mut self, frame: Vec<u8>) -> Result<(), Error> {
        self.pending.push(frame);
        Ok(())
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        let mut out = std::mem::take(&mut self.pending);
        // the agreed order: a deterministic, node-independent total order over
        // the round's distinct frames. NOT arrival order.
        out.sort();
        // stamp each frame with a monotonic agreed view. the sort makes
        // frame->view identical across validators, so height/consensus_time agree.
        out.into_iter()
            .map(|f| {
                let view = self.next_view;
                self.next_view += 1;
                (view, f)
            })
            .collect()
    }
}

/// the NEGATIVE-CONTROL orderer, behind the SAME [`Orderer`] trait: it delivers
/// each validator its frames in raw ARRIVAL order — no agreed order at all. swap
/// [`RoundOrderer`] for this in the harness and nothing else changes; two nodes
/// with opposite arrival orders then apply opposite sequences and an order-
/// dependent qmdb root FORKS. that swap-only divergence is what proves the agreed
/// order is load-bearing, not decoration.
#[derive(Default)]
pub struct ArrivalOrderer {
    pending: Vec<Vec<u8>>,
    /// per-frame ascending view, arrival-ordered — deliberately NOT node-agreed
    /// (this is the negative control; opposite arrival -> opposite view stamps).
    next_view: u64,
}

impl ArrivalOrderer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Orderer for ArrivalOrderer {
    async fn submit(&mut self, frame: Vec<u8>) -> Result<(), Error> {
        self.pending.push(frame);
        Ok(())
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        // NO sort: raw arrival order is exactly the no-agreed-order fork the
        // total order prevents.
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|f| {
                let view = self.next_view;
                self.next_view += 1;
                (view, f)
            })
            .collect()
    }
}

/// the SIM orderer, behind the SAME [`Orderer`] trait: submissions PARK in
/// FIFO arrival order and deliver only when an external [`StepHandle`] releases
/// them — the scripted-scenario engine behind simnode's `/sim/step` (release
/// one) and auto mode (release all). unlike [`RoundOrderer`] it does NOT
/// byte-sort: a sim scenario's value is its EXACT authored order, which a
/// content sort would scramble. like the other orderers it stamps a monotone
/// view per delivered frame. the release budget is shared with the handle by an
/// `Arc`, so simnode's serve thread can release while its actor thread owns the
/// orderer.
pub struct StepOrderer {
    /// parked frames in FIFO (submit) order; the front releases first.
    pending: std::collections::VecDeque<Vec<u8>>,
    /// the next agreed view to stamp, monotone across releases.
    next_view: u64,
    /// the release budget the paired [`StepHandle`] commands.
    budget: std::sync::Arc<StepBudget>,
}

/// the release budget a [`StepHandle`] commands and a [`StepOrderer`] spends,
/// shared by `Arc`. `permits` counts frames cleared to deliver — `release(n)`
/// adds n, each `poll_delivered` spends up to that many (a counting budget, so
/// a release BEFORE a frame parks still delivers it on arrival). `all` latches
/// AUTO mode: every parked frame, and every future submit, delivers.
#[derive(Default)]
struct StepBudget {
    permits: std::sync::atomic::AtomicU64,
    all: std::sync::atomic::AtomicBool,
}

impl StepOrderer {
    /// a fresh sim orderer and the handle that releases its parked frames.
    #[allow(clippy::new_without_default)] // the handle must be returned too.
    pub fn new() -> (Self, StepHandle) {
        let budget = std::sync::Arc::new(StepBudget::default());
        let orderer = Self {
            pending: std::collections::VecDeque::new(),
            next_view: 0,
            budget: budget.clone(),
        };
        (orderer, StepHandle { budget })
    }
}

impl Orderer for StepOrderer {
    async fn submit(&mut self, frame: Vec<u8>) -> Result<(), Error> {
        self.pending.push_back(frame);
        Ok(())
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        use std::sync::atomic::Ordering::Relaxed;
        // auto mode delivers everything parked; otherwise spend up to `permits`
        // parked frames, FIFO from the front. a poll never spends more permits
        // than it delivers, so an over-release simply carries forward.
        let release_all = self.budget.all.load(Relaxed);
        let n = if release_all {
            self.pending.len()
        } else {
            let n = (self.budget.permits.load(Relaxed) as usize).min(self.pending.len());
            self.budget.permits.fetch_sub(n as u64, Relaxed);
            n
        };
        (0..n)
            .map(|_| {
                let frame = self.pending.pop_front().expect("n <= pending.len()");
                let view = self.next_view;
                self.next_view += 1;
                (view, frame)
            })
            .collect()
    }
}

/// the external release trigger for a [`StepOrderer`] (see its doc). `Clone`
/// (it is an `Arc` over the shared budget) and `Send + Sync` — simnode's serve
/// thread holds one while its actor thread owns the orderer.
#[derive(Clone)]
pub struct StepHandle {
    budget: std::sync::Arc<StepBudget>,
}

impl StepHandle {
    /// clear `n` more parked frames to deliver on the next drain — the
    /// `/sim/step` trigger (release one). permits ACCUMULATE: releasing before a
    /// frame parks still delivers it when it arrives.
    pub fn release(&self, n: u64) {
        self.budget
            .permits
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// latch AUTO mode: every parked frame, and every future submit, delivers on
    /// the next drain without a further `release`. a latch — auto mode is not
    /// toggled back off mid-run.
    pub fn release_all(&self) {
        self.budget
            .all
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// how a drained frame landed. every finalized frame gets exactly one of
/// these, and each is a deterministic function of the agreed order plus the
/// agreed ceiling — so dispositions are identical on every honest validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// applied via `host.submit_at`; its events are in the event queue.
    Applied,
    /// a deterministic no-op: the frame failed to decode or a module rejected
    /// the op (host-lent rolled the block back).
    Rejected,
    /// finalized at or past the cutover ceiling — never applied. NOT a final
    /// outcome for a locally-accepted frame: the ACCEPTING node re-proposes
    /// it into the new epoch at cutover (see [`OrderedNode::cutover`]), where
    /// it resolves as applied or rejected under the same [`FrameId`].
    Discarded,
}

/// one frame the ordered lane finished with — how a caller correlates its own
/// submits (by [`FrameId`]) with finalized outcomes, e.g. an app surface
/// holding a reply open until the op lands. also the drain's observability
/// record: the drain is the ONLY seam where a REMOTE validator's frame is
/// decoded, so block contents (an explorer's rows) must be captured here.
#[derive(Clone, Debug)]
pub struct DrainedFrame {
    pub id: FrameId,
    /// the app height stamped for this frame's view (`view_base + view`).
    pub height: u64,
    pub disposition: Disposition,
    /// the composed root-hash after this frame settled. a rejected frame rolls
    /// back and a discarded one never runs (hash unchanged from the previous
    /// block in both cases) — recorded regardless, so every outcome carries
    /// the boundary it left behind.
    pub root_hash: StateRoot,
    /// the decoded op this frame carried. `None` when there was nothing to
    /// decode: a frame discarded at the cutover ceiling (dropped before
    /// decoding) or one whose decode/signature check failed.
    pub op: Option<DrainedOp>,
    /// node-local, NON-CONSENSUS: why a [`Disposition::Rejected`] frame was
    /// rejected — the module's refusal framed `<token>: <sentence>`
    /// ([`host::carried_refusal`]), so a submitter's held reply splits the
    /// module's own token back off, or a short reason for a decode/signature
    /// failure. `None` for an applied or discarded frame. this rides ONLY the
    /// in-memory record: a rejection is a deterministic no-op that every honest
    /// validator computes identically, but the reason is pure observability and
    /// NEVER enters the seal, the WAL, or any hashed root.
    pub reason: Option<String>,
}

/// hex for a log line — a state root as a raw byte array is unreadable, and
/// hand-rolling this beats pulling a hex dependency into the kernel for one line.
fn hex_root(root: &StateRoot) -> String {
    root.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// a mempool-cap refusal: the stable snake_case token a submitter and a log
/// line both key on, plus the bound it hit for the human reading it.
fn custody_refused(reason: &str, bound: usize) -> Error {
    Error::Host(sdk::Error::module(
        reason,
        format!("this node's op mempool is at its {bound} bound — retry shortly"),
    ))
}

/// the decoded contents of one drained frame: authenticated authorship, the
/// root msg, and (for an applied frame) the host's deterministic dispatch
/// trace.
#[derive(Clone, Debug)]
pub struct DrainedOp {
    /// the frame's verified authorship — [`decode_frame`] yields
    /// `Origin::External(pubkey)`, the submitting validator's ed25519 key.
    pub origin: Origin,
    /// the root msg's target module.
    pub target: sdk::ModuleId,
    /// the root msg's payload bytes.
    pub payload: Vec<u8>,
    /// the block's dispatch trace, in drain order — empty for a rejected
    /// frame (a deterministic no-op leaves no trace).
    pub dispatches: Vec<host::DispatchRecord>,
    /// node-local wall-clock cost of applying this block, in microseconds —
    /// the ONE non-deterministic field. measured in THIS effectful node layer
    /// (never inside the clock-free host) and fed only into node-local metrics
    /// (the apply-latency histogram), so it can never enter consensus. differs
    /// per node.
    pub latency_us: u64,
}

/// one decoded member's identity, carried parallel to the block's ops (in
/// member order) so the drained records can be built after the block settles —
/// internal to [`OrderedNode::drain_delivered`].
struct MemberMeta {
    id: FrameId,
    origin: Origin,
    target: sdk::ModuleId,
    payload: Vec<u8>,
}

/// the durable outcome of one drained frame — everything a recovery journal
/// needs to seal the block: its height, how it landed, and the FULL registry
/// root vector after it settled. the roots are the replay positions: on boot a
/// module whose live root equals a seal's recorded root has that block (and
/// everything before it) applied, so per-block skip/apply decisions reduce to
/// root equality — no per-module op counters needed (a qmdb root is an op-log
/// commitment and never repeats; a git head oid never repeats).
#[derive(Clone, Debug)]
pub struct BlockSeal {
    pub height: u64,
    pub disposition: Disposition,
    /// every registered module's `(id, root)` AFTER this block settled, in
    /// registry (sorted-id) order — [`host::Host::module_roots`].
    pub roots: Vec<(sdk::ModuleId, StateRoot)>,
    /// the composed root-hash after this block settled.
    pub root_hash: StateRoot,
}

/// the recovery seam on the ordered lane: a write-ahead journal for finalized
/// frames plus their sealed outcomes. [`OrderedNode`] drives it at exactly the
/// points recovery needs:
///
/// - [`BlockSink::pin`] at submit — the frame bytes become durable BEFORE the
///   consensus engine can propose their digest. the in-memory content store
///   and the engine's own journal (votes/certificates, no payloads) are the
///   only other homes for those bytes, so without the pin a crash between
///   finalization and drain loses a solo network's op forever.
/// - [`BlockSink::pre_apply`] before a finalized frame mutates state (WAL
///   discipline — a crash mid-apply rolls forward from this record).
/// - [`BlockSink::seal`] after the block settles, with the post-block roots.
/// - [`BlockSink::cutover`] at each epoch cutover, so a restart can respawn
///   the engine at the persisted epoch over its existing journal partition.
///
/// errors are FATAL to the drain (see [`Error::Journal`]): applying blocks
/// that recovery will never see again silently breaks the restart contract.
pub trait BlockSink {
    /// durably record a locally-submitted frame's bytes before the orderer
    /// may propose them.
    fn pin(&mut self, frame: &[u8]) -> impl std::future::Future<Output = Result<(), Error>>;
    /// durably record a finalized frame about to be applied at `height`,
    /// with the internal work the host prepared for the block from committed
    /// state — the units a replay must run even after their sources retired
    /// them.
    fn pre_apply(
        &mut self,
        height: u64,
        frame: &[u8],
        prepared: &host::PreparedWork,
    ) -> impl std::future::Future<Output = Result<(), Error>>;
    /// durably record what a block's units observed of their siblings,
    /// BEFORE its first module commits (the host's pre-commit witness): a
    /// replay serves those observations verbatim instead of re-reading
    /// siblings that have since moved past the block.
    fn witness(
        &mut self,
        height: u64,
        witness: &host::Witness,
    ) -> impl std::future::Future<Output = Result<(), Error>>;
    /// durably record a settled block's outcome.
    fn seal(&mut self, seal: &BlockSeal) -> impl std::future::Future<Output = Result<(), Error>>;
    /// durably record an epoch cutover: the new epoch, its app-height base,
    /// the ENGINE PARTICIPANT SET it was spawned over, and the epoch's
    /// RESIDENT set (raw public-key bytes). the sets ride the record because
    /// a restart must respawn the engine (and re-track the mesh) with the
    /// EPOCH'S sets — the instantaneous valset projection may already include
    /// a change awaiting the next cutover.
    fn cutover(
        &mut self,
        epoch: u64,
        view_base: u64,
        participants: &[Vec<u8>],
        residents: &[Vec<u8>],
    ) -> impl std::future::Future<Output = Result<(), Error>>;
}

/// the no-recovery sink: every hook is an immediate `Ok` no-op. the default
/// sink, so every pre-recovery construction site and test keeps its exact
/// behavior (and cost) unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

/// the host's pre-commit witness over this node's [`BlockSink`]: what a
/// block's units observed reaches the journal before its first module
/// commits.
struct SinkWitness<'a, S: BlockSink> {
    sink: &'a mut S,
}

#[async_trait::async_trait(?Send)]
impl<S: BlockSink> host::CommitWitness for SinkWitness<'_, S> {
    async fn record(&mut self, height: u64, witness: &host::Witness) -> Result<(), String> {
        self.sink
            .witness(height, witness)
            .await
            .map_err(|e| e.to_string())
    }
}

impl BlockSink for NullSink {
    async fn pin(&mut self, _frame: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    async fn pre_apply(
        &mut self,
        _height: u64,
        _frame: &[u8],
        _prepared: &host::PreparedWork,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn witness(&mut self, _height: u64, _witness: &host::Witness) -> Result<(), Error> {
        Ok(())
    }
    async fn seal(&mut self, _seal: &BlockSeal) -> Result<(), Error> {
        Ok(())
    }
    async fn cutover(
        &mut self,
        _epoch: u64,
        _view_base: u64,
        _participants: &[Vec<u8>],
        _residents: &[Vec<u8>],
    ) -> Result<(), Error> {
        Ok(())
    }
}

/// how a block's `consensus_time` (the `Env` clock every module reads) is
/// derived from its app height, stamped into every block's [`BlockContext`] by
/// [`OrderedNode::drain_delivered`]. the default is byte-identical to the
/// pre-policy hardcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConsensusTimePolicy {
    /// the validator lane: `consensus_time = height`. the real clock rides in
    /// the frames' agreed order, not a wall read — the height IS the logical
    /// clock. DEFAULT, so every existing construction site keeps today's bytes.
    #[default]
    HeightIsTime,
    /// the sim lane's logical clock: `consensus_time = base_ms + height *
    /// block_ms` — a deterministic millisecond clock advancing one `block_ms`
    /// per block from `base_ms`, so sim receipts carry stable wall-clock-shaped
    /// times with no real clock read.
    Epoch { base_ms: u64, block_ms: u64 },
}

impl ConsensusTimePolicy {
    /// stamp the `consensus_time` for a block at app `height`.
    pub fn stamp(&self, height: u64) -> u64 {
        match self {
            Self::HeightIsTime => height,
            Self::Epoch { base_ms, block_ms } => base_ms + height * block_ms,
        }
    }
}

/// a replicated host on the ORDERED lane. owns its [`Host`] and an [`Orderer`].
/// unlike [`Node`], `submit` does NOT apply to the local host — it only proposes
/// into the agreed order; application happens exclusively in [`OrderedNode::
/// drain_delivered`], in the order the [`Orderer`] delivers, identically on every
/// validator. generic over the concrete orderer `O` (no `dyn`), so the same type
/// serves the deterministic orderer today and the simplex orderer later — and
/// over the recovery sink `S` (default [`NullSink`]: no journal, no cost).
pub struct OrderedNode<O: Orderer, S: BlockSink = NullSink> {
    host: Host,
    orderer: O,
    /// events surfaced by every block APPLIED via `drain_delivered` and not yet
    /// taken. on the ordered lane this is where the reactor's worker driver
    /// reads finalized worker requests from (via `take_events`; try-decode
    /// routing skips the purely observability events). accumulates in
    /// agreed-delivery order.
    events: Vec<Event>,
    /// the latest APPLIED consensus boundary: the last drained APP HEIGHT
    /// (`view_base + engine view`) plus the root-hash after that drain settled.
    /// this is what a state-sync service serves from
    /// (`host::Host::capture_finalized_snapshot` demands exactly this pair) —
    /// `None` until the first frame applies.
    finalized: Option<host::FinalizedBlock>,
    /// the ENGINE view whose seal set `finalized` — what a recovery layer
    /// matches finalization certificates against when persisting the floor.
    /// `None` until a block seals under the CURRENT engine (fresh boot,
    /// resume, or right after a cutover): a recovered boundary's floor is
    /// already persisted, and a new epoch's floor waits for its first block.
    finalized_view: Option<u64>,
    /// the app-height offset of the CURRENT engine's view 0. epoch cutover
    /// respawns the engine with views restarting at 0; the base keeps `Env`
    /// heights and the finalized boundary monotone across epochs
    /// (`app_height = view_base + view` — the orchestrator's epoch_base).
    view_base: u64,
    /// the last ENGINE-relative view drained (what the valset orchestrator
    /// observes and compares cutover views against). reset on epoch respawn.
    last_engine_view: Option<u64>,
    /// per-frame outcomes recorded by `drain_delivered` and not yet taken via
    /// [`OrderedNode::take_drained`]. like `events`, long-lived callers take
    /// these every drain tick; the queue accumulates until taken.
    drained: Vec<DrainedFrame>,
    /// per-BLOCK once-per-block System-injection dispatch traces (upgrade
    /// `Advance`, mailbox `DeliverPending` and its follow-ups), keyed by
    /// height — they belong to no member frame, so [`DrainedFrame`] cannot
    /// carry them (its shape is journaled). the replay paths merge these
    /// after the members' dispatches when they re-execute a block; a live
    /// node must surface the SAME rows or its derived op index diverges
    /// from every replayed peer's. taken via
    /// [`OrderedNode::take_system_dispatches`], beside `take_drained`.
    system_dispatches: Vec<(u64, Vec<host::DispatchRecord>)>,
    /// the deterministic CUTOVER CEILING: frames finalized at or past this
    /// ENGINE view are DISCARDED, not applied. every honest node discards by
    /// the same agreed rule, so a straggler op that finalizes on only some
    /// nodes while engines are being torn down can never fork app state —
    /// the accepting node re-proposes its own discards at cutover.
    view_ceiling: Option<u64>,
    /// the recovery seam (see [`BlockSink`]); [`NullSink`] when recovery is off.
    sink: S,
    /// the RESUME SKIP floor: frames whose app height is at or below this were
    /// already applied (and sealed) before a restart. the consensus engine
    /// re-reports finalizations from its reopened journal above its replay
    /// floor, and the exactly-once digest gate does not survive the process —
    /// so recovered history can be delivered again. skipping by the agreed
    /// height is deterministic; frames ABOVE the floor are genuinely new
    /// (finalized pre-crash but never drained) and apply normally. set ONCE at
    /// [`OrderedNode::resume`] and never moved: it is the boundary between
    /// "already durable elsewhere" and "this process applied it".
    resume_floor: Option<u64>,
    /// the highest app height this process has JOURNALED — advanced at every
    /// block, right before its `pre_apply`. above `resume_floor` a delivered
    /// height at or below this one cannot be a legitimate re-report: it is an
    /// out-of-order delivery, and applying it would compose an op-log-ordered
    /// qmdb root no peer can reproduce. the drain REFUSES it (see
    /// [`Error::OutOfOrder`]) instead of skipping it silently — a silent skip
    /// forks just as surely, and quietly.
    applied_floor: Option<u64>,
    /// the OBSERVATION BARRIER: when set, [`OrderedNode::drain_delivered`]
    /// ends its batch right after any block that CHANGES this module's root,
    /// buffering the undrained remainder for the next call. a caller that
    /// observes the watched module once per drain (the valset orchestrator)
    /// then always observes at exactly the changing block's view — the same
    /// view on every validator regardless of how deliveries batched locally.
    /// without the split, two nodes draining the same views in different
    /// batch shapes would observe a membership change at different views and
    /// schedule DIFFERENT epoch cutovers: a cross-node fork.
    watch_module: Option<sdk::ModuleId>,
    /// frames delivered by the orderer but deferred past an observation
    /// barrier — drained ahead of fresh deliveries on the next call.
    deferred: std::collections::VecDeque<(u64, Vec<u8>)>,
    /// CUSTODY of locally-accepted frames: everything [`OrderedNode::submit`]
    /// acked (pinned + proposed) that has not yet RESOLVED — drained below
    /// the ceiling as applied or rejected. valued with the submit `seq` (the
    /// carry order) and the frame bytes. [`OrderedNode::cutover`] resubmits
    /// whatever is still here into the new engine, so the accept contract
    /// ("Ok ⇒ ordered, or deterministically rejected — never silently lost")
    /// survives the boundary instead of dying with the discard ceiling or
    /// the torn-down engine's queue. in-memory by design: a crash already
    /// loses accepted-but-unfinalized ops (the pre-existing crash window);
    /// the cutover is the NON-crash path this closes.
    outstanding: std::collections::HashMap<FrameId, (u64, Vec<u8>)>,
    /// the BYTES of everything in `outstanding`, maintained beside it so the
    /// intake cap is a comparison rather than a walk of the whole map on every
    /// submitted frame (which is exactly the walk a flood would pay for).
    custody_bytes: usize,
    /// how many frames each ORIGIN key holds in `outstanding` — the per-origin
    /// fairness cap's counter, maintained beside the map for the same reason.
    /// an origin's entry is removed when its last frame resolves, so the map
    /// is bounded by the frame count, not by how many keys have ever submitted.
    custody_per_origin: std::collections::HashMap<Vec<u8>, usize>,
    /// ENQUEUED member frames awaiting a flush, in FIFO (enqueue) order.
    /// [`OrderedNode::submit_frame`] appends here (custody also begins in
    /// `outstanding`); [`OrderedNode::flush_batch`] drains it, greedily packs
    /// the members into batch super-frames, and proposes each to the orderer.
    /// FIFO order is the applied order, so per-node qmdb roots are well-defined.
    /// always a subset of `outstanding` (an un-flushed member is still in
    /// custody), so a cutover rebuilds it from `outstanding` and re-flushes.
    pending_batch: Vec<(FrameId, Vec<u8>)>,
    /// the out-of-band source of component BYTES for code-registry swaps. the
    /// drain reconciles running module code against the committed registry
    /// (`Host::realize_module_swaps`) BEFORE applying each block, fetching any
    /// newly-designated component through this. defaults to
    /// [`host::NoCodeSource`]: a net with no armed swap never touches it, and a
    /// node that never wired a real source FAILS CLOSED at the first armed
    /// boundary instead of silently running stale code. `Arc` so the node and
    /// its recovery sink can share the one source.
    code_source: std::sync::Arc<dyn host::CodeSource>,
    /// THE REPLAY WINDOW: the batch [`FrameId`]s this node has already
    /// journaled, newest last, bounded to [`REPLAY_WINDOW_HEIGHTS`] entries.
    /// consulted in the APPLY path (never in gossip verification — a peer
    /// votes on a digest purely because it holds the bytes, so an old batch
    /// re-proposed byte-identically finalizes normally) and so consulted by
    /// every validator, at the same block, with the same verdict.
    ///
    /// it lives on the NODE, not the orderer: the orderer is rebuilt at every
    /// epoch cutover with a fresh content store and a fresh exactly-once
    /// digest set, and [`OrderedNode::cutover`] keeps the node — so the guard
    /// survives the boundary the cutover used to re-open. a restart restores
    /// it from the recovery journal via [`OrderedNode::seed_replay_window`].
    replay_window: std::collections::VecDeque<(u64, FrameId)>,
    /// the code-swap stall: `(height, attempts)` for the block whose
    /// component bytes have not landed. a forever-retry loop that warned every
    /// tick would evict the ring; the COUNTER is the diagnosis, so it rides a
    /// latched warn. cleared the moment a boundary realizes.
    code_stall: Option<(u64, u64)>,
    /// how each block's `consensus_time` is derived from its height (see
    /// [`ConsensusTimePolicy`]). defaults to `HeightIsTime` — the validator
    /// lane's pre-policy behavior, byte-for-byte.
    time_policy: ConsensusTimePolicy,
    /// SIM ONLY: ops parked by [`OrderedNode::submit_decoded`], keyed by the
    /// placeholder frame id that rides the batch pipeline in their place. the
    /// drain takes the decoded op from here instead of verifying the placeholder
    /// bytes — so an UNSIGNED sim lane applies without the wire codec gaining an
    /// unsigned variant.
    #[cfg(feature = "sim")]
    decoded: std::collections::HashMap<FrameId, host::BlockOp>,
    /// SIM ONLY: a monotone counter minting a UNIQUE placeholder per
    /// [`OrderedNode::submit_decoded`], so two byte-identical unsigned ops are
    /// still distinct ordered units and distinct `decoded` entries.
    #[cfg(feature = "sim")]
    decoded_seq: u64,
}

impl<O: Orderer> OrderedNode<O> {
    pub fn new(host: Host, orderer: O) -> Self {
        Self::with_sink(host, orderer, NullSink)
    }
}

impl<O: Orderer, S: BlockSink> OrderedNode<O, S> {
    /// wrap `host` with an orderer and a recovery sink, starting from genesis
    /// (nothing applied). the sink journals from the very first block.
    pub fn with_sink(host: Host, orderer: O, sink: S) -> Self {
        Self {
            host,
            orderer,
            events: Vec::new(),
            drained: Vec::new(),
            system_dispatches: Vec::new(),
            finalized: None,
            finalized_view: None,
            view_base: 0,
            last_engine_view: None,
            view_ceiling: None,
            sink,
            resume_floor: None,
            applied_floor: None,
            watch_module: None,
            deferred: std::collections::VecDeque::new(),
            outstanding: std::collections::HashMap::new(),
            custody_bytes: 0,
            custody_per_origin: std::collections::HashMap::new(),
            pending_batch: Vec::new(),
            code_source: std::sync::Arc::new(host::NoCodeSource),
            replay_window: std::collections::VecDeque::new(),
            code_stall: None,
            time_policy: ConsensusTimePolicy::HeightIsTime,
            #[cfg(feature = "sim")]
            decoded: std::collections::HashMap::new(),
            #[cfg(feature = "sim")]
            decoded_seq: 0,
        }
    }

    /// RESUME after a restart: `host` already holds the recovered state at
    /// `finalized` (the journal tip a recovery replay verified), and the
    /// current epoch's app heights are based at `view_base`. the finalized
    /// boundary doubles as the resume-skip floor — re-reported history at or
    /// below it is dropped instead of re-applied.
    pub fn resume(
        host: Host,
        orderer: O,
        sink: S,
        finalized: Option<host::FinalizedBlock>,
        view_base: u64,
    ) -> Self {
        Self {
            host,
            orderer,
            events: Vec::new(),
            drained: Vec::new(),
            system_dispatches: Vec::new(),
            finalized,
            finalized_view: None,
            view_base,
            last_engine_view: None,
            view_ceiling: None,
            sink,
            resume_floor: finalized.map(|f| f.height),
            applied_floor: finalized.map(|f| f.height),
            watch_module: None,
            deferred: std::collections::VecDeque::new(),
            outstanding: std::collections::HashMap::new(),
            custody_bytes: 0,
            custody_per_origin: std::collections::HashMap::new(),
            pending_batch: Vec::new(),
            code_source: std::sync::Arc::new(host::NoCodeSource),
            replay_window: std::collections::VecDeque::new(),
            code_stall: None,
            time_policy: ConsensusTimePolicy::HeightIsTime,
            #[cfg(feature = "sim")]
            decoded: std::collections::HashMap::new(),
            #[cfg(feature = "sim")]
            decoded_seq: 0,
        }
    }

    /// RESTORE the replay window after a restart, from the journaled
    /// `(height, batch frame id)` pairs a recovery replay walked — in
    /// ascending height order. without this a restarted validator would apply
    /// a batch its running peers refuse. the seed is truncated to the newest
    /// [`REPLAY_WINDOW_HEIGHTS`] entries, exactly as the live path bounds it.
    pub fn seed_replay_window(&mut self, applied: impl IntoIterator<Item = (u64, FrameId)>) {
        self.replay_window.extend(applied);
        while self.replay_window.len() > REPLAY_WINDOW_HEIGHTS {
            self.replay_window.pop_front();
        }
    }

    /// wire the out-of-band component-byte source for code-registry swaps (the
    /// node injects a blobstore-backed one; tests inject an in-memory map). the
    /// default is [`host::NoCodeSource`] — see the field doc.
    pub fn set_code_source(&mut self, src: std::sync::Arc<dyn host::CodeSource>) {
        self.code_source = src;
    }

    /// the replay window as it stands, ascending by height — what a promotion
    /// hands the validator-ordered rebuild so the new node keeps refusing the
    /// batches the follower-ordered one already journaled.
    pub fn replay_window(&self) -> Vec<(u64, FrameId)> {
        self.replay_window.iter().copied().collect()
    }

    /// dismantle the node into its host and sink — the promotion seam: a
    /// follower-ordered replica hands both to a validator-ordered rebuild
    /// ([`Self::resume`] with an engine orderer) inside the same process.
    /// everything else (undrained events, deferred frames, the orderer) is
    /// dropped with `self`; callers only dismantle at a drained boundary.
    pub fn into_parts(self) -> (Host, S) {
        (self.host, self.sink)
    }

    /// choose how each block's `consensus_time` is derived from its height (see
    /// [`ConsensusTimePolicy`]). the default `HeightIsTime` is the validator
    /// lane; the sim backend sets `Epoch{..}` for its logical millisecond clock.
    pub fn set_consensus_time_policy(&mut self, policy: ConsensusTimePolicy) {
        self.time_policy = policy;
    }

    /// SIM ONLY — the pre-decoded ingress: enqueue an ALREADY-DECODED op to ride
    /// the same batch -> orderer -> drain pipeline as a signed client frame,
    /// WITHOUT the signature a wire frame carries. the unsigned sim lanes
    /// (`/sim/peer-block`, the `hex:` origin escape) carry origins that are not
    /// ed25519 keys and could never verify, so they cannot ride
    /// [`OrderedNode::submit_frame`]. NO wire variant: the op is stored decoded
    /// in a side table keyed by a UNIQUE placeholder frame's id; the orderer
    /// still orders only opaque bytes and [`decode_member`] is untouched (the
    /// codec stays a machine contract). returns the placeholder [`FrameId`] so
    /// the caller correlates the drained outcome exactly as with `submit_frame`.
    #[cfg(feature = "sim")]
    pub fn submit_decoded(&mut self, op: host::BlockOp) -> FrameId {
        // a UNIQUE placeholder per call: two byte-identical unsigned ops must
        // still be DISTINCT ordered units (a tie-free order key) and distinct
        // side-table entries, so key on a monotone counter, not the op content.
        self.decoded_seq += 1;
        let mut placeholder = b"sim-decoded:".to_vec();
        placeholder.extend_from_slice(&self.decoded_seq.to_le_bytes());
        let id = frame_id(&placeholder);
        // stamp the op's own frame id to the placeholder id, so a drained record
        // (keyed by the placeholder's id) and the op agree. the placeholder then
        // joins `pending_batch` exactly like a signed frame — flush packs it, the
        // orderer FIFO-orders it, and the drain resolves it via `decoded`.
        self.decoded.insert(id, host::BlockOp { frame: id, ..op });
        self.pending_batch.push((id, placeholder));
        id
    }

    /// resolve a batch member to its op: a SIM pre-decoded op parked via
    /// [`OrderedNode::submit_decoded`] (taken from the side table by its
    /// placeholder id, WITHOUT the signature check a wire frame carries), else
    /// the verifying wire decode. non-sim builds compile to exactly
    /// [`decode_member`], so the validator lane is untouched.
    fn take_decoded(&mut self, member: &[u8]) -> Result<host::BlockOp, Error> {
        #[cfg(feature = "sim")]
        if let Some(op) = self.decoded.remove(&frame_id(member)) {
            return Ok(op);
        }
        decode_member(member)
    }

    /// arm the observation barrier on `module` (see the field doc): every
    /// drain batch ends right after a block that changes its root, so a
    /// once-per-drain observer sees the change at exactly its block's view.
    pub fn watch_module(&mut self, module: impl Into<sdk::ModuleId>) {
        self.watch_module = Some(module.into());
    }

    /// EPOCH CUTOVER: replace the orderer (dropping the old one aborts its
    /// engine) and rebase app heights at `view_base` (the cutover app height —
    /// the orchestrator's epoch_base). clears the ceiling and the
    /// engine-relative view; the finalized boundary carries over. records the
    /// cutover in the recovery sink FIRST, so a restart respawns the engine at
    /// `epoch` over its own journal partition instead of a predecessor's.
    ///
    /// call this only after a final [`OrderedNode::drain_delivered`] under the
    /// ceiling — anything the old engine finalized past the ceiling was
    /// deterministically discarded on every honest node.
    ///
    /// THE BOUNDARY CARRY: every locally-accepted frame still unresolved —
    /// finalized past the ceiling (discarded) or queued in the torn-down
    /// engine — is re-pinned and resubmitted into the NEW engine here,
    /// byte-identical (same `(origin, seq)`, same [`FrameId`], so a caller
    /// holding a reply by frame id resolves when the carried frame finalizes
    /// in the new epoch). this is what keeps the accept contract true across
    /// the boundary; without it, a cutover under concurrent submit load
    /// silently drops every acked-but-unresolved op. returns the carried
    /// count. the re-pin is REQUIRED, not belt-and-braces: checkpoint pruning
    /// can drop the old epoch's pin record while the carried frame is still
    /// unfinalized, leaving a post-carry finalization unrecoverable. only the
    /// accepting node carries (custody, not origin, gates it), and even a
    /// duplicate byte-identical proposal collapses in the engine's
    /// exactly-once digest gate — a carry can never double-apply.
    pub async fn cutover(
        &mut self,
        orderer: O,
        epoch: u64,
        view_base: u64,
        participants: &[Vec<u8>],
        residents: &[Vec<u8>],
    ) -> Result<usize, Error> {
        self.sink
            .cutover(epoch, view_base, participants, residents)
            .await?;
        self.orderer = orderer;
        self.view_base = view_base;
        self.last_engine_view = None;
        // the finalized boundary carries over, but its VIEW belonged to the
        // torn-down engine's clock — the new epoch's floor waits for its own
        // first sealed block.
        self.finalized_view = None;
        self.view_ceiling = None;
        // events of pre-cutover blocks remain takeable. deferred frames
        // carry OLD-epoch views — stamping them under the new base would
        // corrupt heights, and a caller only cuts over after draining under
        // the ceiling, so any leftover here was past the ceiling (a discard);
        // the bytes of locally-accepted ones survive via the carry below.
        self.deferred.clear();
        // rebuild the pending queue from custody: every un-flushed member is
        // already in `outstanding` (a superset of `pending_batch`), so clearing
        // and rebuilding from `outstanding` carries BOTH the finalized-past-
        // ceiling discards AND anything accepted-but-never-flushed, with no
        // double-enqueue. resubmit in `seq` order so this origin's ops keep the
        // order their submitter observed them acked in. the custody counters
        // are untouched: the carry drains and reinserts the SAME set, so the
        // bytes and per-origin quotas it accounts for are unchanged.
        self.pending_batch.clear();
        let mut carried: Vec<(FrameId, u64, Vec<u8>)> = self
            .outstanding
            .drain()
            .map(|(id, (seq, frame))| (id, seq, frame))
            .collect();
        carried.sort_unstable_by_key(|(_, seq, _)| *seq);
        let count = carried.len();
        for (id, seq, frame) in carried {
            self.pending_batch.push((id, frame.clone()));
            // custody continues: a second cutover before this frame resolves
            // carries it again.
            self.outstanding.insert(id, (seq, frame));
        }
        // ONE flush re-pins + re-proposes the carried members to the NEW
        // orderer as fresh batches (the carry's durability: pinned before
        // cutover returns), preserving FIFO/`seq` order across the boundary.
        self.flush_batch().await?;
        Ok(count)
    }

    /// set the deterministic discard boundary for the CURRENT engine (see the
    /// field doc). idempotent; cleared by [`OrderedNode::cutover`].
    pub fn set_view_ceiling(&mut self, ceiling: u64) {
        self.view_ceiling = Some(ceiling);
    }

    /// the armed discard boundary, for a caller that must apply the SAME rule
    /// before the drain does — a backfill lane taking frames from an
    /// unverified source refuses what this node would discard, instead of
    /// admitting the bytes and discovering the lie after the fold.
    pub fn view_ceiling(&self) -> Option<u64> {
        self.view_ceiling
    }

    /// the last ENGINE-relative finalized view this node drained — the number
    /// the valset orchestrator observes. `None` since the last cutover.
    pub fn last_engine_view(&self) -> Option<u64> {
        self.last_engine_view
    }

    /// SUBMIT — propose a locally-originated msg into the agreed order. framed
    /// with `(origin, seq)` for a tie-free order key + replay identity. does NOT
    /// touch the local host: `root_hash()` is unchanged until the order delivers
    /// this frame back through [`OrderedNode::drain_delivered`] (the semantic
    /// shift — no optimistic echo). returns the frame's [`FrameId`] so the
    /// caller can recognize this op's outcome in [`OrderedNode::take_drained`].
    pub async fn submit(
        &mut self,
        signer: &PrivateKey,
        seq: u64,
        msg: Msg,
    ) -> Result<FrameId, Error> {
        let frame = encode_frame(signer, seq, &msg);
        self.submit_frame(frame).await
    }

    /// take custody of an ALREADY-SIGNED frame (the relay entry point: a
    /// resident signs with its own identity key, a validator injects). the
    /// signature is verified BEFORE anything is pinned — junk from the wire
    /// must never enter the durable store or the orderer. custody semantics
    /// are identical to [`OrderedNode::submit`]: pin, propose, track
    /// outstanding (the cutover carry and the exactly-once digest gate treat
    /// a relayed frame exactly like a local one).
    pub async fn submit_frame(&mut self, frame: Vec<u8>) -> Result<FrameId, Error> {
        // the SIZE GUARD (#215): an over-cap frame must be rejected HERE, as a
        // plain error the submitter sees — commonware's p2p sender ASSERTS on
        // its message cap, so letting the frame through would panic the
        // proposer's gossip task instead. rejected BEFORE the pin: nothing is
        // journaled, proposed, or held in custody for it. guards the relay
        // entry too — a resident's over-cap frame must not panic its relay.
        if frame.len() > MAX_FRAME_BYTES {
            return Err(Error::Host(sdk::Error::module(
                "frame_too_large",
                format!(
                    "op frame is {} bytes, over the {MAX_FRAME_BYTES}-byte cap — split the payload",
                    frame.len()
                ),
            )));
        }
        decode_member(&frame)?;
        let id = frame_id(&frame);
        let (origin, seq) = frame_origin_seq(&frame).expect("decode_member verified the envelope");
        // ONE CUSTODY ENTRY PER FrameId. `outstanding` is the source of truth
        // (`pending_batch` is a subset of it), so a frame already in custody is
        // ACKNOWLEDGED IDEMPOTENTLY, never enqueued a second time: the batch
        // codec is a plain concatenation and the drain applies every member, so
        // a second copy in `pending_batch` would pack the SAME signed op twice
        // into one batch and execute it twice at one height under one root hash
        // on every validator. the caller gets the same id it got the first
        // time, so a reply held against that id still resolves.
        if self.outstanding.contains_key(&id) {
            return Ok(id);
        }
        // the MEMPOOL CAPS. intake is unauthenticated on both doors, and
        // custody drains only as members apply, so every accepted frame must
        // pass a total bound (count and bytes) and a per-origin bound before it
        // is held. refused as a plain deterministic error the submitter sees.
        let over_frame_count = self.outstanding.len() >= MAX_CUSTODY_FRAMES;
        if over_frame_count {
            return Err(custody_refused("mempool_frames_full", MAX_CUSTODY_FRAMES));
        }
        let over_byte_budget = self.custody_bytes + frame.len() > MAX_CUSTODY_BYTES;
        if over_byte_budget {
            return Err(custody_refused("mempool_bytes_full", MAX_CUSTODY_BYTES));
        }
        let origin_held = self
            .custody_per_origin
            .get(&origin)
            .copied()
            .unwrap_or_default();
        let over_origin_quota = origin_held >= MAX_CUSTODY_FRAMES_PER_ORIGIN;
        if over_origin_quota {
            return Err(custody_refused(
                "mempool_origin_full",
                MAX_CUSTODY_FRAMES_PER_ORIGIN,
            ));
        }
        // ENQUEUE, don't propose: the frame joins `pending_batch` (FIFO) and
        // enters custody. it is not pinned or proposed until [`flush_batch`]
        // packs it into a batch super-frame — that is where the durable pin +
        // orderer proposal happen, once per batch. custody begins HERE so a
        // cutover before the flush still carries the accepted-but-unflushed op
        // (the accept contract holds without a flush having run).
        self.custody_bytes += frame.len();
        *self.custody_per_origin.entry(origin).or_default() += 1;
        self.pending_batch.push((id, frame.clone()));
        self.outstanding.insert(id, (seq, frame));
        Ok(id)
    }

    /// CUSTODY ENDS for one member: drop it from `outstanding` and give its
    /// bytes and its origin's quota back. the ONE release site, so the caps'
    /// counters cannot drift from the map they bound.
    fn release_custody(&mut self, id: &FrameId) {
        let Some((_, frame)) = self.outstanding.remove(id) else {
            return;
        };
        self.custody_bytes -= frame.len();
        let Some((origin, _)) = frame_origin_seq(&frame) else {
            return;
        };
        let std::collections::hash_map::Entry::Occupied(mut held) =
            self.custody_per_origin.entry(origin)
        else {
            return;
        };
        *held.get_mut() -= 1;
        if *held.get() == 0 {
            held.remove();
        }
    }

    /// how many member frames are enqueued awaiting the next [`flush_batch`].
    pub fn pending_batch_len(&self) -> usize {
        self.pending_batch.len()
    }

    /// how many frames this node holds in CUSTODY: accepted and neither applied
    /// nor rejected yet. a superset of [`OrderedNode::pending_batch_len`] (a
    /// flushed member is still in custody until it resolves), and the number
    /// the mempool caps bound — so it, not the flush queue, is what a
    /// "pending ops" gauge must report.
    pub fn custody_len(&self) -> usize {
        self.outstanding.len()
    }

    /// FLUSH — drain `pending_batch` (FIFO), greedily pack the member frames
    /// into batch super-frames up to [`MAX_BATCH_BYTES`] and
    /// [`MAX_BATCH_MEMBERS`], and for each batch
    /// PIN its bytes then PROPOSE it to the orderer. returns the number of
    /// batches submitted (`Ok(0)` when nothing was pending). at most
    /// [`MAX_FLUSH_BATCHES_PER_TURN`] batches leave in one call — a full
    /// mempool proposed back to back would stall `/v1/query` on the same loop;
    /// the FIFO remainder stays enqueued for the next turn.
    ///
    /// FIFO order is preserved end-to-end: a member's position in a batch is
    /// its enqueue order, which is the applied order — reordering would fork a
    /// single node's own op-log-order-dependent (qmdb) root. members stay in
    /// custody (`outstanding`); they leave only when a batch finalizes and the
    /// member resolves in [`drain_delivered`]. a new batch is started when
    /// adding the next member would push the encoded batch past either cap; a
    /// single member is never split (it is `<= MAX_FRAME_BYTES`, so it always
    /// forms at least its own batch even if that batch edges over the packing
    /// target — the mesh cap has the headroom).
    pub async fn flush_batch(&mut self) -> Result<usize, Error> {
        let mut batches = 0usize;
        while batches < MAX_FLUSH_BATCHES_PER_TURN {
            let members = self.take_one_batch();
            if members.is_empty() {
                break;
            }
            self.propose_batch(&members).await?;
            batches += 1;
        }
        Ok(batches)
    }

    /// peel the longest FIFO PREFIX of `pending_batch` that fits ONE batch
    /// super-frame's caps, in enqueue (= applied) order. a single member is
    /// never split: the first member is always taken, even when it alone edges
    /// the packing target over (it is `<= MAX_FRAME_BYTES`, and the mesh cap
    /// has the headroom). empty exactly when nothing is enqueued.
    fn take_one_batch(&mut self) -> Vec<Vec<u8>> {
        // running encoded size of the members already counted (each member's
        // `varint(len) || bytes`); the `varint(N)` header is added when
        // projecting whether the next member still fits.
        let mut members_bytes: usize = 0;
        let mut count: usize = 0;
        for (_id, frame) in &self.pending_batch {
            let contrib = varint_len(frame.len() as u64) + frame.len();
            let projected = varint_len(count as u64 + 1) + members_bytes + contrib;
            let overflows = projected > MAX_BATCH_BYTES || count == MAX_BATCH_MEMBERS;
            if count > 0 && overflows {
                break;
            }
            members_bytes += contrib;
            count += 1;
        }
        self.pending_batch
            .drain(..count)
            .map(|(_id, frame)| frame)
            .collect()
    }

    /// encode one batch super-frame, durably PIN it, then PROPOSE it to the
    /// orderer. the pin lands BEFORE the proposal (the same WAL-before-propose
    /// discipline the single-frame path used): once the engine journals a
    /// finalization these bytes are the only thing standing between a crash and
    /// an unrecoverable finalized batch.
    async fn propose_batch(&mut self, members: &[Vec<u8>]) -> Result<(), Error> {
        let batch = encode_batch(members);
        self.sink.pin(&batch).await?;
        self.orderer.submit(batch).await?;
        Ok(())
    }

    /// DRAIN — apply every BATCH the order delivered, STRICTLY in agreed order.
    /// each delivered `(view, frame)` is ONE batch super-frame applied via
    /// `host.submit_block` as ONE block at ONE height under ONE root-hash, with
    /// per-member outcomes surfaced as N [`DrainedFrame`]s (all sharing that
    /// root-hash). returns the count of BATCHES processed (0 when idle) so a test
    /// can drive to a fixpoint deterministically.
    ///
    /// ## rejected vs fatal
    ///
    /// a DETERMINISTIC rejection (a member's decode failure or module error, an
    /// undecodable whole batch, a rejected System injection) is a no-op: every
    /// honest validator finalized the identical bytes and rejects them
    /// identically — the drain keeps going, and the block-level seal disposition
    /// is Applied iff the batch MOVED state. a FATAL boundary fault
    /// ([`host::SubmitError::Fatal`]) is node-local: this registry is now
    /// indeterminate, so the drain STOPS and surfaces [`Error::Fatal`] — applying
    /// even one more finalized batch would compound a state no validator agreed on.
    pub async fn drain_delivered(&mut self) -> Result<usize, Error> {
        // fresh deliveries queue BEHIND anything a previous observation
        // barrier deferred — agreed order is preserved.
        self.deferred.extend(self.orderer.poll_delivered());
        let mut applied = 0usize;
        let mut last_view: Option<u64> = None;
        // the last view with a JOURNALED outcome (applied or rejected) — what
        // the finalized STATE boundary may advance to. a DISCARDED view moves
        // only the engine clock: it is never journaled, so a boundary that
        // included it would claim a height recovery cannot reproduce — and
        // right after a cutover it would collide with the new epoch's first
        // height, demanding a finalization floor that cannot exist until the
        // new epoch finalizes (a joiner syncing that boundary would wedge).
        let mut last_sealed_view: Option<u64> = None;
        while let Some((view, frame)) = self.deferred.pop_front() {
            // a FINALIZED op counts as processed whether or not it applies
            // cleanly — and its VIEW advances the engine clock either way (the
            // view was agreed; discarding or rejecting its op is the same
            // deterministic no-op on every honest node). without this, a node
            // could never OBSERVE the views that carry it past its own cutover.
            applied += 1;
            last_view = Some(view);
            // the agreed view is the block coordinate: the APP HEIGHT is the
            // engine view offset by the epoch base, so heights and the logical
            // clock stay monotone across epoch cutovers — identical on every
            // validator.
            let height = self.view_base + view;
            // the RESUME SKIP floor: recovered state already contains this
            // frame (it was applied and sealed before the restart); the engine
            // re-reported it from its reopened journal. dropping it by agreed
            // height is the same deterministic no-op everywhere.
            let is_resume_replay = self.resume_floor.is_some_and(|floor| height <= floor);
            if is_resume_replay {
                tracing::debug!(
                    target: "ducktape::consensus",
                    height,
                    view,
                    reason = "resume_replay",
                    "skipped a re-reported frame the recovered state already contains"
                );
                continue;
            }
            // MONOTONICITY. above the resume floor every height is this
            // process's own to journal, so a delivery at or below the last one
            // it journaled is not history — it is an out-of-order delivery,
            // and the composed qmdb root is op-log-order-dependent. refuse
            // loudly: applying it forks this node against every peer, and
            // skipping it forks it just as hard while looking healthy.
            if let Some(applied) = self.applied_floor
                && height <= applied
            {
                return Err(Error::OutOfOrder { height, applied });
            }
            let batch_id = frame_id(&frame);
            // the CUTOVER CEILING: a batch finalized at or past the agreed
            // cutover view is DISCARDED WHOLE — the same view-based rule on
            // every honest node, so a straggler batch finalizing during
            // teardown on only some nodes cannot fork app state. never
            // journaled: a discard leaves no state for a restart to recover.
            if let Some(ceiling) = self.view_ceiling
                && view >= ceiling
            {
                // one Discarded outcome per member (a caller correlates by its
                // own member FrameId), and KEEP every member in `outstanding`:
                // none applied anywhere, so the cutover carries them into the
                // new epoch (see [`OrderedNode::cutover`]). an undecodable
                // discarded batch has no members to name and nothing in custody
                // to carry — one record under the batch's own id.
                match decode_batch(&frame) {
                    Ok(members) => {
                        for member in &members {
                            self.drained.push(DrainedFrame {
                                id: frame_id(member),
                                height,
                                disposition: Disposition::Discarded,
                                root_hash: self.host.root_hash(),
                                op: None,
                                reason: None,
                            });
                        }
                    }
                    Err(_) => self.drained.push(DrainedFrame {
                        id: batch_id,
                        height,
                        disposition: Disposition::Discarded,
                        root_hash: self.host.root_hash(),
                        op: None,
                        reason: None,
                    }),
                }
                // a discard seals nothing and journals nothing, so without
                // this line a batch vanishes from the log entirely — the one
                // shape that looks exactly like a halted chain (#1766).
                tracing::debug!(
                    target: "ducktape::consensus",
                    height,
                    view,
                    ceiling,
                    reason = "cutover_ceiling",
                    "discarded a batch finalized past the cutover ceiling"
                );
                continue;
            }
            // THE REPLAY WINDOW: this exact batch already applied at a recent
            // height. consensus cannot catch this — a validator votes for any
            // digest whose bytes it holds, so anyone who kept a finalized
            // batch can re-propose it byte-identically and have it finalize at
            // a NEW height, executing every member's signed op a second time.
            // the refusal lives HERE, in the apply path, keyed on a protocol
            // constant, so every validator reaches it at the same block with
            // the same verdict. journaled Rejected like any other deterministic
            // whole-batch no-op, so the height still seals: its block record
            // first, carrying no work (the refusal runs none), then the seal
            // — recovery replays a seal only against the record it seals. the
            // refusal precedes code-swap realization on purpose: a refused
            // height realizes nothing, since an admission realized here would
            // move the roots of a block that applies nothing.
            let replayed = in_replay_window(&self.replay_window, &batch_id);
            if replayed {
                self.sink
                    .pre_apply(height, &frame, &host::PreparedWork::default())
                    .await?;
                self.drained.push(DrainedFrame {
                    id: batch_id,
                    height,
                    disposition: Disposition::Rejected,
                    root_hash: self.host.root_hash(),
                    op: None,
                    reason: Some("batch replayed".to_string()),
                });
                self.seal(height, Disposition::Rejected).await?;
                self.applied_floor = Some(height);
                self.remember_applied(height, batch_id);
                last_sealed_view = Some(view);
                tracing::warn!(
                    target: "ducktape::consensus",
                    height,
                    view,
                    reason = "batch_replayed",
                    "refused a batch this node already applied"
                );
                continue;
            }
            // CODE-SWAP REALIZATION: reconcile every hot-swappable module's
            // running code against the committed code registry's decision for
            // `height`, BEFORE this block journals or applies — its dispatches
            // must execute the code the registry designates for `height`. keyed
            // purely on committed state + height, so live, restart-replay, and
            // catch-up all realize the identical swap points. FAIL-CLOSED: a
            // node lacking (or holding tampered) bytes for an armed hash must
            // not apply this block on stale code — put the frame back (nothing
            // journaled yet) and surface the stall; the drain retries once the
            // bytes arrive. NEVER a fork: every honest node holding the bytes
            // realizes identically, and one that doesn't stops here. STALLED,
            // not fatal: the frame goes back at the FRONT (nothing journaled
            // yet) and the next drain retries it — the retry is the fetch
            // pump. a caller that fail-stops here halts a chain that was only
            // waiting for bytes.
            if let Err(e) = self
                .host
                .realize_module_swaps(height, self.code_source.as_ref())
                .await
            {
                self.deferred.push_front((view, frame));
                // the stalled frame was counted as processed on pop; it was
                // not, and it will be counted again on the retry.
                return Err(self.note_code_stall(height, e, applied.saturating_sub(1)));
            }
            // a realized boundary clears the stall: the next miss counts from 1.
            self.code_stall = None;
            // below the ceiling this batch RESOLVES here as ONE block at ONE
            // height. WAL discipline: the batch bytes are finalized and about to
            // mutate state — journal them ONCE FIRST, so a crash mid-apply rolls
            // forward from this record instead of losing a finalized batch.
            // this height is now this process's own: journal it and advance the
            // monotonicity floor together, so the next delivery below it is
            // refused rather than composed into the root out of order.
            self.applied_floor = Some(height);
            // the block's internal work, read from committed state BEFORE the
            // frame mutates anything, rides the WAL record: a replay runs the
            // same units after their sources have retired them.
            let prepared = self.host.prepare_work(height).await.map_err(Error::Host)?;
            self.sink.pre_apply(height, &frame, &prepared).await?;
            // an undecodable batch is a DETERMINISTIC whole-block no-op: every
            // honest node finalized the identical bytes and rejects them
            // identically (no fork), and a byzantine proposer cannot halt honest
            // nodes with one corrupt batch.
            let members = match decode_batch(&frame) {
                Ok(m) => m,
                Err(_) => {
                    self.drained.push(DrainedFrame {
                        id: batch_id,
                        height,
                        disposition: Disposition::Rejected,
                        root_hash: self.host.root_hash(),
                        op: None,
                        reason: Some("batch decode failed".to_string()),
                    });
                    self.seal(height, Disposition::Rejected).await?;
                    self.remember_applied(height, batch_id);
                    last_sealed_view = Some(view);
                    continue;
                }
            };
            // decode each member into the ops the block applies. a REPEATED
            // member is skipped: the batch codec is a plain concatenation and
            // both copies decode to the same origin + msg (`seq` is discarded),
            // so applying both executes one signed op twice at one height. the
            // local intake refuses a duplicate at `submit_frame`, but a
            // byzantine proposer packs whatever it likes — so the skip lives
            // HERE too, keyed on the member's own content address, and every
            // honest node computes the identical skip set from the identical
            // bytes. no fork, and no per-member log line (a repeated member is
            // attacker-controlled: one debug per batch, carrying the count). a
            // member that fails to decode is a deterministic no-op: EXCLUDED
            // from the ops and recorded Rejected after the block settles (it
            // shares the block root-hash). the rest carry their identity
            // parallel to `ops`, in member (= applied, = enqueue/FIFO) order,
            // for building the drained records.
            let mut ops: Vec<host::BlockOp> = Vec::new();
            let mut op_meta: Vec<MemberMeta> = Vec::new();
            let mut decode_fail: Vec<(FrameId, String)> = Vec::new();
            let mut seen: std::collections::HashSet<FrameId> =
                std::collections::HashSet::with_capacity(members.len());
            let mut repeated = 0usize;
            for member in &members {
                let mid = frame_id(member);
                let is_repeat = !seen.insert(mid);
                if is_repeat {
                    repeated += 1;
                    continue;
                }
                match self.take_decoded(member) {
                    Ok(op) => {
                        op_meta.push(MemberMeta {
                            id: mid,
                            origin: op.origin.clone(),
                            target: op.msg.target.clone(),
                            payload: op.msg.payload.clone(),
                        });
                        ops.push(op);
                    }
                    // keep the codec's refusal FRAMED (`<reason>: <sentence>`)
                    // — a submitter's held reply surfaces it and the receipt
                    // lane splits it back into its token. node-local
                    // observability only.
                    Err(Error::Host(sdk::Error::Module { reason, sentence })) => {
                        decode_fail.push((mid, sdk::refusal::encode(&reason, &sentence)));
                    }
                    Err(e) => decode_fail.push((mid, e.to_string())),
                }
            }
            if repeated > 0 {
                tracing::debug!(
                    target: "ducktape::consensus",
                    height,
                    view,
                    repeated,
                    reason = "batch_member_repeated",
                    "skipped members a proposer packed more than once into one batch"
                );
            }
            // the observation barrier compares the watched root across the WHOLE
            // batch — only an applied member can move it (rejected members roll
            // back, discards never run).
            let watched_before = self
                .watch_module
                .as_deref()
                .map(|m| self.host.module_root(m));
            // apply the members as ONE block. ctx.origin is UNUSED on the batch
            // path — each member carries its own origin in `ops`, which the host
            // stamps into that member's Env.
            let started = std::time::Instant::now();
            let result = self
                .host
                .submit_block_prepared(
                    BlockContext {
                        height,
                        consensus_time: self.time_policy.stamp(height),
                        origin: Origin::System,
                    },
                    ops,
                    prepared,
                    &mut SinkWitness {
                        sink: &mut self.sink,
                    },
                )
                .await;
            // node-local apply cost of the WHOLE batch — the metrics plane's one
            // non-consensus signal, timed HERE in the effectful node layer (never
            // inside the clock-free host). shared by every member's record.
            let latency_us = started.elapsed().as_micros() as u64;
            let mut outcome = match result {
                Ok(outcome) => outcome,
                // a boundary fault is node-local: this registry is now
                // indeterminate, so STOP — applying more finalized ops would
                // compound a state no validator agreed on.
                Err(e @ host::SubmitError::Fatal(_)) => return Err(e.into()),
                // submit_block folds a MEMBER rejection into its MemberOutcome
                // and never errors the whole batch for one; a whole-batch
                // Rejected can only come from the once-per-block System
                // injection (`Advance`) rejecting — a deterministic no-op.
                // record it batch-level and keep draining.
                Err(host::SubmitError::Rejected(e)) => {
                    self.drained.push(DrainedFrame {
                        id: batch_id,
                        height,
                        disposition: Disposition::Rejected,
                        root_hash: self.host.root_hash(),
                        op: None,
                        reason: Some(host::carried_refusal(&e)),
                    });
                    self.seal(height, Disposition::Rejected).await?;
                    self.remember_applied(height, batch_id);
                    last_sealed_view = Some(view);
                    continue;
                }
            };
            // N DrainedFrames per batch, all sharing the ONE post-batch root-hash.
            let batch_hash = outcome.root_hash;
            self.events.extend(std::mem::take(&mut outcome.events));
            // the block-level seal disposition is DRAIN-based, not root-hash-based:
            // a block is Applied iff it ran real work — any member applied, or a
            // once-per-block System injection dispatched. this is identical live,
            // on forward replay, AND on a torn-heal's PARTIAL commit (which aborts
            // the already-durable mover, so its root-hash cannot be trusted). exactly
            // one seal per batch, below.
            let has_system = outcome.ran_internal_work();
            // surface the internal work's dispatch traces (calls, deliveries,
            // injections) beside the member records: the replay paths
            // (recovery, suffix catch-up) merge these AFTER the members'
            // dispatches when re-executing this block, so a live node must
            // hand its index consumer the same rows or live and replayed op
            // indexes diverge.
            if has_system {
                self.system_dispatches
                    .push((height, outcome.internal_dispatches()));
            }
            let mut any_applied = false;
            let (mut applied_count, mut rejected_count, mut nop_count) = (0usize, 0usize, 0usize);
            // one record per applying member, in member (input/FIFO) order; the
            // host guarantees `members` is 1:1 with `ops` in input order. custody
            // ends for each resolved member.
            for (meta, member_outcome) in op_meta.into_iter().zip(outcome.members) {
                let MemberMeta {
                    id: mid,
                    origin: op_origin,
                    target: op_target,
                    payload: op_payload,
                } = meta;
                self.release_custody(&mid);
                // the heartbeat nop is rejected BY DESIGN (its target does not
                // exist); it is counted on its own so `rejected` below means
                // only what the comment on the idle-block line says it means.
                let is_heartbeat_nop = op_target == NOP_TARGET;
                let (disposition, dispatches, reason) = match member_outcome {
                    MemberOutcome::Applied { dispatches } => {
                        any_applied = true;
                        applied_count += 1;
                        (Disposition::Applied, dispatches, None)
                    }
                    // the host carries the reject error FRAMED
                    // (`host::carried_refusal`), so the receipt lane splits it
                    // into the module's own token and sentence. node-local only.
                    MemberOutcome::Rejected { reason } => {
                        if is_heartbeat_nop {
                            nop_count += 1;
                        } else {
                            rejected_count += 1;
                        }
                        (Disposition::Rejected, Vec::new(), Some(reason))
                    }
                };
                self.drained.push(DrainedFrame {
                    id: mid,
                    height,
                    disposition,
                    root_hash: batch_hash,
                    op: Some(DrainedOp {
                        origin: op_origin,
                        target: op_target,
                        payload: op_payload,
                        dispatches,
                        latency_us,
                    }),
                    reason,
                });
            }
            // members that failed to decode: recorded AFTER the outcome so they
            // share the block root-hash. custody ends — a decode-fail can never
            // apply, so it must not be carried at a cutover.
            for (mid, decode_reason) in decode_fail {
                self.release_custody(&mid);
                self.drained.push(DrainedFrame {
                    id: mid,
                    height,
                    disposition: Disposition::Rejected,
                    root_hash: batch_hash,
                    op: None,
                    reason: Some(decode_reason),
                });
            }
            let block_disp = if any_applied || has_system {
                Disposition::Applied
            } else {
                Disposition::Rejected
            };
            self.seal(height, block_disp).await?;
            self.remember_applied(height, batch_id);
            // the block spine. NOTHING in this repo ever said "height H produced
            // root-hash X" — and fork triage, upgrade verification, and "is my node
            // keeping up" all start exactly there.
            //
            // gated on `any_applied`: an idle chain heartbeats a nop block every
            // second, and at `info` that would fill the 4096-line ring with nothing
            // in ~68 minutes, evicting the evidence around whatever you were hunting.
            if any_applied || has_system {
                tracing::info!(
                    target: "ducktape::consensus",
                    height,
                    view,
                    root_hash = %hex_root(&batch_hash),
                    applied = applied_count,
                    rejected = rejected_count,
                    nops = nop_count,
                    "block committed"
                );
            } else {
                // the member counts ride it: the heartbeat nop is `nops` (its
                // rejection is the design), so an "idle block" with a non-zero
                // `rejected` is a REAL op silently dying.
                tracing::debug!(
                    target: "ducktape::consensus",
                    height,
                    view,
                    rejected = rejected_count,
                    nops = nop_count,
                    "idle block"
                );
            }
            last_sealed_view = Some(view);
            // OBSERVATION BARRIER (once per batch): end the drain right after a
            // batch that moved the watched root, so a once-per-drain observer
            // sees this batch's view — the same observation point on every
            // validator, regardless of how deliveries batched locally.
            if let Some(before) = watched_before {
                let module = self
                    .watch_module
                    .as_deref()
                    .expect("watched_before implies watch_module");
                if self.host.module_root(module) != before {
                    break;
                }
            }
        }
        if let Some(view) = last_view {
            self.last_engine_view = Some(view);
        }
        if let Some(view) = last_sealed_view {
            let height = self.view_base + view;
            // monotone: a resume-skipped re-report must never regress the
            // finalized boundary below the recovered tip.
            if self.finalized.is_none_or(|f| height > f.height) {
                self.finalized = Some(host::FinalizedBlock {
                    height,
                    root_hash: self.host.root_hash(),
                });
                self.finalized_view = Some(view);
            }
        }
        Ok(applied)
    }

    /// count one code-swap stall at `height` and narrate it on the first
    /// attempt and every [`CODE_STALL_WARN_EVERY`] after — an unconditional
    /// warn in a forever-retry loop is a log bomb that evicts the evidence,
    /// and the attempt COUNT is the diagnosis.
    fn note_code_stall(&mut self, height: u64, e: sdk::Error, applied: usize) -> Error {
        let attempts = self
            .code_stall
            .filter(|(stalled, _)| *stalled == height)
            .map_or(1, |(_, seen)| seen + 1);
        self.code_stall = Some((height, attempts));
        let reason = e.to_string();
        let narrate = attempts == 1 || attempts.is_multiple_of(CODE_STALL_WARN_EVERY);
        if narrate {
            tracing::warn!(
                target: "ducktape::consensus",
                height,
                attempts,
                reason = "module_code_unresolved",
                "the drain is stalled awaiting module code: {reason}"
            );
        }
        Error::CodeStalled {
            height,
            applied,
            reason,
        }
    }

    /// remember a batch this node journaled, evicting past
    /// [`REPLAY_WINDOW_HEIGHTS`]. called once per SEALED block, whatever its
    /// disposition: a rejected batch re-proposed later is the same replay, and
    /// the bound must be a pure count of sealed heights or two validators
    /// would remember different depths.
    fn remember_applied(&mut self, height: u64, batch: FrameId) {
        self.replay_window.push_back((height, batch));
        while self.replay_window.len() > REPLAY_WINDOW_HEIGHTS {
            self.replay_window.pop_front();
        }
    }

    /// journal a settled block's outcome: disposition + the post-block root
    /// vector (the replay positions) + the composed root-hash.
    async fn seal(&mut self, height: u64, disposition: Disposition) -> Result<(), Error> {
        let seal = BlockSeal {
            height,
            disposition,
            roots: self.host.module_roots(),
            root_hash: self.host.root_hash(),
        };
        self.sink.seal(&seal).await
    }

    /// the latest APPLIED consensus boundary — what a state-sync service serves
    /// from. `None` until the first delivered frame applies.
    pub fn finalized(&self) -> Option<host::FinalizedBlock> {
        self.finalized
    }

    /// the ENGINE view whose seal set [`OrderedNode::finalized`] — what a
    /// recovery layer matches finalization certificates against when
    /// persisting the floor. `None` until a block seals under the current
    /// engine (fresh boot, resume, or right after a cutover).
    pub fn finalized_view(&self) -> Option<u64> {
        self.finalized_view
    }

    /// the current root-hash of the wrapped host.
    pub fn root_hash(&self) -> StateRoot {
        self.host.root_hash()
    }

    /// the `consensus_time` a block at `height` carries, under this node's
    /// [`ConsensusTimePolicy`] — the same derivation
    /// [`OrderedNode::drain_delivered`] stamps into every applied block's
    /// `Env`. lets a status projection report the exact clock modules compare
    /// `expires_at` against, whatever the policy (a block height on the
    /// validator lane, a millisecond epoch on the sim lane).
    pub fn stamp_consensus_time(&self, height: u64) -> u64 {
        self.time_policy.stamp(height)
    }

    /// the [`ConsensusTimePolicy`] this node stamps blocks under — lets a
    /// status projection report which UNIT `consensus_time` is expressed in.
    pub fn consensus_time_policy(&self) -> ConsensusTimePolicy {
        self.time_policy
    }

    /// take the events accumulated by applied blocks since the last call. the
    /// host-owned reactor drains these, offers each to its workers (try-decode
    /// routing — a `WorkerRequest` is claimed, anything else falls through as
    /// observability), and submits each resulting `OracleResult` op back
    /// through the ordered lane (the oracle-as-op over consensus).
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// take the per-frame outcomes recorded by [`OrderedNode::drain_delivered`]
    /// since the last call, in agreed order. the drop-in counterpart of
    /// [`OrderedNode::take_events`] for callers holding replies open on their
    /// own submitted [`FrameId`]s.
    pub fn take_drained(&mut self) -> Vec<DrainedFrame> {
        std::mem::take(&mut self.drained)
    }

    /// take the once-per-block System-injection dispatch traces recorded
    /// since the last call, `(height, dispatches)` in drain order — the
    /// index consumer appends each block's entry AFTER that block's member
    /// dispatches, exactly where the replay paths put them. these belong to
    /// no member frame, so they ride beside [`OrderedNode::take_drained`],
    /// never inside it.
    pub fn take_system_dispatches(&mut self) -> Vec<(u64, Vec<host::DispatchRecord>)> {
        std::mem::take(&mut self.system_dispatches)
    }

    /// borrow the recovery sink mutably — the pump drives checkpointing and
    /// floor-cert persistence through the same store the drain journals into.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// borrow the orderer (finalization-cert and gate inspection — the pump
    /// reads these to decide when a floor certificate is safe to persist).
    pub fn orderer(&self) -> &O {
        &self.orderer
    }

    /// mutably borrow the orderer. the replica fold driver feeds its
    /// follower orderer through this — observe/admit are orderer-side
    /// operations that must not require dismantling the node.
    pub fn orderer_mut(&mut self) -> &mut O {
        &mut self.orderer
    }

    /// Readiness preflight against the running module's retained state shape.
    pub fn check_module_replacement(&mut self, id: &str, bytes: &[u8]) -> Result<(), sdk::Error> {
        self.host.check_module_replacement(id, bytes)
    }

    /// borrow the wrapped host (queries, module_root inspection, ...).
    pub fn host(&self) -> &Host {
        &self.host
    }
}
