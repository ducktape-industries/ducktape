//! The real overlay-socket transport arm: the plane's medium on a live node.
//!
//! Twin of [`sim`](crate::sim) — where the sim arm is a deterministic
//! in-memory network for the isolation proofs, this arm binds real sockets
//! on the reachability plane's WireGuard overlay, minted by an injected
//! [`SocketFactory`] (the overlay-net socket seam; [`OsSocketFactory`]
//! is today's only arm — plain tokio sockets the kernel routes through the
//! TUN interface):
//! - **datagrams** = one [`DatagramSocket`] on the node's overlay `/128`,
//! - **streams** = a [`StreamListener`] on that same `/128`, dialled with
//!   the source bound to it.
//!
//! Identity is the transport's, exactly as [`crate::transport`] promises: a
//! packet's source `/128` is bound by WireGuard cryptokey routing to exactly
//! one peer, so an inbound datagram or accepted stream is authenticated by its
//! source address alone — no plane-level handshake. This arm turns a source
//! address back into a [`PeerId`] through an injected [`AddressBook`]; an
//! unresolvable source is dropped (never buffered, never admitted).
//!
//! The crate stays free of any cryptography / WireGuard dependency: the
//! `PeerId ↔ overlay-address` mapping lives entirely behind [`AddressBook`],
//! which the node layer supplies (`ula_v6_member_addr` over its known member
//! set). This arm only ever sees raw [`PeerId`]s and [`SocketAddr`]s.
//!
//! MUST bind specifically to the overlay `/128`, never a wildcard: binding
//! wildcard would accept traffic on addresses the identity invariant does not
//! cover. The constructor takes explicit bind addresses; the node passes the
//! `/128`.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpSocket, UdpSocket};

use crate::transport::{DataPlaneTransport, PeerId, TransportError};
use crate::wire::MAX_DATAGRAM;

// ── The socket seam ─────────────────────────────────────
//
// where the plane's sockets come from is INJECTED (the overlay-net
// socket-factory seam): the
// node passes a factory, and this arm never names an OS socket type in its
// own signatures. today's only factory is [`OsSocketFactory`] (the TUN
// backend: the kernel routes overlay ULAs through the WireGuard interface,
// so plain OS sockets carry them); the userspace backend supplies a factory
// whose sockets terminate in an in-process stack instead — with no change
// here or in any consumer.
//
// object-safe by boxed futures (not RPITIT) on purpose: the factory crosses
// the node boundary as `Arc<dyn SocketFactory>` exactly like [`AddressBook`],
// and a per-call vtable hop is noise next to the syscall (or virtual-stack
// poll) behind it.

/// a boxed future — the object-safe shape of this seam's async methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// the duplex byte stream every factory yields: exactly the bounds
/// [`DataPlaneTransport::Stream`] demands, boxed.
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// the plane's stream type under the seam.
pub type PlaneStream = Box<dyn Duplex>;

/// an unconnected datagram socket bound to the node's overlay `/128`.
pub trait DatagramSocket: Send + Sync {
    fn send_to<'a>(&'a self, buf: &'a [u8], dest: SocketAddr) -> BoxFuture<'a, io::Result<usize>>;
    fn recv_from<'a>(&'a self, buf: &'a mut [u8])
    -> BoxFuture<'a, io::Result<(usize, SocketAddr)>>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

/// a stream acceptor bound to the node's overlay `/128`.
pub trait StreamListener: Send + Sync {
    fn accept(&self) -> BoxFuture<'_, io::Result<(PlaneStream, SocketAddr)>>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

/// mints the plane's sockets. `dial_from` binds the stream's source to
/// `local_ip` — the far side authenticates the connection by that source
/// `/128`, so the factory owns source binding, not the caller.
pub trait SocketFactory: Send + Sync {
    fn bind_udp(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<Box<dyn DatagramSocket>>>;
    fn bind_listener(&self, addr: SocketAddr)
    -> BoxFuture<'_, io::Result<Box<dyn StreamListener>>>;
    fn dial_from<'a>(
        &'a self,
        local_ip: IpAddr,
        dest: SocketAddr,
    ) -> BoxFuture<'a, io::Result<PlaneStream>>;
}

/// the OS arm: plain tokio sockets (under TUN mode the kernel makes these
/// overlay-capable; see the seam comment above).
pub struct OsSocketFactory;

impl DatagramSocket for UdpSocket {
    fn send_to<'a>(&'a self, buf: &'a [u8], dest: SocketAddr) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(UdpSocket::send_to(self, buf, dest))
    }

    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, io::Result<(usize, SocketAddr)>> {
        Box::pin(UdpSocket::recv_from(self, buf))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        UdpSocket::local_addr(self)
    }
}

/// The stream class carries whole application messages that the sender has
/// already decided to send — a call's media frames cross a node boundary on
/// [`Service::Gateway`](crate::Service::Gateway), paced at one frame per
/// 20 ms. Nagle's algorithm has nothing to coalesce there, but it still holds
/// each frame until the previous one is acknowledged, which costs a full round
/// trip per frame and delivers them in clumps a receiver's jitter buffer
/// cannot absorb. Measured on this crate's `overlay_impairment` harness at
/// 25 ms one-way and zero loss: median delay 51.6 ms and 449 ms of playout
/// starvation per 20 s with Nagle, 25.1 ms and zero without it — the datagram
/// class's own numbers on the same link.
///
/// Where a stream really is bulk ([`Service::ModuleCode`](crate::Service::ModuleCode),
/// [`Service::StateSync`](crate::Service::StateSync)) the writes are large, so
/// Nagle never engages and disabling it changes nothing. The factory is
/// therefore the right place for it: it is a property of the transport, not of
/// any one service riding it.
fn disable_nagle(stream: &tokio::net::TcpStream) {
    if let Err(error) = stream.set_nodelay(true) {
        tracing::warn!(
            target: "ducktape::dataplane",
            reason = "nodelay_refused",
            %error,
            "overlay stream kept Nagle batching"
        );
    }
}

impl StreamListener for TcpListener {
    fn accept(&self) -> BoxFuture<'_, io::Result<(PlaneStream, SocketAddr)>> {
        Box::pin(async {
            let (stream, addr) = TcpListener::accept(self).await?;
            disable_nagle(&stream);
            Ok((Box::new(stream) as PlaneStream, addr))
        })
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        TcpListener::local_addr(self)
    }
}

impl SocketFactory for OsSocketFactory {
    fn bind_udp(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<Box<dyn DatagramSocket>>> {
        Box::pin(
            async move { Ok(Box::new(UdpSocket::bind(addr).await?) as Box<dyn DatagramSocket>) },
        )
    }

    fn bind_listener(
        &self,
        addr: SocketAddr,
    ) -> BoxFuture<'_, io::Result<Box<dyn StreamListener>>> {
        Box::pin(
            async move { Ok(Box::new(TcpListener::bind(addr).await?) as Box<dyn StreamListener>) },
        )
    }

    fn dial_from<'a>(
        &'a self,
        local_ip: IpAddr,
        dest: SocketAddr,
    ) -> BoxFuture<'a, io::Result<PlaneStream>> {
        Box::pin(async move {
            let socket = match local_ip {
                IpAddr::V4(_) => TcpSocket::new_v4()?,
                IpAddr::V6(_) => TcpSocket::new_v6()?,
            };
            socket.bind(SocketAddr::new(local_ip, 0))?;
            let stream = socket.connect(dest).await?;
            disable_nagle(&stream);
            Ok(Box::new(stream) as PlaneStream)
        })
    }
}

/// The node-supplied `PeerId ↔ overlay-address` mapping. Forward resolution
/// (`*_addr`) returns the FULL socket address per class — the peer's `/128`
/// plus the class port — so this crate needs no port policy of its own.
/// Reverse resolution ([`peer_at`](AddressBook::peer_at)) is by source IP
/// only: under cryptokey routing the source `/128` is the identity and the
/// source port carries no identity (a stream's source port is ephemeral).
///
/// The node builds this over its finalized member set; a source `/128` with
/// no member maps to `None`, which this arm drops.
pub trait AddressBook: Send + Sync + 'static {
    /// Where to send `peer`'s datagrams: its overlay `/128` + the datagram
    /// port. `None` if `peer` is not a reachable member.
    fn datagram_addr(&self, peer: PeerId) -> Option<SocketAddr>;

    /// Where to dial `peer`'s streams: its overlay `/128` + the listener
    /// port. `None` if `peer` is not a reachable member.
    fn stream_addr(&self, peer: PeerId) -> Option<SocketAddr>;

    /// The authenticated peer whose overlay source address is `src`. `None`
    /// for an unknown source — the arm drops it, so it never reaches admission
    /// or a consumer queue.
    fn peer_at(&self, src: IpAddr) -> Option<PeerId>;
}

/// The real transport: overlay UDP + TCP bound to the node's `/128`.
pub struct OverlaySockets {
    udp: Arc<dyn DatagramSocket>,
    listener: Box<dyn StreamListener>,
    /// The overlay `/128` this node presents as its source. Dialled streams
    /// bind their source here so the far side authenticates us by it.
    local_ip: IpAddr,
    addresses: Arc<dyn AddressBook>,
    /// Retained for per-connect dials (see [`SocketFactory::dial_from`]).
    factory: Arc<dyn SocketFactory>,
    /// How many arrivals this socket has dropped for an unrecognised source
    /// — see [`OverlaySockets::note_unknown_source`].
    unknown_sources: std::sync::atomic::AtomicU64,
}

impl OverlaySockets {
    /// Bind on plain OS sockets — [`bind_with`](OverlaySockets::bind_with)
    /// over [`OsSocketFactory`], the arm every current caller means.
    pub async fn bind(
        datagram_bind: SocketAddr,
        stream_bind: SocketAddr,
        addresses: Arc<dyn AddressBook>,
    ) -> io::Result<Self> {
        Self::bind_with(
            Arc::new(OsSocketFactory),
            datagram_bind,
            stream_bind,
            addresses,
        )
        .await
    }

    /// Bind the datagram and stream sockets to the given overlay addresses and
    /// wire in the address book. `datagram_bind` and `stream_bind` MUST carry
    /// the same overlay `/128` (this node's) — a port of `0` lets the OS pick,
    /// which is how the tests stand two endpoints on one loopback IP. the
    /// factory owns what a "socket" is — see the socket-seam comment atop
    /// this module.
    pub async fn bind_with(
        factory: Arc<dyn SocketFactory>,
        datagram_bind: SocketAddr,
        stream_bind: SocketAddr,
        addresses: Arc<dyn AddressBook>,
    ) -> io::Result<Self> {
        if datagram_bind.ip() != stream_bind.ip() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram and stream sockets must share the node's overlay /128",
            ));
        }
        let udp = factory.bind_udp(datagram_bind).await?;
        let listener = factory.bind_listener(stream_bind).await?;
        Ok(OverlaySockets {
            udp: udp.into(),
            listener,
            local_ip: datagram_bind.ip(),
            addresses,
            factory,
            unknown_sources: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// An arrival from a `/128` the address book does not know. It is dropped
    /// — that is the authentication — but it used to be dropped in COMPLETE
    /// silence, and that silence is a failure mode of its own: a peer whose
    /// key the tracked set does not carry (a member the view has not caught up
    /// on, a resident dropped at a cutover) reaches our port, is discarded, and
    /// every layer above reports health. A huddle where our frames leave and
    /// theirs never arrive looks exactly like this and says nothing.
    ///
    /// Rate-limited hard: media arrives tens of times a second and an
    /// unconditional line here would evict the ring it belongs in. First
    /// arrival, then every 1000th, carrying the count — the counter IS the
    /// diagnosis.
    fn note_unknown_source(&self, src: SocketAddr) {
        use std::sync::atomic::Ordering;

        let dropped = self.unknown_sources.fetch_add(1, Ordering::Relaxed) + 1;
        if dropped == 1 || dropped.is_multiple_of(1000) {
            tracing::warn!(
                target: "ducktape::dataplane",
                %src,
                dropped,
                reason = "unknown_source",
                "dropped an arrival from a /128 this node does not track — it \
                 authenticates to no peer, so nothing above will ever see it"
            );
        }
    }

    /// The actually-bound datagram address (resolves an OS-assigned port).
    pub fn local_datagram_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    /// The actually-bound stream (listener) address.
    pub fn local_stream_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Dial `dest` with the source bound to this node's overlay `/128`, so the
    /// acceptor authenticates the connection by our source address.
    async fn dial(&self, dest: SocketAddr) -> io::Result<PlaneStream> {
        self.factory.dial_from(self.local_ip, dest).await
    }
}

impl DataPlaneTransport for OverlaySockets {
    type Stream = PlaneStream;

    async fn send_datagram(&self, to: PeerId, frame: Vec<u8>) -> Result<(), TransportError> {
        let dest = self
            .addresses
            .datagram_addr(to)
            .ok_or(TransportError::Unreachable(to))?;
        // Fire-and-forget: one datagram, one syscall. A short write or a
        // transient send error is the medium losing the frame — the datagram
        // contract permits loss, so we surface only a hard socket failure.
        self.udp.send_to(&frame, dest).await?;
        Ok(())
    }

    async fn recv_datagram(&self) -> Result<(PeerId, Vec<u8>), TransportError> {
        let mut buf = [0u8; MAX_DATAGRAM];
        loop {
            let (n, src) = self.udp.recv_from(&mut buf).await?;
            // Source /128 is the identity. An unknown source is not ours to
            // deliver — drop it and keep receiving (the single demux caller
            // must still make progress).
            match self.addresses.peer_at(src.ip()) {
                Some(peer) => return Ok((peer, buf[..n].to_vec())),
                None => self.note_unknown_source(src),
            }
        }
    }

    async fn connect(&self, to: PeerId) -> Result<Self::Stream, TransportError> {
        let dest = self
            .addresses
            .stream_addr(to)
            .ok_or(TransportError::Unreachable(to))?;
        Ok(self.dial(dest).await?)
    }

    async fn accept(&self) -> Result<(PeerId, Self::Stream), TransportError> {
        loop {
            let (stream, src) = self.listener.accept().await?;
            // Same authentication as datagrams: the source /128 names the
            // opener. An unknown source is dropped (the stream closes on
            // drop); we never hand an unauthenticated stream to the plane.
            match self.addresses.peer_at(src.ip()) {
                Some(peer) => return Ok((peer, stream)),
                None => self.note_unknown_source(src),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, Ipv6Addr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The option [`disable_nagle`] exists to set, read back off both ends of a
    /// real connection. The seam boxes every stream as a `dyn Duplex` before a
    /// caller sees it, so this is the last point where the concrete socket is
    /// in hand — and "verified by inspection" is not verification. That the two
    /// call sites (`accept` and `dial_from`) actually reach here is what the
    /// `overlay_impairment` harness measures: with the option the stream class
    /// delivers at the path delay, without it a full round trip later.
    #[tokio::test]
    async fn disable_nagle_clears_the_option_on_a_real_socket() {
        let bound = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dest = bound.local_addr().unwrap();
        let dialing = tokio::spawn(async move { tokio::net::TcpStream::connect(dest).await });

        let (accepted, _) = bound.accept().await.unwrap();
        let dialed = dialing.await.unwrap().expect("dial succeeds");

        for (side, stream) in [("accepted", &accepted), ("dialled", &dialed)] {
            assert!(
                !stream.nodelay().unwrap(),
                "{side}: a fresh socket is expected to start with Nagle on, or this proves nothing"
            );
            disable_nagle(stream);
            assert!(
                stream.nodelay().unwrap(),
                "{side}: overlay stream must not batch frames behind Nagle"
            );
        }
    }

    /// A test address book for two endpoints that share the loopback IP but
    /// bind distinct OS-assigned ports. Forward resolution carries the full
    /// per-peer address; reverse resolution is unambiguous because each node
    /// has exactly one peer (production uses distinct `/128`s — proven by the
    /// privileged-Linux smoke, not here).
    struct TwoNodeBook {
        forward: HashMap<PeerId, (SocketAddr, SocketAddr)>, // peer -> (datagram, stream)
        one_peer: PeerId,
    }

    impl AddressBook for TwoNodeBook {
        fn datagram_addr(&self, peer: PeerId) -> Option<SocketAddr> {
            self.forward.get(&peer).map(|(d, _)| *d)
        }
        fn stream_addr(&self, peer: PeerId) -> Option<SocketAddr> {
            self.forward.get(&peer).map(|(_, s)| *s)
        }
        fn peer_at(&self, _src: IpAddr) -> Option<PeerId> {
            Some(self.one_peer)
        }
    }

    fn lo() -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
    }

    async fn book_for(peer: PeerId, dgram: SocketAddr, stream: SocketAddr) -> Arc<dyn AddressBook> {
        let mut forward = HashMap::new();
        forward.insert(peer, (dgram, stream));
        Arc::new(TwoNodeBook {
            forward,
            one_peer: peer,
        })
    }

    #[tokio::test]
    async fn datagram_and_stream_round_trip_over_real_sockets() {
        let (a_id, b_id) = (PeerId([1; 32]), PeerId([2; 32]));

        // Bind both endpoints first with placeholder books, learn their
        // OS-assigned ports, then rebind the books to point at each other.
        let a = OverlaySockets::bind(lo(), lo(), book_for(b_id, lo(), lo()).await)
            .await
            .expect("bind a");
        let b = OverlaySockets::bind(lo(), lo(), book_for(a_id, lo(), lo()).await)
            .await
            .expect("bind b");

        let (a_dgram, a_stream) = (
            a.local_datagram_addr().unwrap(),
            a.local_stream_addr().unwrap(),
        );
        let (b_dgram, b_stream) = (
            b.local_datagram_addr().unwrap(),
            b.local_stream_addr().unwrap(),
        );

        // Rebuild with real peer addresses now that ports are known.
        let a = OverlaySockets {
            addresses: book_for(b_id, b_dgram, b_stream).await,
            ..a
        };
        let b = OverlaySockets {
            addresses: book_for(a_id, a_dgram, a_stream).await,
            ..b
        };

        // datagram: a -> b, authenticated as a.
        a.send_datagram(b_id, b"opus frame".to_vec())
            .await
            .expect("send datagram");
        let (from, bytes) = b.recv_datagram().await.expect("recv datagram");
        assert_eq!(from, a_id, "inbound datagram authenticated by source /128");
        assert_eq!(bytes, b"opus frame");

        // stream: a dials b, both directions carry bytes, and b sees a.
        let dialed = tokio::spawn(async move {
            let mut s = a.connect(b_id).await.expect("connect");
            s.write_all(b"ping").await.unwrap();
            s.flush().await.unwrap();
            let mut echo = [0u8; 4];
            s.read_exact(&mut echo).await.unwrap();
            echo
        });
        let (opener, mut server_stream) = b.accept().await.expect("accept");
        assert_eq!(opener, a_id, "accepted stream authenticated by source /128");
        let mut req = [0u8; 4];
        server_stream.read_exact(&mut req).await.unwrap();
        assert_eq!(&req, b"ping");
        server_stream.write_all(&req).await.unwrap();
        server_stream.flush().await.unwrap();
        assert_eq!(&dialed.await.unwrap(), b"ping");
    }
}
