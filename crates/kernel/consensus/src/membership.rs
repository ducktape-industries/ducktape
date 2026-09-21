use abi::validators::Member;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Blocker, Receiver, Sender};
use commonware_runtime::Handle;
use futures::StreamExt as _;
use futures::channel::mpsc;
use host::Tip;

use crate::anchor::Anchor;
use crate::catchup::{self, Signal};
use crate::chain::Chain;
use crate::engine::{Engine, Epoch, floor};
use crate::lanes::{EngineMux, Lanes};
use crate::marshal::{Marshal, MarshalMailbox};
use crate::roster::{Roster, validators_of};
use crate::{Context, Network};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("epoch {epoch} names a member key no validator scheme decodes")]
    Members { epoch: u64 },
    #[error("epoch {epoch} has no floor: neither an anchor block nor a finalization names the tip")]
    Floor { epoch: u64 },
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
    lanes: Lanes<S, R, B>,
    marshal: MarshalMailbox,
    anchor: Anchor,
    chain: C,
    engine: Option<Engine>,
    seats: mpsc::UnboundedSender<Signal>,
    catch_up: Handle<()>,
}

impl<E, S: Sender, R: Receiver, B, C> Drop for Membership<E, S, R, B, C> {
    fn drop(&mut self) {
        self.catch_up.abort();
    }
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
        marshal: &Marshal,
        chain: C,
    ) -> Membership<E, S, R, B, C> {
        let EngineMux { lanes, heard } = mux;
        let (seats, seated) = mpsc::unbounded();
        let heard = heard
            .into_stream()
            .map(|(epoch, peer)| Signal::Heard { epoch, peer });
        let signals = futures::stream::select(seated, heard);
        let catch_up = context.child("catch_up").spawn({
            let network = network.clone();
            let marshal = marshal.mailbox().clone();
            move |_| catchup::run(network, marshal, signals)
        });
        Membership {
            context,
            partition,
            network,
            roster,
            lanes,
            marshal: marshal.mailbox().clone(),
            anchor: marshal.anchor().clone(),
            chain,
            engine: None,
            seats,
            catch_up,
        }
    }

    pub fn roster(&self) -> &Roster {
        &self.roster
    }

    pub async fn seat(&mut self, tip: Tip, members: &[Member]) -> Result<Standing, Error> {
        let epoch = self.network.epoch_after(tip.height);
        let validators = validators_of(members).ok_or(Error::Members { epoch })?;
        self.roster.seat(epoch, validators);
        let standing = self.engine(epoch, tip).await?;
        let _ = self.seats.unbounded_send(Signal::Seated(epoch));
        Ok(standing)
    }

    async fn engine(&mut self, epoch: u64, tip: Tip) -> Result<Standing, Error> {
        let Some(scheme) = self.roster.scheme(epoch) else {
            self.engine = None;
            return Ok(Standing::Follower);
        };
        let floor = floor(&self.marshal, &self.anchor, &self.network, tip)
            .await
            .ok_or(Error::Floor { epoch })?;
        let lanes = self.lanes.register(epoch).await;
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
