use commonware_p2p::utils::mux::{MuxHandle, Muxer, SubReceiver, SubSender};
use commonware_p2p::{Receiver, Sender};
use commonware_runtime::{Handle, Spawner};

const MAILBOX: usize = 1024;

pub struct MarshalLanes<S, R> {
    pub broadcast: (S, R),
    pub backfill: (S, R),
}

pub struct EngineChannels<S, R> {
    pub vote: (S, R),
    pub certificate: (S, R),
    pub resolver: (S, R),
}

pub(crate) struct EngineLanes<S, R, B> {
    pub vote: (S, R),
    pub certificate: (S, R),
    pub resolver: (S, R),
    pub blocker: B,
}

pub struct EngineMux<S: Sender, R: Receiver, B> {
    vote: MuxHandle<S, R>,
    certificate: MuxHandle<S, R>,
    resolver: MuxHandle<S, R>,
    blocker: B,
    muxers: [Handle<Result<(), R::Error>>; 3],
}

impl<S: Sender, R: Receiver, B> Drop for EngineMux<S, R, B> {
    fn drop(&mut self) {
        for muxer in &self.muxers {
            muxer.abort();
        }
    }
}

impl<S: Sender, R: Receiver, B: Clone> EngineMux<S, R, B> {
    pub fn start<E: Spawner>(
        context: E,
        channels: EngineChannels<S, R>,
        blocker: B,
    ) -> EngineMux<S, R, B> {
        let (vote, vote_handle) = mux(context.child("vote"), channels.vote);
        let (certificate, certificate_handle) =
            mux(context.child("certificate"), channels.certificate);
        let (resolver, resolver_handle) = mux(context.child("resolver"), channels.resolver);
        EngineMux {
            vote,
            certificate,
            resolver,
            blocker,
            muxers: [vote_handle, certificate_handle, resolver_handle],
        }
    }

    pub(crate) async fn lanes(
        &mut self,
        epoch: u64,
    ) -> EngineLanes<SubSender<S>, SubReceiver<R>, B> {
        EngineLanes {
            vote: subchannel(&mut self.vote, epoch).await,
            certificate: subchannel(&mut self.certificate, epoch).await,
            resolver: subchannel(&mut self.resolver, epoch).await,
            blocker: self.blocker.clone(),
        }
    }
}

type Demux<S, R> = (MuxHandle<S, R>, Handle<Result<(), <R as Receiver>::Error>>);

fn mux<E: Spawner, S: Sender, R: Receiver>(context: E, (sender, receiver): (S, R)) -> Demux<S, R> {
    let (muxer, handle) = Muxer::new(context, sender, receiver, MAILBOX);
    (handle, muxer.start())
}

async fn subchannel<S: Sender, R: Receiver>(
    mux: &mut MuxHandle<S, R>,
    epoch: u64,
) -> (SubSender<S>, SubReceiver<R>) {
    mux.register(epoch)
        .await
        .expect("an epoch's lanes register once")
}

#[cfg(feature = "sim")]
pub use sim::SimMesh;

#[cfg(feature = "sim")]
mod sim {
    use commonware_cryptography::ed25519::PublicKey;
    use commonware_p2p::simulated::{Control, Manager, Oracle, Receiver, Sender};
    use commonware_runtime::{Clock, Quota};

    use super::{EngineChannels, MarshalLanes};

    const BROADCAST: u64 = 0;
    const BACKFILL: u64 = 1;
    const VOTE: u64 = 2;
    const CERTIFICATE: u64 = 3;
    const RESOLVER: u64 = 4;

    pub struct SimMesh<E: Clock> {
        oracle: Oracle<PublicKey, E>,
        me: PublicKey,
        quota: Quota,
    }

    impl<E: Clock> SimMesh<E> {
        pub fn new(oracle: Oracle<PublicKey, E>, me: PublicKey, quota: Quota) -> SimMesh<E> {
            SimMesh { oracle, me, quota }
        }

        pub fn me(&self) -> &PublicKey {
            &self.me
        }

        pub fn provider(&self) -> Manager<PublicKey, E> {
            self.oracle.manager()
        }

        pub fn blocker(&self) -> Control<PublicKey, E> {
            self.oracle.control(self.me.clone())
        }

        pub async fn marshal_lanes(
            &self,
        ) -> MarshalLanes<Sender<PublicKey, E>, Receiver<PublicKey>> {
            MarshalLanes {
                broadcast: self.register(BROADCAST).await,
                backfill: self.register(BACKFILL).await,
            }
        }

        pub async fn engine_channels(
            &self,
        ) -> EngineChannels<Sender<PublicKey, E>, Receiver<PublicKey>> {
            EngineChannels {
                vote: self.register(VOTE).await,
                certificate: self.register(CERTIFICATE).await,
                resolver: self.register(RESOLVER).await,
            }
        }

        async fn register(&self, channel: u64) -> (Sender<PublicKey, E>, Receiver<PublicKey>) {
            self.oracle
                .control(self.me.clone())
                .register(channel, self.quota)
                .await
                .expect("a sim channel registers once")
        }
    }
}
