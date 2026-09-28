use commonware_codec::Decode as _;
use commonware_consensus::Reporter as _;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Activity, Certificate as Heard};
use commonware_consensus::types::Height;
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
    while let Some(signal) = signals.next().await {
        match catch_up.step(signal) {
            Some(Step::Hint(hint)) => {
                marshal.hint_finalized(Height::new(hint.height), NonEmptyVec::new(hint.peer));
            }
            Some(Step::Follow { epoch, certificate }) => {
                if let Some(finalization) = finalization(&mut rng, &roster, epoch, &certificate) {
                    marshal.report(Activity::Finalization(finalization));
                }
            }
            None => {}
        }
    }
}

/// The finalization a follower of `epoch` heard, taken only when it names
/// `epoch` and a quorum of the validators seated for `epoch` signed it: the
/// check a seated engine makes on the same certificate.
fn finalization(
    rng: &mut impl rand_core::CryptoRng,
    roster: &Roster,
    epoch: u64,
    certificate: &IoBuf,
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
    let names_the_epoch = finalization.round().epoch().get() == epoch;
    let signed = names_the_epoch && finalization.verify(rng, &verifier, &Sequential);
    signed.then_some(finalization)
}
