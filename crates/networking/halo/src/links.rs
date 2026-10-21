//! The real medium: QUIC on UDP at a member's halo address or where it told a link it is reached,
//! else QUIC in a WebSocket on its HTTPS address. A dial stays in flight to each member with no
//! direct link; a direct link is preferred, and a relayed one closes as soon as one is adopted.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use bytes::Bytes;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::Metrics;
use commonware_runtime::telemetry::metrics::Registered;
use commonware_runtime::telemetry::metrics::raw::Counter;
use futures::StreamExt as _;
use futures::future::Either;
use futures::stream::FuturesUnordered;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf};
use tokio::sync::{Notify, mpsc, watch};

use crate::control::{self, Said};
use crate::members::Members;
use crate::quic::{self, Quic, Udp};
use crate::reach::Reach;
use crate::relay::{self, Pipe, Session};
use crate::transport::{Error, Transport};

const YIELD: Duration = Duration::from_secs(1);
const INBOUND: NonZeroUsize = NonZeroUsize::new(256).expect("nonzero");
const DATAGRAMS: NonZeroUsize = NonZeroUsize::new(1024).expect("nonzero");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Queues {
    pub inbound: NonZeroUsize,
    pub datagrams: NonZeroUsize,
}

impl Default for Queues {
    fn default() -> Queues {
        Queues {
            inbound: INBOUND,
            datagrams: DATAGRAMS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Path {
    Direct,
    Relayed,
}

pub struct Carried {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl AsyncRead for Carried {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for Carried {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().send)
            .poll_write(cx, buf)
            .map_err(io::Error::from)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_shutdown(cx)
    }
}

impl Carried {
    /// Abandons the stream, so the peer reads a reset rather than a clean end.
    pub fn reset(mut self) {
        let _ = self.send.reset(0u32.into());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Dialed,
    Accepted,
}

fn preferred(me: &PublicKey, peer: &PublicKey, side: Side) -> bool {
    match side {
        Side::Dialed => me < peer,
        Side::Accepted => peer < me,
    }
}

struct Link {
    connection: quinn::Connection,
    preferred: bool,
    path: Path,
    _endpoint: Option<quinn::Endpoint>,
}

impl Link {
    fn open(&self) -> bool {
        self.connection.close_reason().is_none()
    }
}

#[derive(Default)]
struct Outbox {
    introduce: HashMap<PublicKey, (Vec<SocketAddr>, bool)>,
    introduced: HashMap<PublicKey, (Vec<SocketAddr>, bool)>,
}

impl Outbox {
    fn said(&self) -> Vec<Said> {
        let introduce = self
            .introduce
            .iter()
            .map(|(to, (addrs, reply))| Said::Introduce {
                to: to.as_ref().to_vec(),
                addrs: control::written(addrs),
                reply: *reply,
            });
        let introduced = self
            .introduced
            .iter()
            .map(|(from, (addrs, reply))| Said::Introduced {
                from: from.as_ref().to_vec(),
                addrs: control::written(addrs),
                reply: *reply,
            });
        introduce.chain(introduced).collect()
    }

    fn forget(&mut self, key: &PublicKey) {
        self.introduce.remove(key);
        self.introduced.remove(key);
    }
}

struct Peer {
    dialing: tokio::sync::Mutex<()>,
    links: Mutex<Vec<Link>>,
    changed: watch::Sender<()>,
    kept: AtomicBool,
    relaying: AtomicBool,
    told: watch::Sender<Vec<SocketAddr>>,
    outbox: Mutex<Outbox>,
    posted: watch::Sender<()>,
}

impl Default for Peer {
    fn default() -> Peer {
        Peer {
            dialing: tokio::sync::Mutex::default(),
            links: Mutex::default(),
            changed: watch::channel(()).0,
            kept: AtomicBool::new(false),
            relaying: AtomicBool::new(false),
            told: watch::channel(Vec::new()).0,
            outbox: Mutex::default(),
            posted: watch::channel(()).0,
        }
    }
}

impl Peer {
    fn held(&self) -> std::sync::MutexGuard<'_, Vec<Link>> {
        self.links.lock().expect("the links lock is never poisoned")
    }

    fn best(held: &[Link]) -> Option<&Link> {
        held.iter()
            .filter(|link| link.open())
            .max_by_key(|link| (link.path == Path::Direct, link.preferred))
    }

    fn live(&self) -> Option<quinn::Connection> {
        Peer::best(&self.held()).map(|link| link.connection.clone())
    }

    fn path(&self) -> Option<Path> {
        Peer::best(&self.held()).map(|link| link.path)
    }

    fn paths(&self) -> Vec<Path> {
        self.held()
            .iter()
            .filter(|link| link.open())
            .map(|link| link.path)
            .collect()
    }

    fn direct(&self) -> bool {
        self.path() == Some(Path::Direct)
    }

    fn adopt(&self, link: Link) {
        let mut held = self.held();
        held.retain(Link::open);
        held.push(link);
        let direct = held.iter().any(|link| link.path == Path::Direct);
        if direct {
            for relayed in held.iter().filter(|link| link.path == Path::Relayed) {
                relayed.connection.close(0u32.into(), b"direct");
            }
        }
        drop(held);
        self.changed.send_replace(());
    }

    fn forget(&self, connection: &quinn::Connection) {
        let mut held = self.held();
        held.retain(|kept| kept.connection.stable_id() != connection.stable_id());
        drop(held);
        self.changed.send_replace(());
    }

    fn unseat(&self) {
        let gone = std::mem::take(&mut *self.held());
        for link in gone {
            link.connection.close(0u32.into(), b"unseated");
        }
        self.told.send_replace(Vec::new());
        self.post(|outbox| *outbox = Outbox::default());
    }

    fn told(&self, addrs: Vec<SocketAddr>) -> bool {
        let fresh = self.told.send_if_modified(|held| {
            let fresh = *held != addrs;
            if fresh {
                *held = addrs;
            }
            fresh
        });
        if fresh {
            self.changed.send_replace(());
        }
        fresh
    }

    fn post(&self, write: impl FnOnce(&mut Outbox)) {
        write(
            &mut self
                .outbox
                .lock()
                .expect("the outbox lock is never poisoned"),
        );
        self.posted.send_replace(());
    }

    fn said(&self) -> Vec<Said> {
        self.outbox
            .lock()
            .expect("the outbox lock is never poisoned")
            .said()
    }

    async fn arrival(&self, changed: &mut watch::Receiver<()>) -> Option<quinn::Connection> {
        loop {
            changed.changed().await.ok()?;
            if let Some(connection) = self.live() {
                return Some(connection);
            }
        }
    }
}

enum Attempt {
    Linked,
    Unanswered,
    Told,
    Refused,
}

enum Next {
    Again,
    Pause(Duration),
    Wait,
}

pub struct Links {
    me: PublicKey,
    members: Arc<Members>,
    quic: Quic,
    reach: Arc<dyn Reach>,
    peers: Mutex<HashMap<PublicKey, Arc<Peer>>>,
    mine: watch::Sender<Vec<SocketAddr>>,
    relearn: Arc<Notify>,
    inbound: mpsc::Sender<(PublicKey, Carried)>,
    accepted: tokio::sync::Mutex<mpsc::Receiver<(PublicKey, Carried)>>,
    received: mpsc::Sender<(PublicKey, Bytes)>,
    datagrams: tokio::sync::Mutex<mpsc::Receiver<(PublicKey, Bytes)>>,
    opened: Registered<Counter>,
}

impl Links {
    pub fn start<E: Metrics>(
        context: &E,
        identity: PrivateKey,
        members: Arc<Members>,
        udp: Udp,
        queues: Queues,
        reach: Arc<dyn Reach>,
    ) -> Result<Arc<Links>, quic::Error> {
        let quic = Quic::bind(udp, &identity, members.clone())?;
        let (inbound, accepted) = mpsc::channel(queues.inbound.get());
        let (received, datagrams) = mpsc::channel(queues.datagrams.get());
        let opened = context.register(
            "halo_streams_opened",
            "streams this node opened to a member over QUIC",
            Counter::default(),
        );
        let links = Arc::new(Links {
            me: identity.public_key(),
            members,
            quic,
            reach,
            peers: Mutex::default(),
            mine: watch::channel(Vec::new()).0,
            relearn: Arc::new(Notify::new()),
            inbound,
            accepted: tokio::sync::Mutex::new(accepted),
            received,
            datagrams: tokio::sync::Mutex::new(datagrams),
            opened,
        });
        tokio::spawn(accepting(
            Arc::downgrade(&links),
            links.quic.endpoint().clone(),
        ));
        tokio::spawn(learning(Arc::downgrade(&links)));
        Ok(links)
    }

    pub fn udp(&self) -> io::Result<SocketAddr> {
        self.quic.local()
    }

    pub fn reached_at(&self) -> watch::Receiver<Vec<SocketAddr>> {
        self.mine.subscribe()
    }

    pub async fn dial_self(&self, halo: &str) -> Result<(), &'static str> {
        let Some(at) = resolved(&self.quic, halo).await else {
            return Err("unresolved");
        };
        let connection = self.quic.dial(&self.me, at).await;
        let connection = connection.map_err(|_| "undialed")?;
        connection.close(0u32.into(), b"reached itself");
        Ok(())
    }

    pub fn seated(self: &Arc<Self>, changed: Vec<PublicKey>) {
        let moved = !changed.is_empty();
        for key in changed {
            if key == self.me {
                continue;
            }
            let peer = self.peer(&key);
            let unkept = !peer.kept.swap(true, Ordering::AcqRel);
            if unkept {
                tokio::spawn(keeping(Arc::downgrade(self), key, peer.changed.subscribe()));
            }
            peer.changed.send_replace(());
        }
        if moved {
            self.relearn.notify_one();
        }
    }

    pub async fn linked(&self, key: &PublicKey) {
        self.reached(key, |peer| peer.live().is_some()).await;
    }

    pub async fn direct(&self, key: &PublicKey) {
        self.reached(key, Peer::direct).await;
    }

    async fn reached(&self, key: &PublicKey, done: impl Fn(&Peer) -> bool) {
        let peer = self.peer(key);
        let mut changed = peer.changed.subscribe();
        loop {
            if done(&peer) {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn path(&self, key: &PublicKey) -> Option<Path> {
        self.peer(key).path()
    }

    pub fn paths(&self, key: &PublicKey) -> Vec<Path> {
        self.peer(key).paths()
    }

    pub fn link_changes(&self, key: &PublicKey) -> watch::Receiver<()> {
        self.peer(key).changed.subscribe()
    }

    pub async fn relayed(self: &Arc<Self>, pipe: Pipe) -> Result<PublicKey, quic::Error> {
        let session = Session::over(pipe);
        let quic = self.quic.beside(session.clone())?;
        let accepted = quic.endpoint().accept();
        let ended = session.ended();
        futures::pin_mut!(accepted, ended);
        let incoming = match futures::future::select(accepted, ended).await {
            Either::Left((Some(incoming), _)) => incoming,
            Either::Left((None, _)) | Either::Right(((), _)) => {
                return Err(quic::Error::Io(io::ErrorKind::ConnectionAborted.into()));
            }
        };
        let connection = incoming.await?;
        let Some(peer) = quic::peer_of(&connection) else {
            connection.close(0u32.into(), b"no node key");
            return Err(quic::Error::Io(io::ErrorKind::InvalidData.into()));
        };
        self.carry(
            &peer,
            connection,
            Side::Accepted,
            Path::Relayed,
            Some(quic.endpoint().clone()),
        );
        Ok(peer)
    }

    fn peer(&self, key: &PublicKey) -> Arc<Peer> {
        self.peers
            .lock()
            .expect("the peers lock is never poisoned")
            .entry(key.clone())
            .or_default()
            .clone()
    }

    fn all(&self) -> Vec<(PublicKey, Arc<Peer>)> {
        self.peers
            .lock()
            .expect("the peers lock is never poisoned")
            .iter()
            .map(|(key, peer)| (key.clone(), peer.clone()))
            .collect()
    }

    fn stir(&self) {
        for (key, peer) in self.all() {
            if key != self.me {
                self.introduce(&key, &peer);
            }
            peer.changed.send_replace(());
        }
    }

    async fn connection(self: &Arc<Self>, to: &PublicKey) -> Result<quinn::Connection, Error> {
        let peer = self.peer(to);
        let mut changed = peer.changed.subscribe();
        if let Some(connection) = peer.live() {
            return Ok(connection);
        }
        let dialed = async {
            let mut told = peer.told.subscribe();
            let targets = self.targets(to, &peer).await;
            match self.dial(to, &peer, targets, &mut told).await {
                Attempt::Linked => peer.live().ok_or_else(|| Error::Unreachable(to.clone())),
                Attempt::Unanswered | Attempt::Told | Attempt::Refused => {
                    Err(Error::Unreachable(to.clone()))
                }
            }
        };
        let arrived = peer.arrival(&mut changed);
        futures::pin_mut!(dialed, arrived);
        match futures::future::select(dialed, arrived).await {
            Either::Left((dialed, _)) => dialed,
            Either::Right((Some(connection), _)) => Ok(connection),
            Either::Right((None, dialed)) => dialed.await,
        }
    }

    async fn targets(&self, key: &PublicKey, peer: &Peer) -> Vec<SocketAddr> {
        let mut targets = Vec::new();
        if let Some(halo) = self.members.halo(key)
            && let Some(at) = resolved(&self.quic, &halo).await
        {
            targets.push(at);
        }
        let told: Vec<SocketAddr> = peer.told.borrow().clone();
        for at in told {
            let fresh = !targets.contains(&at);
            if fresh && self.quic.reaches(&at) {
                targets.push(at);
            }
        }
        targets
    }

    async fn dial(
        self: &Arc<Self>,
        to: &PublicKey,
        peer: &Peer,
        targets: Vec<SocketAddr>,
        told: &mut watch::Receiver<Vec<SocketAddr>>,
    ) -> Attempt {
        let _dialing = peer.dialing.lock().await;
        if peer.direct() {
            return Attempt::Linked;
        }
        let mut attempts: FuturesUnordered<_> = targets
            .into_iter()
            .map(|at| async move { self.quic.dial(to, at).await })
            .collect();
        if attempts.is_empty() {
            return Attempt::Refused;
        }
        let mut unanswered = false;
        loop {
            let heard = told.changed();
            futures::pin_mut!(heard);
            match futures::future::select(attempts.next(), heard).await {
                Either::Left((None, _)) => break,
                Either::Left((Some(Ok(connection)), _)) => {
                    tracing::debug!(target: "ducktape::halo", peer = %to, "halo dialed a member directly");
                    self.carry(to, connection, Side::Dialed, Path::Direct, None);
                    return Attempt::Linked;
                }
                Either::Left((Some(Err(error)), _)) => unanswered |= error.unanswered(),
                Either::Right(_) => return Attempt::Told,
            }
        }
        match unanswered {
            true => Attempt::Unanswered,
            false => Attempt::Refused,
        }
    }

    async fn relay(self: &Arc<Self>, key: &PublicKey, https: &str) -> Result<(), quic::Error> {
        let pipe = self.reach.relay(https).await?;
        let quic = self.quic.beside(Session::over(pipe))?;
        let connection = quic.dial(key, relay::THERE).await?;
        self.carry(
            key,
            connection,
            Side::Dialed,
            Path::Relayed,
            Some(quic.endpoint().clone()),
        );
        Ok(())
    }

    fn relay_unlinked(self: &Arc<Self>, key: &PublicKey, peer: &Arc<Peer>) {
        if peer.live().is_some() {
            return;
        }
        let Some(https) = self.members.https(key) else {
            return;
        };
        if peer.relaying.swap(true, Ordering::AcqRel) {
            return;
        }
        let links = Arc::downgrade(self);
        let key = key.clone();
        let peer = peer.clone();
        tokio::spawn(async move {
            let Some(strong) = links.upgrade() else {
                return;
            };
            let relayed = strong.relay(&key, &https).await;
            drop(strong);
            peer.relaying.store(false, Ordering::Release);
            if let Err(error) = relayed {
                tracing::debug!(target: "ducktape::halo", peer = %key, %error, reason = "unrelayed", "halo did not reach a member over its https address");
            }
        });
    }

    fn introduce(&self, key: &PublicKey, peer: &Peer) {
        let unlinked = peer.live().is_none() && self.members.https(key).is_none();
        let mine = self.mine.borrow().clone();
        if !unlinked || mine.is_empty() {
            return;
        }
        let introduction = (mine, true);
        for (carrier, carrying) in self.all() {
            if carrier == *key || carrying.live().is_none() {
                continue;
            }
            let held = carrying
                .outbox
                .lock()
                .expect("the outbox lock is never poisoned")
                .introduce
                .get(key)
                == Some(&introduction);
            if held {
                continue;
            }
            carrying.post(|outbox| {
                outbox.introduce.insert(key.clone(), introduction.clone());
            });
        }
    }

    fn heard(&self, from: &PublicKey, said: Said) {
        match said {
            Said::Mine { addrs } => {
                self.peer(from).told(control::read_addrs(&addrs));
            }
            Said::Introduce { to, addrs, reply } => {
                let Ok(to) = PublicKey::decode(to.as_slice()) else {
                    return;
                };
                if to != self.me && self.members.admits(&to) {
                    self.peer(&to).post(|outbox| {
                        outbox
                            .introduced
                            .insert(from.clone(), (control::read_addrs(&addrs), reply));
                    });
                }
            }
            Said::Introduced {
                from: who,
                addrs,
                reply,
            } => {
                let Ok(who) = PublicKey::decode(who.as_slice()) else {
                    return;
                };
                let known = who != self.me && self.members.admits(&who);
                if !known {
                    return;
                }
                let introduced = self.peer(&who);
                introduced.told(control::read_addrs(&addrs));
                let mine = self.mine.borrow().clone();
                if reply && !mine.is_empty() && !introduced.direct() {
                    self.peer(from).post(|outbox| {
                        let held = outbox.introduce.entry(who).or_insert((mine.clone(), false));
                        if held.0 != mine {
                            *held = (mine, false);
                        }
                    });
                }
            }
        }
    }

    async fn learn(&self) {
        let Ok(udp) = self.quic.local() else {
            return;
        };
        for https in self.members.https_asked(&self.me) {
            match self.reach.seen(&https, udp).await {
                Ok(ip) => {
                    let at = SocketAddr::new(ip, udp.port());
                    let moved = self.mine.send_if_modified(|mine| {
                        let moved = *mine != [at];
                        if moved {
                            *mine = vec![at];
                        }
                        moved
                    });
                    if moved {
                        tracing::info!(target: "ducktape::halo", event = "halo_learned", "halo learned the address it is reached at directly");
                        self.stir();
                    }
                    return;
                }
                Err(error) => {
                    tracing::debug!(target: "ducktape::halo", %https, %error, reason = "unseen", "halo could not learn its address from a member's https address");
                }
            }
        }
    }

    fn carry(
        self: &Arc<Self>,
        key: &PublicKey,
        connection: quinn::Connection,
        side: Side,
        path: Path,
        endpoint: Option<quinn::Endpoint>,
    ) {
        let link = Link {
            connection: connection.clone(),
            preferred: preferred(&self.me, key, side),
            path,
            _endpoint: endpoint,
        };
        self.peer(key).adopt(link);
        tokio::spawn(carried(
            Arc::downgrade(self),
            key.clone(),
            connection.clone(),
        ));
        tokio::spawn(telling(Arc::downgrade(self), key.clone(), connection));
        let unlearned = self.mine.borrow().is_empty();
        if path == Path::Relayed && unlearned {
            self.relearn.notify_one();
        }
        self.stir();
    }

    fn lost(&self, key: &PublicKey, connection: &quinn::Connection) {
        self.peer(key).forget(connection);
        let any = self.all().iter().any(|(_, peer)| peer.live().is_some());
        if !any {
            self.relearn.notify_one();
        }
    }

    async fn keep(
        self: &Arc<Self>,
        key: &PublicKey,
        tries: &mut u32,
        changed: &mut watch::Receiver<()>,
    ) -> Next {
        changed.borrow_and_update();
        let peer = self.peer(key);
        let Some(halo) = self.members.halo(key) else {
            peer.unseat();
            for (_, carrying) in self.all() {
                carrying.post(|outbox| outbox.forget(key));
            }
            return Next::Wait;
        };
        if peer.direct() {
            *tries = 0;
            return Next::Wait;
        }
        self.relay_unlinked(key, &peer);
        self.introduce(key, &peer);
        let mut told = peer.told.subscribe();
        let targets = self.targets(key, &peer).await;
        if targets.is_empty() {
            return Next::Wait;
        }
        let dials_first = preferred(&self.me, key, Side::Dialed);
        let yields = !dials_first && *tries == 0 && peer.live().is_none();
        if yields {
            *tries = 1;
            return Next::Pause(YIELD);
        }
        match self.dial(key, &peer, targets, &mut told).await {
            Attempt::Linked => {
                *tries = 0;
                Next::Wait
            }
            Attempt::Told => Next::Again,
            Attempt::Unanswered => {
                *tries += 1;
                let worth_a_line = *tries == 2 || tries.is_multiple_of(10);
                if worth_a_line {
                    tracing::debug!(target: "ducktape::halo", peer = %key, at = %halo, attempts = *tries, reason = "undialed", "halo did not reach a member directly; it dials again");
                }
                Next::Again
            }
            Attempt::Refused => {
                tracing::debug!(target: "ducktape::halo", peer = %key, at = %halo, reason = "refused", "halo's dials to a member were refused; it dials again when the member or its address moves");
                Next::Wait
            }
        }
    }
}

impl Drop for Links {
    fn drop(&mut self) {
        self.quic.endpoint().close(0u32.into(), b"halo stopped");
    }
}

async fn resolved(quic: &Quic, halo: &str) -> Option<SocketAddr> {
    let mut found = tokio::net::lookup_host(halo).await.ok()?;
    found.find(|at| quic.reaches(at))
}

async fn keeping(links: Weak<Links>, key: PublicKey, mut changed: watch::Receiver<()>) {
    let mut tries = 0;
    loop {
        let Some(strong) = links.upgrade() else {
            return;
        };
        let next = strong.keep(&key, &mut tries, &mut changed).await;
        drop(strong);
        let woken = match next {
            Next::Again => true,
            Next::Wait => changed.changed().await.is_ok(),
            Next::Pause(pause) => {
                let woken = changed.changed();
                let paused = tokio::time::sleep(pause);
                futures::pin_mut!(woken, paused);
                match futures::future::select(woken, paused).await {
                    Either::Left((woken, _)) => woken.is_ok(),
                    Either::Right(((), _)) => true,
                }
            }
        };
        if !woken {
            return;
        }
    }
}

async fn learning(links: Weak<Links>) {
    loop {
        let Some(strong) = links.upgrade() else {
            return;
        };
        let relearn = strong.relearn.clone();
        drop(strong);
        relearn.notified().await;
        let Some(strong) = links.upgrade() else {
            return;
        };
        strong.learn().await;
    }
}

async fn accepting(links: Weak<Links>, endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let links = links.clone();
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else {
                return;
            };
            let Some(links) = links.upgrade() else {
                return;
            };
            let Some(peer) = quic::peer_of(&connection) else {
                connection.close(0u32.into(), b"no node key");
                return;
            };
            if peer == links.me {
                connection.close(0u32.into(), b"reached itself");
                return;
            }
            links.carry(&peer, connection, Side::Accepted, Path::Direct, None);
        });
    }
}

async fn carried(links: Weak<Links>, peer: PublicKey, connection: quinn::Connection) {
    let streams = async {
        while let Ok((send, recv)) = connection.accept_bi().await {
            let Some(links) = links.upgrade() else {
                return;
            };
            let _ = links
                .inbound
                .send((peer.clone(), Carried { send, recv }))
                .await;
        }
    };
    let datagrams = async {
        while let Ok(datagram) = connection.read_datagram().await {
            let Some(links) = links.upgrade() else {
                return;
            };
            let _ = links.received.try_send((peer.clone(), datagram));
        }
    };
    let said = async {
        while let Ok(recv) = connection.accept_uni().await {
            tokio::spawn(hearing(links.clone(), peer.clone(), recv));
        }
    };
    futures::pin_mut!(streams, datagrams, said);
    let ended = futures::future::select(streams, datagrams);
    futures::future::select(ended, said).await;
    if let Some(links) = links.upgrade() {
        links.lost(&peer, &connection);
    }
}

async fn hearing(links: Weak<Links>, from: PublicKey, mut recv: quinn::RecvStream) {
    let Ok(version) = recv.read_u8().await else {
        return;
    };
    if version != control::VERSION {
        return;
    }
    while let Ok(Some(said)) = control::read(&mut recv).await {
        let Some(links) = links.upgrade() else {
            return;
        };
        links.heard(&from, said);
    }
}

async fn telling(links: Weak<Links>, key: PublicKey, connection: quinn::Connection) {
    let Some(strong) = links.upgrade() else {
        return;
    };
    let mut mine = strong.mine.subscribe();
    let peer = strong.peer(&key);
    drop(strong);
    let mut posted = peer.posted.subscribe();
    let Ok(mut send) = connection.open_uni().await else {
        return;
    };
    if send.write_all(&[control::VERSION]).await.is_err() {
        return;
    }
    let mut said_mine: Vec<SocketAddr> = Vec::new();
    let mut said: Vec<Said> = Vec::new();
    loop {
        let current = mine.borrow_and_update().clone();
        if !current.is_empty() && current != said_mine {
            let said = Said::Mine {
                addrs: control::written(&current),
            };
            if control::write(&mut send, &said).await.is_err() {
                return;
            }
            said_mine = current;
        }
        posted.borrow_and_update();
        let held = peer.said();
        for fresh in held.iter().filter(|held| !said.contains(held)) {
            if control::write(&mut send, fresh).await.is_err() {
                return;
            }
        }
        said = held;
        let moved = mine.changed();
        let post = posted.changed();
        let closed = connection.closed();
        futures::pin_mut!(moved, post, closed);
        let woken = futures::future::select(moved, post);
        match futures::future::select(woken, closed).await {
            Either::Left((Either::Left((Ok(()), _)) | Either::Right((Ok(()), _)), _)) => {}
            Either::Left(_) | Either::Right(_) => return,
        }
    }
}

impl Transport for Arc<Links> {
    type Stream = Carried;

    async fn connect(&self, to: &PublicKey) -> Result<Carried, Error> {
        let connection = self.connection(to).await?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|_| Error::Unreachable(to.clone()))?;
        self.opened.inc();
        Ok(Carried { send, recv })
    }

    async fn accept(&self) -> Option<(PublicKey, Carried)> {
        self.accepted.lock().await.recv().await
    }

    async fn send_datagram(&self, to: &PublicKey, frame: Bytes) -> Result<(), Error> {
        let connection = self.connection(to).await?;
        let most = connection.max_datagram_size().unwrap_or(0);
        let fits = frame.len() <= most;
        if !fits {
            return Err(Error::Oversized(frame.len()));
        }
        connection
            .send_datagram(frame)
            .map_err(|_| Error::NoDatagrams(to.clone()))
    }

    async fn recv_datagram(&self) -> Option<(PublicKey, Bytes)> {
        self.datagrams.lock().await.recv().await
    }
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::Signer as _;

    use super::*;

    #[test]
    fn of_two_connections_between_a_pair_both_ends_prefer_the_one_the_lower_key_dialed() {
        let one = PrivateKey::from_seed(1).public_key();
        let two = PrivateKey::from_seed(2).public_key();
        let (low, high) = if one < two { (one, two) } else { (two, one) };
        assert!(preferred(&low, &high, Side::Dialed));
        assert!(preferred(&high, &low, Side::Accepted));
        assert!(!preferred(&high, &low, Side::Dialed));
        assert!(!preferred(&low, &high, Side::Accepted));
    }

    #[test]
    fn an_introduction_is_said_while_held_and_not_once_its_member_is_forgotten() {
        let to = PrivateKey::from_seed(3).public_key();
        let mine: Vec<SocketAddr> = vec!["203.0.113.7:9100".parse().unwrap()];
        let mut outbox = Outbox::default();
        outbox.introduce.insert(to.clone(), (mine.clone(), true));
        assert_eq!(outbox.said().len(), 1);
        assert_eq!(outbox.said(), outbox.said());
        outbox.forget(&to);
        assert!(outbox.said().is_empty());
    }

    #[test]
    fn a_members_newest_addresses_replace_what_it_told_before() {
        let peer = Peer::default();
        let first: Vec<SocketAddr> = vec!["203.0.113.7:9100".parse().unwrap()];
        let second: Vec<SocketAddr> = vec!["198.51.100.2:9100".parse().unwrap()];
        assert!(peer.told(first));
        assert!(peer.told(second.clone()));
        assert_eq!(*peer.told.borrow(), second);
    }
}
