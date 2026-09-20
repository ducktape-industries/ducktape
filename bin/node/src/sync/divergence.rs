//! the root-divergence watch: does a peer's finalized state root agree with
//! ours at the height we both finalized?
//!
//! simplex orders `sha256(frame)` digests — the certificate binds ordered
//! BYTES, never a root — so two nodes whose fold produced different state
//! agree on every block and keep finalizing, voting and serving `/v1`. the
//! roots are already on the wire: the detection lane's [`statesync::TipCoords`]
//! carries `(height, root_hash)` as an atomic pair, every validator answers
//! it, and until now only a node that was CATCHING UP ever compared one.
//!
//! DETECTION ONLY. nothing here refuses a peer, drops a connection, re-routes
//! a source, gates admission or votes — a divergence is named in the log and
//! that is all. the composed root is not committed in consensus (that is a
//! wire change and its own arc), so this makes the fork nameable within a
//! poll cycle rather than preventing it.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use commonware_p2p::Sender as P2pSender;
use sdk::StateRoot;
use statesync::{SyncRequest, SyncResponse};

use crate::sync::serve::SyncStateRequest;

/// how often a validator polls ONE co-peer for its tip coordinates.
///
/// the compare only lands when the two tips sit at the same height, so the
/// tick is a sampling rate, not a detection latency: a divergence is
/// permanent (each root folds the last), so any later aligned poll names it.
/// one request per tick, round-robin over the book, keeps the chatter flat in
/// a fleet of any size.
const ROOT_POLL_TICK: std::time::Duration = std::time::Duration::from_secs(12);

/// the divergence warn's latch stride: first sighting, then every Nth.
///
/// a fork does not heal, so this repeats for the life of the node — an
/// unlatched warn on a 12s poll would evict the 4096-line ring roughly hourly
/// per diverged peer and destroy the context around the first sighting, which
/// is the line an operator actually needs.
const WARN_EVERY: u64 = 25;

/// compare one peer's finalized tip with our own and NAME a disagreement.
///
/// `seen` counts the divergent observations per peer — it is the latch's
/// memory, and the returned count is what the line carried (`None` when this
/// observation was not named). the decision is a function of its arguments so
/// it is exercisable without a node, a clock or a socket.
///
/// UNALIGNED HEIGHTS ARE NOT EVIDENCE. two honest nodes are constantly one
/// block apart, and a root at a height is immutable once finalized — so the
/// compare is skipped unless both tips name the same height, and when it does
/// fire there is no false-positive reading of it.
pub(crate) fn note_peer_root(
    mine: (u64, StateRoot),
    peer: &str,
    theirs: (u64, StateRoot),
    seen: &mut std::collections::BTreeMap<String, u64>,
) -> Option<u64> {
    let aligned = mine.0 == theirs.0;
    let agreed = mine.1 == theirs.1;
    if !aligned || agreed {
        return None;
    }
    let count = seen.entry(peer.to_string()).or_insert(0);
    *count += 1;
    let attempts = *count;
    let name_it = attempts == 1 || attempts.is_multiple_of(WARN_EVERY);
    if !name_it {
        return None;
    }
    tracing::warn!(
        target: "ducktape::statesync",
        peer = %peer,
        height = mine.0,
        ours = %noded::hex_bytes(mine.1.as_bytes()),
        theirs = %noded::hex_bytes(theirs.1.as_bytes()),
        attempts,
        reason = "root_divergence",
        "a peer's state root differs from ours at the same finalized height — \
         one of the two folded different state and neither side will self-heal"
    );
    Some(attempts)
}

/// pick this round's poll target: the next entry after `cursor` in `members`
/// that is not `me`, round-robin. `None` when there is nobody else to poll
/// (a solo validator, or a member list this node reads as itself alone) —
/// generic over the key type so the selection is exercisable with plain
/// strings, no real keypair required.
///
/// re-deriving this from a fresh `members` slice every tick is what makes a
/// promotion or a drain take effect immediately: there is no standing
/// rotation state to fall out of sync with the valset, only a cursor into
/// whatever list this round was handed.
pub(crate) fn select_candidate<K: PartialEq + Clone>(
    members: &[K],
    me: &K,
    cursor: usize,
) -> Option<K> {
    let len = members.len();
    (0..len).find_map(|step| {
        let candidate = &members[(cursor + step) % len];
        (candidate != me).then(|| candidate.clone())
    })
}

/// the validator's forever watch: poll one co-validator per tick on the
/// detection lane it already answers for others, and compare its root with
/// ours.
///
/// a validator polls nobody otherwise — `fetch_tip_coords` has exactly one
/// caller, the parked resident's loop — so on a validator-only network
/// nothing ever read a peer's tip at all. the candidate pool is the
/// COMMITTED VALIDATOR SET, never the transport book: a resident carries no
/// statesync server (`replica::park`'s dispatch loop only completes its own
/// waiters), so a peer book that also holds residents burns the poll's one
/// slot per tick on a structural dead end, stretching detection latency with
/// the fleet's resident count instead of holding it flat.
pub(crate) async fn watch_root_divergence<S>(
    client: crate::blob_fetch::ServeLaneBlobClient<S>,
    state_tx: futures::channel::mpsc::Sender<SyncStateRequest>,
    metrics: noded::NodeMetrics,
    me: ed25519::PublicKey,
    label: String,
) where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    let mut seen = std::collections::BTreeMap::new();
    let mut cursor = 0usize;
    let mut state_tx = state_tx;
    loop {
        tokio::time::sleep(ROOT_POLL_TICK).await;
        watch_once(
            &client,
            &mut state_tx,
            &metrics,
            &mut cursor,
            &mut seen,
            &me,
            &label,
        )
        .await;
    }
}

/// one watch tick — the whole body, so it is reachable without a clock.
#[allow(clippy::too_many_arguments)]
async fn watch_once<S>(
    client: &crate::blob_fetch::ServeLaneBlobClient<S>,
    state_tx: &mut futures::channel::mpsc::Sender<SyncStateRequest>,
    metrics: &noded::NodeMetrics,
    cursor: &mut usize,
    seen: &mut std::collections::BTreeMap<String, u64>,
    me: &ed25519::PublicKey,
    label: &str,
) where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    // one round trip serves both halves: our own tip (the compare's other
    // side) and the committed member list this tick polls from — read FRESH
    // every tick, off the same loop-owned state the Standing gate and the
    // epoch cutover already read, so a promotion or a drain that just
    // committed is honoured on the very next poll, never a stale rotation.
    let Some((mine, members)) = local_state(state_tx).await else {
        return;
    };
    let Some(peer) = select_candidate(&members, me, *cursor) else {
        return; // no co-validator to poll (solo, or the only member is us).
    };
    *cursor = cursor.wrapping_add(1);
    let asked = client
        .request_from(peer.clone(), SyncRequest::TipCoords)
        .await;
    // an unreachable or unhelpful peer is the ordinary state of a mesh, and
    // this loop retries forever: per-attempt noise, never a warn.
    let Ok(SyncResponse::TipCoords(theirs)) = asked else {
        tracing::debug!(
            target: "ducktape::statesync",
            node = %label,
            peer = %noded::hex_bytes(peer.as_ref()),
            reason = "tip_coords_unanswered",
            "root divergence watch skipped this peer"
        );
        return;
    };
    // the watch's only evidence that it RAN — the poll reached a co-peer, the
    // peer answered, and this node read its own tip back. silence otherwise
    // means agreement, which is indistinguishable from a lane that was never
    // wired, so an e2e can gate on this line.
    tracing::debug!(
        target: "ducktape::statesync",
        node = %label,
        peer = %noded::hex_bytes(peer.as_ref()),
        ours = mine.0,
        theirs = theirs.height,
        reason = "root_polled",
        "root divergence watch compared tips with a co-peer"
    );
    // the same answer carries this node's LAG — what a co-validator has
    // finalized against what we serve. the projection decides what the gap
    // means; nothing here polls for it (#2498).
    metrics.record_peer_tip(theirs.height);
    note_peer_root(
        mine,
        &noded::hex_bytes(peer.as_ref()),
        (theirs.height, theirs.root_hash),
        seen,
    );
}

/// this node's own finalized `(height, root)` and the committed validator
/// set, read together through the consensus loop's
/// [`SyncStateRequest::TipCoords`] seam — the same fresh `participants` read
/// the drain loop's own cutover and the Standing gate use, never the
/// transport book that also carries residents. `None` while the loop is
/// shutting down or has no finalized boundary yet.
async fn local_state(
    state_tx: &mut futures::channel::mpsc::Sender<SyncStateRequest>,
) -> Option<((u64, StateRoot), Vec<ed25519::PublicKey>)> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    futures::SinkExt::send(state_tx, SyncStateRequest::TipCoords { reply })
        .await
        .ok()?;
    let coords = answer.await.ok()?.ok()?;
    let members = coords
        .participants
        .iter()
        .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
        .collect();
    Some(((coords.height, coords.root_hash), members))
}

#[cfg(test)]
mod tests {
    use super::{WARN_EVERY, note_peer_root, select_candidate};
    use sdk::StateRoot;

    fn root(byte: u8) -> StateRoot {
        StateRoot([byte; 32])
    }

    /// the whole compare in one pass: only an ALIGNED height is evidence, an
    /// agreeing root is silence, and a divergence is named on the first
    /// sighting and every `WARN_EVERY`th after — the fork never heals, so the
    /// poll re-observes it forever.
    #[test]
    fn only_an_aligned_disagreement_is_named_and_it_latches() {
        let mut seen = std::collections::BTreeMap::new();

        // agreeing at the same height: nothing named, nothing counted.
        assert_eq!(
            note_peer_root((7, root(1)), "aa", (7, root(1)), &mut seen),
            None
        );
        // disagreeing at DIFFERENT heights: two honest nodes one block apart,
        // which is the steady state of every healthy mesh.
        assert_eq!(
            note_peer_root((7, root(1)), "aa", (8, root(2)), &mut seen),
            None
        );
        assert_eq!(
            note_peer_root((9, root(1)), "aa", (8, root(2)), &mut seen),
            None
        );
        assert!(seen.is_empty(), "no divergence was observed yet");

        // the real thing: same height, different root.
        assert_eq!(
            note_peer_root((7, root(1)), "bb", (7, root(2)), &mut seen),
            Some(1),
            "the first sighting is always named"
        );
        for _ in 2..WARN_EVERY {
            assert_eq!(
                note_peer_root((7, root(1)), "bb", (7, root(2)), &mut seen),
                None
            );
        }
        assert_eq!(
            note_peer_root((7, root(1)), "bb", (7, root(2)), &mut seen),
            Some(WARN_EVERY),
            "then every Nth, carrying the count that IS the diagnosis"
        );
        assert_eq!(seen.get("bb"), Some(&WARN_EVERY));

        // the latch is per PEER: a second diverged peer is named on its own
        // first sighting rather than swallowed by the first one's stride.
        assert_eq!(
            note_peer_root((7, root(1)), "cc", (7, root(3)), &mut seen),
            Some(1)
        );
        assert_eq!(seen.get("aa"), None, "an agreeing peer mints no entry");
    }

    /// the candidate pool round-robins over the members handed to it THIS
    /// round, skipping only our own key — so a member list that dropped a
    /// resident, gained a promotion, or shrank to a fresh valset between
    /// ticks is honoured on the very next call, with no rotation state
    /// surviving from the prior tick.
    #[test]
    fn candidate_selection_skips_self_and_round_robins_over_the_current_members() {
        let members = ["a", "b", "c"];
        let me = "a";

        // cursor 0 lands on "a" — skip to the next entry, "b".
        assert_eq!(select_candidate(&members, &me, 0), Some("b"));
        // cursor 1 lands on "b" directly.
        assert_eq!(select_candidate(&members, &me, 1), Some("b"));
        // cursor 2 lands on "c" directly.
        assert_eq!(select_candidate(&members, &me, 2), Some("c"));
        // the cursor wraps: 3 % 3 == 0 lands on "a" again — skip to "b".
        assert_eq!(select_candidate(&members, &me, 3), Some("b"));

        // a solo member list of just ourselves has nobody to poll.
        assert_eq!(select_candidate(&["a"], &me, 0), None);
        // an empty member list (the loop shutting down mid-read) likewise.
        assert_eq!(select_candidate::<&str>(&[], &me, 0), None);

        // re-reading a SMALLER list (a drain took a member out) never panics
        // and still finds the survivor.
        assert_eq!(select_candidate(&["a", "c"], &me, 5), Some("c"));
    }
}
