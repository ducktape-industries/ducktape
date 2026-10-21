//! The relayed path: halo's own QUIC carried as packets in a pipe of messages (a WebSocket on a
//! member's HTTPS address), one quinn endpoint per pipe, so a member that takes no UDP from another
//! is reached with the same handshake, admission and lanes.

use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Sink, SinkExt as _, Stream, StreamExt as _};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::{mpsc, watch};

pub type Outbound = Pin<Box<dyn Sink<Bytes, Error = io::Error> + Send>>;
pub type Inbound = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

pub struct Pipe {
    pub send: Outbound,
    pub recv: Inbound,
}

#[cfg(feature = "sim")]
impl Pipe {
    pub fn pair() -> (Pipe, Pipe) {
        let (to_far, from_near) = futures::channel::mpsc::unbounded::<Bytes>();
        let (to_near, from_far) = futures::channel::mpsc::unbounded::<Bytes>();
        let near = Pipe {
            send: Box::pin(to_far.sink_map_err(io::Error::other)),
            recv: Box::pin(from_far.map(Ok::<Bytes, io::Error>)),
        };
        let far = Pipe {
            send: Box::pin(to_near.sink_map_err(io::Error::other)),
            recv: Box::pin(from_near.map(Ok::<Bytes, io::Error>)),
        };
        (near, far)
    }
}

pub(crate) const HERE: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1));
pub(crate) const THERE: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 1));

pub(crate) struct Session {
    out: mpsc::Sender<Bytes>,
    inbound: Mutex<mpsc::Receiver<Bytes>>,
    ended: watch::Receiver<bool>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Session")
    }
}

impl Session {
    pub(crate) fn over(pipe: Pipe) -> Arc<Session> {
        let (out, outgoing) = mpsc::channel(1);
        let (incoming, inbound) = mpsc::channel(1);
        let (end, ended) = watch::channel(false);
        tokio::spawn(async move {
            pump(pipe, outgoing, incoming).await;
            end.send_replace(true);
        });
        Arc::new(Session {
            out,
            inbound: Mutex::new(inbound),
            ended,
        })
    }

    pub(crate) async fn ended(&self) {
        let mut ended = self.ended.clone();
        let _ = ended.wait_for(|ended| *ended).await;
    }
}

async fn pump(pipe: Pipe, mut outgoing: mpsc::Receiver<Bytes>, incoming: mpsc::Sender<Bytes>) {
    let Pipe { mut send, mut recv } = pipe;
    let sending = async {
        while let Some(packet) = outgoing.recv().await {
            if send.feed(packet).await.is_err() {
                return;
            }
            while let Ok(more) = outgoing.try_recv() {
                if send.feed(more).await.is_err() {
                    return;
                }
            }
            if send.flush().await.is_err() {
                return;
            }
        }
        let _ = send.close().await;
    };
    let receiving = async {
        while let Some(Ok(packet)) = recv.next().await {
            if incoming.send(packet).await.is_err() {
                return;
            }
        }
    };
    futures::pin_mut!(sending, receiving);
    futures::future::select(sending, receiving).await;
}

type Reserving = Pin<
    Box<dyn Future<Output = Result<mpsc::OwnedPermit<Bytes>, mpsc::error::SendError<()>>> + Send>,
>;

struct Writable {
    out: mpsc::Sender<Bytes>,
    waiting: Mutex<Option<Reserving>>,
}

impl fmt::Debug for Writable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Writable")
    }
}

impl UdpPoller for Writable {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let waiting = this
            .waiting
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let out = this.out.clone();
        let reserving = waiting.get_or_insert_with(|| Box::pin(out.reserve_owned()));
        let Poll::Ready(reserved) = reserving.as_mut().poll(cx) else {
            return Poll::Pending;
        };
        *waiting = None;
        match reserved {
            Ok(_room) => Poll::Ready(Ok(())),
            Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
}

impl AsyncUdpSocket for Session {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Writable {
            out: self.out.clone(),
            waiting: Mutex::new(None),
        })
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        match self.out.try_send(Bytes::copy_from_slice(transmit.contents)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(io::ErrorKind::WouldBlock.into()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut inbound = self.inbound.lock().unwrap_or_else(PoisonError::into_inner);
        let Poll::Ready(packet) = inbound.poll_recv(cx) else {
            return Poll::Pending;
        };
        let Some(packet) = packet else {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        };
        let (Some(buf), Some(slot)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Ok(0));
        };
        let len = packet.len().min(buf.len());
        buf[..len].copy_from_slice(&packet[..len]);
        *slot = RecvMeta {
            addr: THERE,
            len,
            stride: len,
            ecn: None,
            dst_ip: None,
        };
        Poll::Ready(Ok(1))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(HERE)
    }

    fn may_fragment(&self) -> bool {
        false
    }
}
