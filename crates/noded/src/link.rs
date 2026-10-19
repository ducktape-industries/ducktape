// The p2p mesh's connections: the runtime's own, and those accepted on the listener the node was handed, read and written in pieces so a link dies after a stretch with no bytes moving, never while a long message is still moving.
// `Mesh::start` hands `Linked` to commonware p2p in place of the runtime context, so commonware's one bind takes that listener.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use std::time::{Duration, SystemTime};

use commonware_runtime::signal::Signal;
use commonware_runtime::telemetry::metrics::{Metric, Registered};
use commonware_runtime::{
    BufferPool, BufferPooler, Clock, Error, Handle, IoBufs, Metrics, Name, Network, Resolver,
    SinkOf, Spawner, StreamOf, Supervisor,
};
use futures::future::{Either, select};
use governor::clock::{Clock as GovernorClock, ReasonablyRealtime};
use rand_core::{TryCryptoRng, TryRng};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

const PIECE_BYTES: usize = 64 << 10;

pub struct Linked<E> {
    context: E,
    idle: Duration,
    handed: Handed,
}

/// The bound listener a node hands its mesh, until commonware binds it; every child of a
/// `Linked` shares it.
#[derive(Clone, Default)]
struct Handed(Arc<ArcSwapOption<std::net::TcpListener>>);

impl Handed {
    fn take(&self) -> Option<std::net::TcpListener> {
        self.0.swap(None).and_then(Arc::into_inner)
    }
}

pub struct Listener<L, C> {
    inner: Door<L>,
    clock: C,
    idle: Duration,
}

/// Where a listener accepts: the runtime's own socket, or the one the node was handed.
enum Door<L> {
    Runtime(L),
    Handed(tokio::net::TcpListener),
}

/// One end of a connection: the runtime's own, or one accepted on the handed listener.
pub enum Half<R, T> {
    Runtime(R),
    Handed(T),
}

pub struct Written(OwnedWriteHalf);

pub struct Read(BufReader<OwnedReadHalf>);

pub struct Sink<S, C> {
    inner: S,
    clock: C,
    idle: Duration,
}

pub struct Stream<S, C> {
    inner: S,
    clock: C,
    idle: Duration,
}

impl<E> Linked<E> {
    pub fn new(context: E, idle: Duration) -> Linked<E> {
        Linked {
            context,
            idle,
            handed: Handed::default(),
        }
    }

    /// A `Linked` whose bind takes `listener`, which the caller bound, rather than binding one.
    pub fn handing(context: E, idle: Duration, listener: std::net::TcpListener) -> Linked<E> {
        Linked {
            context,
            idle,
            handed: Handed(Arc::new(ArcSwapOption::from_pointee(listener))),
        }
    }

    fn map(self, f: impl FnOnce(E) -> E) -> Linked<E> {
        Linked {
            context: f(self.context),
            idle: self.idle,
            handed: self.handed,
        }
    }

    fn with(&self, context: E) -> Linked<E> {
        Linked {
            context,
            idle: self.idle,
            handed: self.handed.clone(),
        }
    }
}

async fn moving<T, C: Clock>(
    clock: &C,
    idle: Duration,
    work: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    match select(pin!(work), pin!(clock.sleep(idle))).await {
        Either::Left((moved, _)) => moved,
        Either::Right(((), _)) => Err(Error::Timeout),
    }
}

fn wrapped<S, T, C: Supervisor>(
    (sink, stream): (S, T),
    clock: &C,
    idle: Duration,
) -> (Sink<S, C>, Stream<T, C>) {
    let sink = Sink {
        inner: sink,
        clock: clock.child("sink"),
        idle,
    };
    let stream = Stream {
        inner: stream,
        clock: clock.child("stream"),
        idle,
    };
    (sink, stream)
}

/// The handed listener, which must be bound at `socket`, as tokio's.
fn handed(
    listener: std::net::TcpListener,
    socket: SocketAddr,
) -> Result<tokio::net::TcpListener, Error> {
    let bound = listener.local_addr().map_err(|_| Error::BindFailed)?;
    if bound != socket {
        return Err(Error::BindFailed);
    }
    crate::transport::served(listener).map_err(|_| Error::BindFailed)
}

impl<E: Network + Clock + Supervisor> Network for Linked<E> {
    type Listener = Listener<E::Listener, E>;

    async fn bind(&self, socket: SocketAddr) -> Result<Self::Listener, Error> {
        let inner = match self.handed.take() {
            Some(listener) => Door::Handed(handed(listener, socket)?),
            None => Door::Runtime(self.context.bind(socket).await?),
        };
        Ok(Listener {
            inner,
            clock: self.context.child("listener"),
            idle: self.idle,
        })
    }

    async fn dial(&self, socket: SocketAddr) -> Result<(SinkOf<Self>, StreamOf<Self>), Error> {
        let (sink, stream) = self.context.dial(socket).await?;
        let dialed = (Half::Runtime(sink), Half::Runtime(stream));
        Ok(wrapped(dialed, &self.context, self.idle))
    }
}

/// A connection accepted on the handed listener, set as the node's runtime sets the ones it
/// accepts itself: no delay, no linger. Its reads and writes are bounded by the link's idle limit.
fn accepted(stream: tokio::net::TcpStream) -> (Written, Read) {
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(target: "ducktape::mesh", %error, "an accepted link keeps its delay");
    }
    if let Err(error) = stream.set_zero_linger() {
        tracing::debug!(target: "ducktape::mesh", %error, "an accepted link keeps its linger");
    }
    let (read, write) = stream.into_split();
    (
        Written(write),
        Read(BufReader::with_capacity(PIECE_BYTES, read)),
    )
}

impl<L: commonware_runtime::Listener, C: Clock + Supervisor> commonware_runtime::Listener
    for Listener<L, C>
{
    type Sink = Sink<Half<L::Sink, Written>, C>;
    type Stream = Stream<Half<L::Stream, Read>, C>;

    async fn accept(&mut self) -> Result<(SocketAddr, Self::Sink, Self::Stream), Error> {
        let (address, sink, stream) = match &mut self.inner {
            Door::Runtime(listener) => {
                let (address, sink, stream) = listener.accept().await?;
                (address, Half::Runtime(sink), Half::Runtime(stream))
            }
            Door::Handed(listener) => {
                let (stream, address) = listener.accept().await.map_err(|_| Error::Closed)?;
                let (sink, stream) = accepted(stream);
                (address, Half::Handed(sink), Half::Handed(stream))
            }
        };
        let (sink, stream) = wrapped((sink, stream), &self.clock, self.idle);
        Ok((address, sink, stream))
    }

    fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        match &self.inner {
            Door::Runtime(listener) => listener.local_addr(),
            Door::Handed(listener) => listener.local_addr(),
        }
    }
}

impl<R: commonware_runtime::Sink> commonware_runtime::Sink for Half<R, Written> {
    async fn send(&mut self, bufs: impl Into<IoBufs> + Send) -> Result<(), Error> {
        match self {
            Half::Runtime(sink) => sink.send(bufs).await,
            Half::Handed(Written(sink)) => {
                let mut bufs = bufs.into();
                sink.write_all_buf(&mut bufs)
                    .await
                    .map_err(|_| Error::SendFailed)
            }
        }
    }
}

impl<R: commonware_runtime::Stream> commonware_runtime::Stream for Half<R, Read> {
    async fn recv(&mut self, len: usize) -> Result<IoBufs, Error> {
        match self {
            Half::Runtime(stream) => stream.recv(len).await,
            Half::Handed(Read(stream)) => {
                let mut buf = vec![0; len];
                stream
                    .read_exact(&mut buf)
                    .await
                    .map_err(|_| Error::RecvFailed)?;
                Ok(IoBufs::from(buf))
            }
        }
    }

    fn peek(&self, max_len: usize) -> &[u8] {
        match self {
            Half::Runtime(stream) => stream.peek(max_len),
            Half::Handed(Read(stream)) => {
                let buffered = stream.buffer();
                &buffered[..buffered.len().min(max_len)]
            }
        }
    }
}

impl<S: commonware_runtime::Sink, C: Clock> commonware_runtime::Sink for Sink<S, C> {
    async fn send(&mut self, bufs: impl Into<IoBufs> + Send) -> Result<(), Error> {
        let mut unsent = bufs.into();
        while !unsent.is_empty() {
            let piece = unsent.split_to(unsent.len().min(PIECE_BYTES));
            moving(&self.clock, self.idle, self.inner.send(piece)).await?;
        }
        Ok(())
    }
}

impl<S: commonware_runtime::Stream, C: Clock> commonware_runtime::Stream for Stream<S, C> {
    async fn recv(&mut self, len: usize) -> Result<IoBufs, Error> {
        let mut received = IoBufs::default();
        while received.len() < len {
            let buffered = self.inner.peek(len - received.len()).len();
            let piece = buffered.max(1);
            let bytes = moving(&self.clock, self.idle, self.inner.recv(piece)).await?;
            received.append(bytes.coalesce());
        }
        Ok(received)
    }

    fn peek(&self, max_len: usize) -> &[u8] {
        self.inner.peek(max_len)
    }
}

impl<E: Supervisor> Supervisor for Linked<E> {
    fn name(&self) -> Name {
        self.context.name()
    }

    fn child(&self, label: &'static str) -> Self {
        self.with(self.context.child(label))
    }

    fn with_attribute(self, key: &'static str, value: impl std::fmt::Display) -> Self {
        self.map(|context| context.with_attribute(key, value))
    }
}

impl<E: Spawner> Spawner for Linked<E> {
    fn shared(self, blocking: bool) -> Self {
        self.map(|context| context.shared(blocking))
    }

    fn dedicated(self) -> Self {
        self.map(Spawner::dedicated)
    }

    fn spawn<F, Fut, T>(self, f: F) -> Handle<T>
    where
        F: FnOnce(Self) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (idle, handed) = (self.idle, self.handed.clone());
        self.context.spawn(move |context| {
            f(Linked {
                context,
                idle,
                handed,
            })
        })
    }

    fn stop(
        self,
        value: i32,
        timeout: Option<Duration>,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.context.stop(value, timeout)
    }

    fn stopped(&self) -> Signal {
        self.context.stopped()
    }
}

impl<E: Metrics> Metrics for Linked<E> {
    fn register<N: Into<String>, H: Into<String>, M: Metric>(
        &self,
        name: N,
        help: H,
        metric: M,
    ) -> Registered<M> {
        self.context.register(name, help, metric)
    }

    fn encode(&self) -> String {
        self.context.encode()
    }
}

impl<E: Clock> Clock for Linked<E> {
    fn current(&self) -> SystemTime {
        self.context.current()
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'static {
        self.context.sleep(duration)
    }

    fn sleep_until(&self, deadline: SystemTime) -> impl Future<Output = ()> + Send + 'static {
        self.context.sleep_until(deadline)
    }
}

impl<E: Clock> GovernorClock for Linked<E> {
    type Instant = SystemTime;

    fn now(&self) -> SystemTime {
        self.context.current()
    }
}

impl<E: Clock> ReasonablyRealtime for Linked<E> {}

impl<E: Resolver> Resolver for Linked<E> {
    fn resolve(&self, host: &str) -> impl Future<Output = Result<Vec<IpAddr>, Error>> + Send {
        self.context.resolve(host)
    }
}

impl<E: BufferPooler> BufferPooler for Linked<E> {
    fn network_buffer_pool(&self) -> &BufferPool {
        self.context.network_buffer_pool()
    }

    fn storage_buffer_pool(&self) -> &BufferPool {
        self.context.storage_buffer_pool()
    }
}

impl<E: TryRng> TryRng for Linked<E> {
    type Error = E::Error;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        self.context.try_next_u32()
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        self.context.try_next_u64()
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        self.context.try_fill_bytes(dest)
    }
}

impl<E: TryCryptoRng> TryCryptoRng for Linked<E> {}

#[cfg(test)]
mod tests;
