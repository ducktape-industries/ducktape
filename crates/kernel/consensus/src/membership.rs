use abi::validators::Member;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Blocker, Receiver, Sender};

use crate::chain::Chain;
use crate::engine::{Engine, Epoch, floor};
use crate::lanes::EngineMux;
use crate::marshal::MarshalMailbox;
use crate::roster::{Roster, validators_of};
use crate::{Context, Network};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("epoch {epoch} names a member key no validator scheme decodes")]
    Members { epoch: u64 },
    #[error("epoch {epoch} has no anchor block to start from")]
    Anchor { epoch: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    Validator,
    Follower,
}

pub struct Membership<E, S: Sender, R: Receiver, B, C> {
    context: E,
    partition: String,
    network: Network,
    roster: Roster,
    mux: EngineMux<S, R, B>,
    marshal: MarshalMailbox,
    chain: C,
    engine: Option<Engine>,
}

impl<E, S, R, B, C> Membership<E, S, R, B, C>
where
    E: Context,
    S: Sender<PublicKey = PublicKey>,
    R: Receiver<PublicKey = PublicKey>,
    B: Blocker<PublicKey = PublicKey>,
    C: Chain,
{
    pub fn new(
        context: E,
        partition: String,
        network: Network,
        roster: Roster,
        mux: EngineMux<S, R, B>,
        marshal: MarshalMailbox,
        chain: C,
    ) -> Membership<E, S, R, B, C> {
        Membership {
            context,
            partition,
            network,
            roster,
            mux,
            marshal,
            chain,
            engine: None,
        }
    }

    pub fn roster(&self) -> &Roster {
        &self.roster
    }

    pub async fn seat(&mut self, epoch: u64, members: &[Member]) -> Result<Standing, Error> {
        let validators = validators_of(members).ok_or(Error::Members { epoch })?;
        self.roster.seat(epoch, validators);
        let Some(scheme) = self.roster.scheme(epoch) else {
            self.engine = None;
            return Ok(Standing::Follower);
        };
        let floor = floor(&self.marshal, self.network.epoch_length, epoch)
            .await
            .ok_or(Error::Anchor { epoch })?;
        let lanes = self.mux.lanes(epoch).await;
        self.engine = None;
        self.engine = Some(Engine::start(
            self.context.child("engine").with_attribute("epoch", epoch),
            &self.partition,
            &self.network,
            Epoch {
                number: epoch,
                scheme,
                floor,
            },
            lanes,
            &self.marshal,
            self.chain.clone(),
        ));
        Ok(Standing::Validator)
    }
}
