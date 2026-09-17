//! Host-lifecycle helpers for per-use planes (the per-use data plane):
//! every service binds the
//! same way — compute its overlay addresses, retry the bind until the
//! reachability plane's interface (and this node's `/128`) exists,
//! construct the [`DataPlane`], and register its stream service. Only
//! admission, the address book, and the accept/serve loop differ per
//! service — callers keep those; this owns the shared bring-up core so it
//! is written once instead of once per [`Service`] consumer.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use crate::{
    AddressBook, AdmissionPolicy, BulkPacer, DataPlane, OverlaySockets, PlaneConfig, RegisterError,
    Service, SocketFactory, StreamPolicy, StreamService,
};

/// Everything [`bind_stream_plane`] needs to bind one service's overlay
/// sockets and register its stream service. `book` (the caller's combined
/// address book / admission policy) travels separately, since it is
/// generic over the caller's type rather than a trait object here.
pub struct StreamPlaneSpec {
    /// This node's own overlay `/128` — where the service's sockets bind.
    pub own_ip: IpAddr,
    pub service: Service,
    pub pacing: StreamPacing,
    pub policy: StreamPolicy,
    /// How long to wait between failed binds. The overlay `/128` only
    /// exists once the reachability plane has the interface up, so a fresh
    /// bring-up races that — this is the retry cadence while it waits.
    pub retry: Duration,
}

/// Whether one per-use plane owns its stream budget or participates in a
/// process-wide link budget. This avoids accepting a local config that would
/// be silently ignored whenever a shared pacer is present.
pub enum StreamPacing {
    Local(PlaneConfig),
    Shared(BulkPacer),
}

/// Bind one service's overlay sockets on this node's `/128`, retrying on
/// `retry` until the overlay interface (and the `/128`) exists. The wait is
/// unbounded by contract — the reachability plane brings the interface up in
/// the background — so it is reported at a bounded cadence: the first failed
/// attempt and every 20th after carry the running count (a plane that never
/// comes up is a climbing `attempts`), and a bind that lands after retries
/// says so once.
pub async fn bind_overlay_sockets(
    factory: Arc<dyn SocketFactory>,
    own_ip: IpAddr,
    service: Service,
    book: Arc<dyn AddressBook>,
    retry: Duration,
) -> OverlaySockets {
    let datagram_bind = SocketAddr::new(own_ip, service.overlay_datagram_port());
    let stream_bind = SocketAddr::new(own_ip, service.overlay_stream_port());
    let started = std::time::Instant::now();
    let mut attempts: u64 = 0;
    loop {
        match OverlaySockets::bind_with(factory.clone(), datagram_bind, stream_bind, book.clone())
            .await
        {
            Ok(sockets) => {
                if attempts > 0 {
                    tracing::info!(
                        target: "ducktape::dataplane",
                        ?service,
                        attempts,
                        elapsed_s = started.elapsed().as_secs(),
                        "overlay plane bound (the interface came up)"
                    );
                }
                return sockets;
            }
            Err(err) => {
                attempts += 1;
                if attempts == 1 || attempts.is_multiple_of(20) {
                    tracing::warn!(
                        target: "ducktape::dataplane",
                        reason = "overlay_bind_retry",
                        ?service,
                        %datagram_bind,
                        attempts,
                        elapsed_s = started.elapsed().as_secs(),
                        error = %err,
                        "overlay plane NOT bound — waiting on the overlay interface"
                    );
                }
                tokio::time::sleep(retry).await;
            }
        }
    }
}

/// Bind the service's overlay sockets (retrying on `spec.retry` until the
/// overlay interface is up), start the plane, and register its stream
/// service. The returned [`DataPlane`] must be kept alive by the caller —
/// its demux/accept pumps stop when it drops. On a [`RegisterError`] (the
/// service already registered — a caller bug, since each `Service` binds
/// exactly one plane) the freshly bound `plane` is dropped with the error,
/// which stops its pumps and releases the sockets.
pub async fn bind_stream_plane<B>(
    spec: StreamPlaneSpec,
    factory: Arc<dyn SocketFactory>,
    book: Arc<B>,
) -> Result<
    (
        DataPlane<OverlaySockets>,
        Arc<StreamService<OverlaySockets>>,
    ),
    RegisterError,
>
where
    B: AddressBook + AdmissionPolicy + Send + Sync + 'static,
{
    let sockets = bind_overlay_sockets(
        factory,
        spec.own_ip,
        spec.service,
        book.clone() as Arc<dyn AddressBook>,
        spec.retry,
    )
    .await;
    let admission: Arc<dyn AdmissionPolicy> = book;
    let plane = match spec.pacing {
        StreamPacing::Local(config) => DataPlane::new(sockets, admission, config),
        StreamPacing::Shared(pacer) => DataPlane::new_with_pacer(sockets, admission, pacer),
    };
    let svc = plane.stream_service(spec.service, spec.policy)?;
    Ok((plane, Arc::new(svc)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::net::Ipv6Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    // `super::*` already brings in AddressBook, AdmissionPolicy, RegisterError,
    // and SocketFactory (all `use`d by the parent module); only the
    // test-only real-socket plumbing needs importing here.
    use crate::{
        BoxFuture, DatagramSocket, FlowId, OsSocketFactory, PeerId, PlaneStream, StreamListener,
    };

    /// A factory that refuses to bind N times, then delegates to the OS
    /// factory — models the overlay `/128` appearing late (the reachability
    /// plane brings the interface up in the background while this node
    /// keeps retrying its bind).
    struct FlakyFactory {
        failures_left: AtomicUsize,
        inner: OsSocketFactory,
    }

    impl FlakyFactory {
        fn new(failures: usize) -> Self {
            FlakyFactory {
                failures_left: AtomicUsize::new(failures),
                inner: OsSocketFactory,
            }
        }
    }

    impl SocketFactory for FlakyFactory {
        fn bind_udp(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<Box<dyn DatagramSocket>>> {
            if self.failures_left.load(Ordering::SeqCst) > 0 {
                self.failures_left.fetch_sub(1, Ordering::SeqCst);
                return Box::pin(async {
                    Err(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "overlay /128 not up yet",
                    ))
                });
            }
            self.inner.bind_udp(addr)
        }

        fn bind_listener(
            &self,
            addr: SocketAddr,
        ) -> BoxFuture<'_, io::Result<Box<dyn StreamListener>>> {
            self.inner.bind_listener(addr)
        }

        fn dial_from<'a>(
            &'a self,
            local_ip: IpAddr,
            dest: SocketAddr,
        ) -> BoxFuture<'a, io::Result<PlaneStream>> {
            self.inner.dial_from(local_ip, dest)
        }
    }

    /// An address book that resolves nothing — this test only exercises
    /// bind-retry and registration, never a real peer exchange.
    struct NullBook;

    impl AddressBook for NullBook {
        fn datagram_addr(&self, _peer: PeerId) -> Option<SocketAddr> {
            None
        }
        fn stream_addr(&self, _peer: PeerId) -> Option<SocketAddr> {
            None
        }
        fn peer_at(&self, _src: IpAddr) -> Option<PeerId> {
            None
        }
    }

    impl AdmissionPolicy for NullBook {
        fn permits(&self, _peer: PeerId, _service: Service, _flow: FlowId) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn bind_retries_until_interface_appears() {
        let factory: Arc<dyn SocketFactory> = Arc::new(FlakyFactory::new(2));
        let book = Arc::new(NullBook);
        let spec = StreamPlaneSpec {
            own_ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
            service: Service::STATE_SYNC,
            pacing: StreamPacing::Local(PlaneConfig {
                bulk_bytes_per_sec: 1_000_000,
                bulk_burst_bytes: 64 * 1024,
            }),
            policy: StreamPolicy { accept_backlog: 4 },
            retry: Duration::from_millis(10),
        };

        let started = Instant::now();
        let (plane, svc) = tokio::time::timeout(
            Duration::from_millis(500),
            bind_stream_plane(spec, factory, book),
        )
        .await
        .expect("bind_stream_plane must not hang past the retry budget")
        .expect("bind must succeed once the (fake) interface comes up");
        let elapsed = started.elapsed();

        // Two failures at a 10ms retry cadence: the third attempt can only
        // succeed after at least two retry sleeps — this is what tells
        // "retried until bound" apart from "happened to bind first try".
        assert!(
            elapsed >= Duration::from_millis(20),
            "expected at least two retry sleeps (~20ms), took {elapsed:?}"
        );

        // The returned StreamService must already be registered on the
        // returned plane: a second registration for the same service is
        // refused.
        let dup = plane.stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 1 });
        assert!(
            matches!(dup, Err(RegisterError::AlreadyRegistered)),
            "bind_stream_plane's returned service must already be registered on the plane"
        );
        drop(svc);
    }
}
