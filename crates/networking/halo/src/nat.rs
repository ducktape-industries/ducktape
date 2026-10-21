//! Routers for tests on an internet held in memory. An open router lets in anything; a cone one
//! keeps one public port per host (the host's own when free) and lets in only what comes from a
//! remote the host sent to; a symmetric one maps each remote apart.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use bytes::Bytes;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::{mpsc, watch};

type Arrived = mpsc::UnboundedSender<(SocketAddr, Bytes)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Open,
    Cone,
    Symmetric,
}

#[derive(Clone, Default)]
pub struct Internet {
    ports: Arc<Mutex<HashMap<SocketAddr, Port>>>,
}

#[derive(Clone)]
struct Port {
    filter: Filter,
    arrived: Arrived,
    dropped: Arc<watch::Sender<u64>>,
}

#[derive(Clone)]
enum Filter {
    Everything,
    Nothing,
    Only(SocketAddr),
    SentTo(Arc<Mutex<HashSet<SocketAddr>>>),
}

impl Internet {
    fn ports(&self) -> MutexGuard<'_, HashMap<SocketAddr, Port>> {
        self.ports.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bind(&self, ip: Ipv4Addr, wanted: u16, port: Port) -> io::Result<SocketAddr> {
        let mut ports = self.ports();
        let at = free(&ports, ip, wanted)?;
        ports.insert(at, port);
        Ok(at)
    }

    fn send(&self, from: SocketAddr, to: SocketAddr, packet: Bytes) {
        let Some(port) = self.ports().get(&to).cloned() else {
            return;
        };
        let passes = match &port.filter {
            Filter::Everything => true,
            Filter::Nothing => false,
            Filter::Only(remote) => from == *remote,
            Filter::SentTo(sent) => sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&from),
        };
        if !passes {
            port.dropped.send_modify(|dropped| *dropped += 1);
            return;
        }
        let _ = port.arrived.send((from, packet));
    }
}

fn free(ports: &HashMap<SocketAddr, Port>, ip: Ipv4Addr, wanted: u16) -> io::Result<SocketAddr> {
    let at = |port: u16| SocketAddr::new(ip.into(), port);
    let open = |port: &u16| *port != 0 && !ports.contains_key(&at(*port));
    std::iter::once(wanted)
        .chain(1..=u16::MAX)
        .find(open)
        .map(at)
        .ok_or_else(|| io::ErrorKind::AddrInUse.into())
}

pub struct Router {
    kind: Kind,
    public: Ipv4Addr,
    internet: Internet,
    dropped: Arc<watch::Sender<u64>>,
}

impl Router {
    pub fn new(kind: Kind, public: Ipv4Addr, internet: &Internet) -> Arc<Router> {
        Arc::new(Router {
            kind,
            public,
            internet: internet.clone(),
            dropped: Arc::new(watch::channel(0).0),
        })
    }

    pub fn public(&self) -> IpAddr {
        self.public.into()
    }

    pub fn dropped(&self) -> watch::Receiver<u64> {
        self.dropped.subscribe()
    }

    pub fn free_port(&self) -> io::Result<u16> {
        free(&self.internet.ports(), self.public, 0).map(|at| at.port())
    }

    pub fn behind(self: &Arc<Self>, port: u16) -> io::Result<Arc<Host>> {
        let (arrived, inbound) = mpsc::unbounded_channel();
        let host = Arc::new(Host {
            router: self.clone(),
            inside: SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), port),
            mappings: Mutex::default(),
            sent: Arc::default(),
            arrived,
            inbound: Mutex::new(inbound),
        });
        match self.kind {
            Kind::Open | Kind::Cone => {
                host.mapping(None)?;
            }
            Kind::Symmetric => {
                self.bind(port, Filter::Nothing, host.arrived.clone())?;
            }
        }
        Ok(host)
    }

    fn bind(&self, wanted: u16, filter: Filter, arrived: Arrived) -> io::Result<SocketAddr> {
        let port = Port {
            filter,
            arrived,
            dropped: self.dropped.clone(),
        };
        self.internet.bind(self.public, wanted, port)
    }
}

pub struct Host {
    router: Arc<Router>,
    inside: SocketAddr,
    mappings: Mutex<HashMap<Option<SocketAddr>, SocketAddr>>,
    sent: Arc<Mutex<HashSet<SocketAddr>>>,
    arrived: Arrived,
    inbound: Mutex<mpsc::UnboundedReceiver<(SocketAddr, Bytes)>>,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host")
            .field("inside", &self.inside)
            .finish()
    }
}

impl Host {
    fn mapping(&self, remote: Option<SocketAddr>) -> io::Result<SocketAddr> {
        let mut mappings = self.mappings.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mapped) = mappings.get(&remote) {
            return Ok(*mapped);
        }
        let (wanted, filter) = match remote {
            None if self.router.kind == Kind::Open => (self.inside.port(), Filter::Everything),
            None => (self.inside.port(), Filter::SentTo(self.sent.clone())),
            Some(remote) => (0, Filter::Only(remote)),
        };
        let mapped = self.router.bind(wanted, filter, self.arrived.clone())?;
        mappings.insert(remote, mapped);
        Ok(mapped)
    }
}

#[derive(Debug)]
struct Ready;

impl UdpPoller for Ready {
    fn poll_writable(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for Host {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Ready)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let to = transmit.destination;
        let remote = match self.router.kind {
            Kind::Open | Kind::Cone => None,
            Kind::Symmetric => Some(to),
        };
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(to);
        let mapped = self.mapping(remote)?;
        let packet = Bytes::copy_from_slice(transmit.contents);
        self.router.internet.send(mapped, to, packet);
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut inbound = self.inbound.lock().unwrap_or_else(PoisonError::into_inner);
        let Poll::Ready(arrived) = inbound.poll_recv(cx) else {
            return Poll::Pending;
        };
        let Some((from, packet)) = arrived else {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        };
        let (Some(buf), Some(slot)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Ok(0));
        };
        let len = packet.len().min(buf.len());
        buf[..len].copy_from_slice(&packet[..len]);
        *slot = RecvMeta {
            addr: from,
            len,
            stride: len,
            ecn: None,
            dst_ip: None,
        };
        Poll::Ready(Ok(1))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.inside)
    }

    fn may_fragment(&self) -> bool {
        false
    }
}
