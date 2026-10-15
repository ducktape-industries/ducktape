use commonware_p2p::utils::mux::{Builder as _, MuxHandle, Muxer, SubReceiver, SubSender};
use commonware_p2p::{Channel, Message, Receiver, Sender};
use commonware_runtime::{Handle, Spawner};
use commonware_utils::channel::mpsc;
use futures::stream::{BoxStream, select_all, unfold};
use futures::{Stream, StreamExt as _};

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
    pub(crate) lanes: Lanes<S, R, B>,
    pub(crate) heard: Heard<R::PublicKey>,
}

impl<S: Sender, R: Receiver, B: Clone> EngineMux<S, R, B> {
    pub fn start<E: Spawner>(
        context: E,
        channels: EngineChannels<S, R>,
        blocker: B,
    ) -> EngineMux<S, R, B> {
        let (vote, vote_handle, vote_heard) = mux(context.child("vote"), channels.vote);
        let (certificate, certificate_handle, certificate_heard) =
            mux(context.child("certificate"), channels.certificate);
        let (resolver, resolver_handle, resolver_heard) =
            mux(context.child("resolver"), channels.resolver);
        EngineMux {
            lanes: Lanes {
                vote,
                certificate,
                resolver,
                blocker,
                muxers: [vote_handle, certificate_handle, resolver_handle],
            },
            heard: Heard {
                lanes: [vote_heard, certificate_heard, resolver_heard],
            },
        }
    }
}

pub(crate) struct Lanes<S: Sender, R: Receiver, B> {
    vote: MuxHandle<S, R>,
    certificate: MuxHandle<S, R>,
    resolver: MuxHandle<S, R>,
    blocker: B,
    muxers: [Handle<Result<(), R::Error>>; 3],
}

impl<S: Sender, R: Receiver, B> Drop for Lanes<S, R, B> {
    fn drop(&mut self) {
        for muxer in &self.muxers {
            muxer.abort();
        }
    }
}

impl<S: Sender, R: Receiver, B: Clone> Lanes<S, R, B> {
    pub(crate) async fn register(
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

type Unrouted<P> = mpsc::Receiver<(Channel, Message<P>)>;

pub(crate) struct Heard<P> {
    lanes: [Unrouted<P>; 3],
}

impl<P: Send + 'static> Heard<P> {
    pub(crate) fn into_stream(self) -> impl Stream<Item = (u64, P)> + Send + Unpin {
        select_all(self.lanes.map(unrouted)).map(|(epoch, (peer, _))| (epoch, peer))
    }
}

fn unrouted<P: Send + 'static>(lane: Unrouted<P>) -> BoxStream<'static, (Channel, Message<P>)> {
    unfold(lane, |mut lane| async move {
        let heard = lane.recv().await?;
        Some((heard, lane))
    })
    .boxed()
}

type Demux<S, R> = (
    MuxHandle<S, R>,
    Handle<Result<(), <R as Receiver>::Error>>,
    Unrouted<<R as Receiver>::PublicKey>,
);

fn mux<E: Spawner, S: Sender, R: Receiver>(context: E, (sender, receiver): (S, R)) -> Demux<S, R> {
    let (muxer, handle, heard) = Muxer::builder(context, sender, receiver, MAILBOX)
        .with_backup()
        .build();
    (handle, muxer.start(), heard)
}

async fn subchannel<S: Sender, R: Receiver>(
    mux: &mut MuxHandle<S, R>,
    epoch: u64,
) -> (SubSender<S>, SubReceiver<R>) {
    mux.register(epoch)
        .await
        .expect("an epoch's lanes register once")
}

pub mod channel {
    pub const BROADCAST: u64 = 0;
    pub const BACKFILL: u64 = 1;
    pub const VOTE: u64 = 2;
    pub const CERTIFICATE: u64 = 3;
    pub const RESOLVER: u64 = 4;
    pub const RELAY: u64 = 5;
}

#[cfg(feature = "sim")]
pub use sim::SimMesh;

#[cfg(feature = "sim")]
mod sim {
    use commonware_cryptography::ed25519::PublicKey;
    use commonware_p2p::simulated::{Control, Manager, Oracle, Receiver, Sender};
    use commonware_runtime::{Clock, Quota};

    use super::channel::{BACKFILL, BROADCAST, CERTIFICATE, RESOLVER, VOTE};
    use super::{EngineChannels, MarshalLanes};

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
