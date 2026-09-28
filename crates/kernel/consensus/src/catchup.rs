use std::collections::BTreeSet;

use commonware_codec::Decode as _;
use commonware_consensus::Reporter as _;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Activity, Certificate as Heard};
use commonware_consensus::types::{Height, Round};
use commonware_cryptography::certificate::{Scheme as _, Verifier as _};
use commonware_cryptography::ed25519::PublicKey;
use commonware_parallel::Sequential;
use commonware_runtime::IoBuf;
use commonware_utils::vec::NonEmptyVec;
use futures::{Stream, StreamExt as _};
use node::Digest;

use crate::Network;
use crate::marshal::{Certificate, MarshalMailbox};
use crate::roster::Roster;

/// The rounds a follower remembers reporting, newest kept. Every validator
/// broadcasts its own finalization, so a follower hears up to one copy per
/// validator of each, all within a few views of one another.
const REPORTED: usize = 64;

// a signal is moved once, down a stream: boxing the key buys nothing
#[allow(clippy::large_enum_variant)]
pub(crate) enum Signal {
    Seated(u64),
    Heard {
        epoch: u64,
        peer: PublicKey,
        certificate: Option<IoBuf>,
    },
}

pub(crate) struct Hint {
    pub height: u64,
    pub peer: PublicKey,
}

pub(crate) enum Step {
    Hint(Hint),
    /// A certificate heard for the seated epoch, which this node follows
    /// without an engine.
    Follow {
        epoch: u64,
        certificate: IoBuf,
    },
}

pub(crate) struct CatchUp {
    network: Network,
    roster: Roster,
    seated: Option<u64>,
}

impl CatchUp {
    pub(crate) fn new(network: Network, roster: Roster) -> CatchUp {
        CatchUp {
            network,
            roster,
            seated: None,
        }
    }

    pub(crate) fn step(&mut self, signal: Signal) -> Option<Step> {
        match signal {
            Signal::Seated(epoch) => self.on_seated(epoch),
            Signal::Heard {
                epoch,
                peer,
                certificate,
            } => self.on_heard(epoch, peer, certificate),
        }
    }

    fn on_seated(&mut self, epoch: u64) -> Option<Step> {
        self.seated = Some(epoch);
        None
    }

    fn on_heard(&self, epoch: u64, peer: PublicKey, certificate: Option<IoBuf>) -> Option<Step> {
        let seated = self.seated?;
        let network_is_ahead = epoch > seated;
        if network_is_ahead {
            return Some(Step::Hint(Hint {
                height: self.network.anchor(seated + 1),
                peer,
            }));
        }
        let follows = epoch == seated && !self.roster.participates(seated);
        let certificate = certificate.filter(|_| follows)?;
        Some(Step::Follow { epoch, certificate })
    }
}

pub(crate) async fn run(
    mut rng: impl rand_core::CryptoRng,
    network: Network,
    roster: Roster,
    mut marshal: MarshalMailbox,
    mut signals: impl Stream<Item = Signal> + Unpin,
) {
    let mut catch_up = CatchUp::new(network, roster.clone());
    let mut reported = BTreeSet::new();
    while let Some(signal) = signals.next().await {
        match catch_up.step(signal) {
            Some(Step::Hint(hint)) => {
                marshal.hint_finalized(Height::new(hint.height), NonEmptyVec::new(hint.peer));
            }
            Some(Step::Follow { epoch, certificate }) => {
                let heard = finalization(&mut rng, &roster, epoch, &certificate, &mut reported);
                if let Some(finalization) = heard {
                    marshal.report(Activity::Finalization(finalization));
                }
            }
            None => {}
        }
    }
}

/// The finalization a follower of `epoch` heard, taken only when it names
/// `epoch` and a quorum of the validators seated for `epoch` signed it: the
/// check a seated engine makes on the same certificate. A copy for a round
/// in `reported` is skipped unverified (the marshal ignores a repeat anyway);
/// a verified one's round joins it.
fn finalization(
    rng: &mut impl rand_core::CryptoRng,
    roster: &Roster,
    epoch: u64,
    certificate: &IoBuf,
    reported: &mut BTreeSet<Round>,
) -> Option<Certificate> {
    let verifier = roster.verifier(epoch)?;
    // an epoch that seats nobody certifies nothing
    if verifier.participants().is_empty() {
        return None;
    }
    let config = verifier.certificate_codec_config();
    let Heard::Finalization(finalization) =
        Heard::<Scheme, Digest>::decode_cfg(certificate.as_ref(), &config).ok()?
    else {
        return None;
    };
    let round = finalization.round();
    // decoding copies bytes; the signatures are the cost worth skipping
    if round.epoch().get() != epoch || reported.contains(&round) {
        return None;
    }
    if !finalization.verify(rng, &verifier, &Sequential) {
        return None;
    }
    reported.insert(round);
    if reported.len() > REPORTED {
        reported.pop_first();
    }
    Some(finalization)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::ops::Range;

    use commonware_codec::Encode as _;
    use commonware_consensus::simplex::types::{Finalization, Finalize, Proposal};
    use commonware_consensus::types::{Epoch, View};
    use commonware_cryptography::{Signer as _, ed25519, sha256};
    use commonware_utils::non_empty;
    use commonware_utils::ordered::Set;
    use rand_core::{TryCryptoRng, TryRng};

    use super::*;

    const NAMESPACE: &[u8] = b"catchup";

    /// Counts its draws: a batch verification draws its seed first.
    struct Draws(u64);

    impl TryRng for Draws {
        type Error = Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Infallible> {
            self.0 += 1;
            Ok(0x9e37_79b9)
        }

        fn try_next_u64(&mut self) -> Result<u64, Infallible> {
            self.0 += 1;
            Ok(0x9e37_79b9_7f4a_7c15)
        }

        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
            self.0 += 1;
            dst.fill(0x5a);
            Ok(())
        }
    }

    impl TryCryptoRng for Draws {}

    #[test]
    fn a_second_copy_of_a_finalization_is_not_verified_again() {
        let mut keys: Vec<_> = (1..=4).map(ed25519::PrivateKey::from_seed).collect();
        keys.sort_by_key(|key| key.public_key());
        let validators =
            Set::try_from(keys.iter().map(|key| key.public_key()).collect::<Vec<_>>()).unwrap();
        let roster = Roster::new(NAMESPACE.to_vec(), None);
        roster.seat(1, validators.clone());
        let verifier = roster.verifier(1).unwrap();
        // the certificate `signers` of `keys` assembled for `view`, as one
        // validator broadcasts it
        let copy = |view: u64, signers: Range<usize>| {
            let proposal = Proposal::new(
                Round::new(Epoch::new(1), View::new(view)),
                View::new(view - 1),
                sha256::Digest([view as u8; 32]),
            );
            let finalizes: Vec<_> = keys[signers]
                .iter()
                .map(|key| {
                    let scheme = Scheme::signer(NAMESPACE, validators.clone(), key.clone());
                    Finalize::sign(&scheme.unwrap(), proposal.clone()).unwrap()
                })
                .collect();
            let signed =
                Finalization::from_finalizes(&verifier, non_empty![@finalizes.iter()], &Sequential)
                    .unwrap();
            IoBuf::from(Heard::<Scheme, Digest>::Finalization(signed).encode())
        };
        let mut rng = Draws(0);
        let mut reported = BTreeSet::new();

        assert!(finalization(&mut rng, &roster, 1, &copy(3, 0..4), &mut reported).is_some());
        let draws = rng.0;
        assert!(draws > 0, "the first copy is verified");
        // another validator's copy of the same round, a different quorum
        assert!(finalization(&mut rng, &roster, 1, &copy(3, 1..4), &mut reported).is_none());
        assert_eq!(rng.0, draws, "the second copy is not verified");
        // an earlier round not yet reported is still verified and taken
        assert!(finalization(&mut rng, &roster, 1, &copy(2, 0..3), &mut reported).is_some());
        assert!(rng.0 > draws);
    }
}
