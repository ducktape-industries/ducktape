// The p2p mesh's connections: the runtime's own, read and written in pieces so a link dies after a stretch with no bytes moving, never while a long message is still moving.
// `Mesh::start` hands `Linked` to commonware p2p in place of the runtime context.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
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

const PIECE_BYTES: usize = 64 << 10;

pub struct Linked<E> {
    context: E,
    idle: Duration,
}

pub struct Listener<L, C> {
    inner: L,
    clock: C,
    idle: Duration,
}

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
        Linked { context, idle }
    }

    fn map(self, f: impl FnOnce(E) -> E) -> Linked<E> {
        Linked {
            context: f(self.context),
            idle: self.idle,
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

impl<E: Network + Clock + Supervisor> Network for Linked<E> {
    type Listener = Listener<E::Listener, E>;

    async fn bind(&self, socket: SocketAddr) -> Result<Self::Listener, Error> {
        let inner = self.context.bind(socket).await?;
        Ok(Listener {
            inner,
            clock: self.context.child("listener"),
            idle: self.idle,
        })
    }

    async fn dial(&self, socket: SocketAddr) -> Result<(SinkOf<Self>, StreamOf<Self>), Error> {
        let dialed = self.context.dial(socket).await?;
        Ok(wrapped(dialed, &self.context, self.idle))
    }
}

impl<L: commonware_runtime::Listener, C: Clock + Supervisor> commonware_runtime::Listener
    for Listener<L, C>
{
    type Sink = Sink<L::Sink, C>;
    type Stream = Stream<L::Stream, C>;

    async fn accept(&mut self) -> Result<(SocketAddr, Self::Sink, Self::Stream), Error> {
        let (address, sink, stream) = self.inner.accept().await?;
        let (sink, stream) = wrapped((sink, stream), &self.clock, self.idle);
        Ok((address, sink, stream))
    }

    fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.inner.local_addr()
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
        Linked::new(self.context.child(label), self.idle)
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
        let idle = self.idle;
        self.context
            .spawn(move |context| f(Linked::new(context, idle)))
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
