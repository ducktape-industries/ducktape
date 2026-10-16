use commonware_consensus::types::Height;
use commonware_cryptography::ed25519::PublicKey;
use commonware_utils::vec::NonEmptyVec;
use futures::{Stream, StreamExt as _};

use crate::Network;
use crate::marshal::MarshalMailbox;

pub(crate) enum Signal {
    Seated(u64),
    Heard { epoch: u64, peer: PublicKey },
}

pub(crate) struct Hint {
    pub height: u64,
    pub peer: PublicKey,
}

pub(crate) struct CatchUp {
    network: Network,
    seated: Option<u64>,
}

impl CatchUp {
    pub(crate) fn new(network: Network) -> CatchUp {
        CatchUp {
            network,
            seated: None,
        }
    }

    pub(crate) fn step(&mut self, signal: Signal) -> Option<Hint> {
        match signal {
            Signal::Seated(epoch) => self.on_seated(epoch),
            Signal::Heard { epoch, peer } => self.on_heard(epoch, peer),
        }
    }

    fn on_seated(&mut self, epoch: u64) -> Option<Hint> {
        self.seated = Some(epoch);
        None
    }

    fn on_heard(&self, epoch: u64, peer: PublicKey) -> Option<Hint> {
        let seated = self.seated?;
        let network_is_ahead = epoch > seated;
        network_is_ahead.then(|| Hint {
            height: self.network.anchor(seated + 1),
            peer,
        })
    }
}

pub(crate) async fn run(
    network: Network,
    marshal: MarshalMailbox,
    mut signals: impl Stream<Item = Signal> + Unpin,
) {
    let mut catch_up = CatchUp::new(network);
    while let Some(signal) = signals.next().await {
        let Some(hint) = catch_up.step(signal) else {
            continue;
        };
        marshal.hint_finalized(Height::new(hint.height), NonEmptyVec::new(hint.peer));
    }
}
