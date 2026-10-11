//! Each validator's newest finalize vote this node's engine heard, as the
//! height of the block it voted for.
//!
//! The engine reports every signer-unique vote in its retained window,
//! one that lands after its certificate reached quorum included, so a
//! validator whose votes usually arrive last shows here though no
//! certificate carries it. A vote names a proposal, not a height: it
//! counts once this node has applied the block it names, whichever of the
//! two comes second. A vote for a block this node never applies (an
//! orphaned notarization, a digest no block has) never counts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use commonware_actor::Feedback;
use commonware_consensus::Reporter;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Activity, Finalize};
use commonware_consensus::types::Round;
use commonware_cryptography::ed25519::PublicKey;
use commonware_utils::ordered::Set;
use node::Digest;

use crate::engine::VIEW_RETENTION;
use crate::marshal::MarshalMailbox;

/// The applied blocks a late vote can still name. The engine admits votes
/// down to `VIEW_RETENTION` views below its finalized view, a view holds
/// one block at most, and this node applies only finalized blocks, so a
/// vote the engine admits names one of the last `VIEW_RETENTION + 1`
/// applied blocks or one not applied yet; the rest is margin.
const APPLIED: usize = 2 * VIEW_RETENTION as usize;

/// Votes kept waiting for their block, newest rounds kept: bounds what a
/// lagging node, or a validator voting for digests no block has, can make
/// this node hold.
const PENDING: usize = 256;

/// The book an engine's reporter fills and the node reads. Every height in
/// it is one the caller of [`Votes::applied`] passed, so a reader that
/// takes the lock that caller holds never sees a height past its tip.
#[derive(Clone, Default)]
pub struct Votes(Arc<Mutex<Book>>);

#[derive(Default)]
struct Book {
    /// The keys of the validators this node's engine is seated with, none
    /// while it runs no engine: the only votes the book takes.
    seated: BTreeSet<Vec<u8>>,
    /// The blocks this node applied last, oldest first.
    applied: VecDeque<(Digest, u64)>,
    /// Voters' keys, by the round and digest they voted for, while the
    /// block is not applied here.
    pending: BTreeMap<(Round, Digest), Vec<Vec<u8>>>,
    /// Each validator key's newest counted height.
    signed: BTreeMap<Vec<u8>, u64>,
}

impl Votes {
    /// Counts the votes waiting for the block at `height` this node just
    /// applied, and keeps it for the votes still to come.
    pub fn applied(&self, digest: Digest, height: u64) {
        let mut book = self.book();
        let Book {
            applied,
            pending,
            signed,
            ..
        } = &mut *book;
        if applied.len() == APPLIED {
            applied.pop_front();
        }
        applied.push_back((digest, height));
        pending.retain(|(_, voted), voters| {
            if *voted != digest {
                return true;
            }
            for key in voters.drain(..) {
                raise(signed, key, height);
            }
            false
        });
    }

    /// Each validator key's height: the newest block this node applied
    /// that the validator sent this node's engine a finalize vote for.
    pub fn heights(&self) -> BTreeMap<Vec<u8>, u64> {
        self.book().signed.clone()
    }

    /// Forgets every vote and takes none until the next [`Votes::seat`]: a
    /// node that stops validating hears none, what it heard before would
    /// only age, and its dropped engine can still report one more.
    pub(crate) fn clear(&self) {
        *self.book() = Book::default();
    }

    /// Seats the book for an engine seated with `validators`: it takes only
    /// their votes and forgets every other key's, so a validator seated again
    /// after a demotion shows no height until it votes again.
    pub(crate) fn seat(&self, validators: &Set<PublicKey>) {
        let mut book = self.book();
        let Book {
            seated,
            pending,
            signed,
            ..
        } = &mut *book;
        *seated = validators.iter().map(|key| key.as_ref().to_vec()).collect();
        signed.retain(|key, _| seated.contains(key));
        pending.retain(|_, voters| {
            voters.retain(|key| seated.contains(key));
            !voters.is_empty()
        });
    }

    /// The reporter for an engine seated with `validators`: it hands every
    /// activity to `marshal` unchanged and books each finalize vote first.
    pub(crate) fn recorder(&self, marshal: MarshalMailbox, validators: Set<PublicKey>) -> Recorder {
        Recorder {
            marshal,
            votes: self.clone(),
            validators: Arc::new(validators),
        }
    }

    /// Books `finalize` under its signer's key in `validators`, the set its
    /// engine was seated with (the one the signer index points into).
    fn heard(&self, validators: &Set<PublicKey>, finalize: &Finalize<Scheme, Digest>) {
        let Some(key) = validators.get(usize::from(finalize.attestation.signer)) else {
            return;
        };
        let key = key.as_ref().to_vec();
        let proposal = &finalize.proposal;
        let mut book = self.book();
        let Book {
            seated,
            applied,
            pending,
            signed,
        } = &mut *book;
        // a dropped engine can report once more after its seat moved on
        if !seated.contains(&key) {
            return;
        }
        if let Some(&(_, height)) = applied.iter().find(|(id, _)| *id == proposal.payload) {
            raise(signed, key, height);
            return;
        }
        pending
            .entry((proposal.round, proposal.payload))
            .or_default()
            .push(key);
        if pending.len() > PENDING {
            pending.pop_first();
        }
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.0.lock().expect("the vote book lock is never poisoned")
    }
}

/// A replayed or reordered vote never lowers a height.
fn raise(signed: &mut BTreeMap<Vec<u8>, u64>, key: Vec<u8>, height: u64) {
    let newest = signed.entry(key).or_insert(height);
    *newest = (*newest).max(height);
}

/// An engine's reporter. The engine reports a network vote before its
/// signature is checked, and one past the quorum is never checked; the
/// mesh authenticates its sender and the batcher refuses a sender that is
/// not the vote's signer. That is enough for a status row: a validator can
/// misstate only its own.
#[derive(Clone)]
pub(crate) struct Recorder {
    marshal: MarshalMailbox,
    votes: Votes,
    validators: Arc<Set<PublicKey>>,
}

impl Reporter for Recorder {
    type Activity = Activity<Scheme, Digest>;

    fn report(&mut self, activity: Self::Activity) -> Feedback {
        if let Activity::Finalize(finalize) = &activity {
            self.votes.heard(&self.validators, finalize);
        }
        self.marshal.report(activity)
    }
}

#[cfg(test)]
mod tests {
    use commonware_consensus::simplex::types::Proposal;
    use commonware_consensus::types::{Epoch, View};
    use commonware_cryptography::{Signer as _, ed25519, sha256};

    use super::*;

    const NAMESPACE: &[u8] = b"votes";

    struct Seated {
        keys: Vec<ed25519::PrivateKey>,
        validators: Set<PublicKey>,
    }

    impl Seated {
        fn new() -> Seated {
            let mut keys: Vec<_> = (1..=4).map(ed25519::PrivateKey::from_seed).collect();
            keys.sort_by_key(|key| key.public_key());
            let validators =
                Set::try_from(keys.iter().map(|key| key.public_key()).collect::<Vec<_>>()).unwrap();
            Seated { keys, validators }
        }

        /// A book seated with these validators.
        fn votes(&self) -> Votes {
            let votes = Votes::default();
            votes.seat(&self.validators);
            votes
        }

        fn vote(&self, votes: &Votes, voter: usize, view: u64, id: Digest) {
            let scheme =
                Scheme::signer(NAMESPACE, self.validators.clone(), self.keys[voter].clone())
                    .unwrap();
            let proposal = Proposal::new(
                Round::new(Epoch::new(0), View::new(view)),
                View::new(view - 1),
                id,
            );
            votes.heard(
                &self.validators,
                &Finalize::sign(&scheme, proposal).unwrap(),
            );
        }

        fn height(&self, votes: &Votes, voter: usize) -> Option<u64> {
            let key = self.keys[voter].public_key();
            votes.heights().get(key.as_ref()).copied()
        }
    }

    fn block(n: u8) -> Digest {
        sha256::Digest([n; 32])
    }

    #[test]
    fn a_vote_after_its_block_is_applied_counts() {
        let seated = Seated::new();
        let votes = seated.votes();
        votes.applied(block(5), 5);
        // the quorum finalized block 5 and this node applied it; the
        // fourth vote arrives late and still counts
        seated.vote(&votes, 3, 7, block(5));
        assert_eq!(seated.height(&votes, 3), Some(5));
        assert_eq!(seated.height(&votes, 0), None, "a validator not heard");
    }

    #[test]
    fn a_vote_ahead_of_this_node_waits_for_its_block() {
        let seated = Seated::new();
        let votes = seated.votes();
        votes.applied(block(5), 5);
        seated.vote(&votes, 0, 7, block(5));
        seated.vote(&votes, 0, 8, block(6));
        assert_eq!(seated.height(&votes, 0), Some(5), "block 6 is not applied");
        votes.applied(block(6), 6);
        assert_eq!(seated.height(&votes, 0), Some(6));
        // a vote replayed for an older block never lowers it
        seated.vote(&votes, 0, 7, block(5));
        assert_eq!(seated.height(&votes, 0), Some(6));
    }

    #[test]
    fn a_vote_for_a_block_never_applied_never_counts() {
        let seated = Seated::new();
        let votes = seated.votes();
        seated.vote(&votes, 1, 7, block(99));
        seated.vote(&votes, 2, 6, block(3));
        for height in 1..=3 {
            votes.applied(block(height), u64::from(height));
        }
        assert_eq!(seated.height(&votes, 1), None, "block 99 never applied");
        assert_eq!(seated.height(&votes, 2), Some(3));
    }

    #[test]
    fn the_waiting_votes_are_bounded_oldest_round_first() {
        let seated = Seated::new();
        let votes = seated.votes();
        seated.vote(&votes, 2, 3, block(1));
        for view in 100..100 + PENDING as u64 {
            seated.vote(&votes, 1, view, sha256::Digest([0xee; 32]));
        }
        let book = votes.book();
        assert_eq!(book.pending.len(), PENDING);
        assert!(
            book.pending
                .keys()
                .all(|(round, _)| round.view().get() >= 100),
            "the oldest round went first"
        );
    }

    #[test]
    fn clearing_forgets_every_vote() {
        let seated = Seated::new();
        let votes = seated.votes();
        votes.applied(block(5), 5);
        seated.vote(&votes, 0, 7, block(5));
        seated.vote(&votes, 1, 8, block(6));
        votes.clear();
        votes.applied(block(6), 6);
        assert!(votes.heights().is_empty());
    }

    #[test]
    fn a_cleared_book_takes_no_vote_until_it_is_seated_again() {
        let seated = Seated::new();
        let votes = seated.votes();
        votes.applied(block(5), 5);
        votes.clear();
        votes.applied(block(5), 5);
        // the dropped engine's batcher reports once more
        seated.vote(&votes, 0, 7, block(5));
        assert!(votes.heights().is_empty());
        votes.seat(&seated.validators);
        seated.vote(&votes, 0, 7, block(5));
        assert_eq!(seated.height(&votes, 0), Some(5));
    }

    #[test]
    fn a_validator_seated_again_starts_with_no_height() {
        let seated = Seated::new();
        let votes = seated.votes();
        votes.applied(block(5), 5);
        seated.vote(&votes, 0, 7, block(5));
        seated.vote(&votes, 3, 7, block(5));
        seated.vote(&votes, 3, 8, block(6));
        // validator 3 is demoted; its old engine reports it once more
        let three: Vec<_> = seated.validators.iter().take(3).cloned().collect();
        votes.seat(&Set::try_from(three).unwrap());
        seated.vote(&votes, 3, 8, block(6));
        votes.applied(block(6), 6);
        assert_eq!(seated.height(&votes, 0), Some(5));
        assert_eq!(seated.height(&votes, 3), None);
        // and seated again: nothing from its old seat survives
        votes.seat(&seated.validators);
        assert_eq!(seated.height(&votes, 3), None);
    }
}
