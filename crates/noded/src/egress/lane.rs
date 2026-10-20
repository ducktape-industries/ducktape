//! The `ducktape/egress` lane: a member key with no node of its own serves egress through relay
//! nodes. The relay streams it the tip, its assigned requests and their bodies, and pools the
//! frames it signs; `serve_box` is the far end, run by `ducktape egress serve`.
use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use abi::role::egress::{Answer, Op, RequestRow};
use abi::role::{self, validators};
use abi::{BlobId, ProgramId, Refusal};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use halo::{Class, Halo, Lane, Seat};
use host::{Layer, RECENT, Tip};
use node::{BLOCK_BYTES, Frame, Preconfirm, Source};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::task::{AbortHandle, JoinSet};

use crate::egress::call::{self, Caller};
use crate::egress::calls::Calls;
use crate::egress::desk::{Called, Desk, Reading, Sent, Step};
use crate::egress::station::Serving;
use crate::egress::{Station, TARGET, Window, execute};
use crate::wire::Status;
use crate::{Client, Context, Daemon, Error, Result, Workspace};

pub const LANE: &str = "ducktape/egress";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum ToBox {
    Hello {
        network: String,
    },
    Body {
        id: u64,
        body: std::result::Result<Vec<u8>, String>,
    },
    Read {
        tip: Tip,
        role: ProgramId,
        assigned: Vec<RequestRow>,
        serving: Vec<String>,
        member: bool,
    },
    Unbound,
    Taken {
        tag: u64,
        verdict: Verdict,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum ToNode {
    Frame { tag: u64, frame: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Verdict {
    Pending,
    Stale,
    Carried,
    Untaken,
    Refused(Refusal),
}

impl Verdict {
    fn of(sent: &Sent) -> Verdict {
        match sent {
            Sent::Pending(_) => Verdict::Pending,
            Sent::Stale => Verdict::Stale,
            Sent::Carried => Verdict::Carried,
            Sent::Untaken => Verdict::Untaken,
            Sent::Refused(refusal) => Verdict::Refused(refusal.clone()),
        }
    }

    fn sent(self, frame: Vec<u8>) -> Sent {
        match self {
            Verdict::Pending => Sent::Pending(frame),
            Verdict::Stale => Sent::Stale,
            Verdict::Carried => Sent::Carried,
            Verdict::Untaken => Sent::Untaken,
            Verdict::Refused(refusal) => Sent::Refused(refusal),
        }
    }
}

pub async fn write<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &impl BorshSerialize,
) -> std::io::Result<()> {
    let bytes = borsh::to_vec(message)?;
    let len = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

pub async fn read<R: AsyncRead + Unpin, M: BorshDeserialize>(
    reader: &mut R,
    most: usize,
) -> std::io::Result<M> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > most {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a message of {len} bytes is past the {most} this side reads"),
        ));
    }
    let mut bytes = Vec::new();
    reader.take(len as u64).read_to_end(&mut bytes).await?;
    if bytes.len() != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    borsh::from_slice(&bytes)
}

fn frame_message_most() -> std::io::Result<usize> {
    let empty = ToNode::Frame {
        tag: 0,
        frame: Vec::new(),
    };
    Ok(BLOCK_BYTES + borsh::to_vec(&empty)?.len())
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn spawn<E: Context>(
    daemon: Arc<Daemon<E>>,
    rpc: tokio::runtime::Handle,
    backlog: NonZeroUsize,
) -> Result<()> {
    let streams = daemon.halo.plane().streams(LANE, backlog.get())?;
    let sessions = rpc.clone();
    rpc.spawn(async move {
        while let Some(inbound) = streams.accept().await {
            sessions.spawn(relay(daemon.clone(), inbound.from, inbound.stream));
        }
    });
    Ok(())
}

enum Gone {
    Read(std::io::Error),
    Write(std::io::Error),
    TipsClosed,
}

impl Gone {
    fn reason(&self) -> &'static str {
        match self {
            Gone::Read(_) => "read_failed",
            Gone::Write(_) => "write_failed",
            Gone::TipsClosed => "tips_closed",
        }
    }

    fn error(&self) -> Option<&std::io::Error> {
        match self {
            Gone::Read(error) | Gone::Write(error) => Some(error),
            Gone::TipsClosed => None,
        }
    }
}

async fn relay<E: Context, S: AsyncRead + AsyncWrite + Send + 'static>(
    daemon: Arc<Daemon<E>>,
    from: PublicKey,
    stream: S,
) {
    let key = from.as_ref().to_vec();
    let (reader, writer) = tokio::io::split(stream);
    let writer = tokio::sync::Mutex::new(writer);
    let gone = tokio::select! {
        gone = push(&daemon, key, &writer) => gone,
        gone = take(&daemon, reader, &writer) => gone,
    };
    tracing::info!(
        target: TARGET,
        event = "egress_box_gone",
        %from,
        reason = gone.reason(),
        error = gone.error().map(tracing::field::display),
        "a box serving egress over the lane left this node"
    );
}

async fn push<E: Context, W: AsyncWrite + Unpin>(
    daemon: &Arc<Daemon<E>>,
    key: Vec<u8>,
    writer: &tokio::sync::Mutex<W>,
) -> Gone {
    let hello = ToBox::Hello {
        network: daemon.descriptor.network.clone(),
    };
    if let Err(error) = write(&mut *writer.lock().await, &hello).await {
        return Gone::Write(error);
    }
    let mut tips = daemon.tips.subscribe();
    let mut bodied = BTreeSet::new();
    let mut fetching: HashMap<u64, AbortHandle> = HashMap::new();
    let mut fetches = JoinSet::new();
    loop {
        let message = match super::read_for(daemon, key.clone()).await {
            Ok(Some((role, tip, reading))) => {
                let assigned: BTreeSet<u64> = reading.assigned.iter().map(|row| row.id).collect();
                bodied.retain(|id| assigned.contains(id));
                fetching.retain(|id, fetch| {
                    let kept = assigned.contains(id);
                    if !kept {
                        fetch.abort();
                    }
                    kept
                });
                for row in &reading.assigned {
                    if bodied.contains(&row.id) || fetching.contains_key(&row.id) {
                        continue;
                    }
                    let id = row.id;
                    let daemon = daemon.clone();
                    let parts = row.request.parts().to_vec();
                    let fetch = fetches.spawn(async move { (id, sought(&daemon, &parts).await) });
                    fetching.insert(id, fetch);
                }
                let assigned = reading
                    .assigned
                    .into_iter()
                    .filter(|row| bodied.contains(&row.id))
                    .collect();
                Some(ToBox::Read {
                    tip,
                    role,
                    assigned,
                    serving: reading.serving,
                    member: reading.member,
                })
            }
            Ok(None) => Some(ToBox::Unbound),
            Err(error) => {
                tracing::debug!(
                    target: TARGET,
                    event = "egress_lane_unread",
                    reason = "egress_query_failed",
                    %error,
                    "the node could not read the egress program for a box"
                );
                None
            }
        };
        if let Some(message) = message
            && let Err(error) = write(&mut *writer.lock().await, &message).await
        {
            return Gone::Write(error);
        }
        tokio::select! {
            changed = tips.changed() => {
                if changed.is_err() {
                    return Gone::TipsClosed;
                }
            }
            Some(done) = fetches.join_next() => {
                let mut done = Some(done);
                while let Some(fetched) = done {
                    if let Ok((id, body)) = fetched
                        && fetching.remove(&id).is_some()
                    {
                        bodied.insert(id);
                        let message = ToBox::Body { id, body };
                        if let Err(error) = write(&mut *writer.lock().await, &message).await {
                            return Gone::Write(error);
                        }
                    }
                    done = fetches.try_join_next();
                }
            }
        }
    }
}

async fn sought<E: Context>(
    daemon: &Daemon<E>,
    parts: &[BlobId],
) -> std::result::Result<Vec<u8>, String> {
    for part in parts {
        let held = daemon.blobs.store().has(part) || daemon.blobs.seek(*part).await;
        if !held {
            return Err(format!(
                "the relay node does not hold part {} yet; no member gave it",
                abi::hex(part.digest())
            ));
        }
    }
    super::stored(daemon, parts)
}

async fn take<E: Context, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    daemon: &Daemon<E>,
    mut reader: R,
    writer: &tokio::sync::Mutex<W>,
) -> Gone {
    let most = match frame_message_most() {
        Ok(most) => most,
        Err(error) => return Gone::Read(error),
    };
    loop {
        let ToNode::Frame { tag, frame } = match read(&mut reader, most).await {
            Ok(message) => message,
            Err(error) => return Gone::Read(error),
        };
        let taken = daemon
            .submit(frame.clone(), Source::Client, Preconfirm::Off)
            .await;
        let verdict = Verdict::of(&super::taken(daemon, taken, frame));
        if let Err(error) = write(&mut *writer.lock().await, &ToBox::Taken { tag, verdict }).await {
            return Gone::Write(error);
        }
    }
}

pub struct Seating {
    pub seats: Vec<Seat>,
    pub lanes: Vec<Lane>,
}

pub async fn seating(client: &Client, key: &PrivateKey, status: &Status) -> Result<Seating> {
    let ask = |program: &ProgramId, query: Vec<u8>| {
        Frame::sign(
            key,
            status.network.as_bytes(),
            status.anchor(),
            None,
            program,
            query,
        )
        .encode()
    };
    let named = |name: &str| status.roles.bound(name).cloned().map_err(Error::Refused);
    let valset = named(role::VALIDATORS)?;
    let members = client
        .query(
            Layer::Confirmed,
            ask(&valset, abi::encode(&validators::Query::Members)),
        )
        .await?;
    let validators::Reply::Members(members) = abi::decode(&members).map_err(Error::Decode)? else {
        return Err(Error::Corrupt(
            "the validators program answered Members with another reply".into(),
        ));
    };
    let validators = client
        .query(
            Layer::Confirmed,
            ask(&valset, abi::encode(&validators::Query::Validators)),
        )
        .await?;
    let validators::Reply::Validators(validators) =
        abi::decode(&validators).map_err(Error::Decode)?
    else {
        return Err(Error::Corrupt(
            "the validators program answered Validators with another reply".into(),
        ));
    };
    let halo = named(role::HALO)?;
    let lanes = client
        .query(
            Layer::Confirmed,
            ask(&halo, abi::encode(&abi::role::halo::Query::Lanes)),
        )
        .await?;
    let abi::role::halo::Reply::Lanes(lanes) = abi::decode(&lanes).map_err(Error::Decode)? else {
        return Err(Error::Corrupt(
            "the halo program answered Lanes with another reply".into(),
        ));
    };
    Ok(Seating {
        seats: crate::run::seats(&members, &validators),
        lanes,
    })
}

async fn next_block(client: &Client) -> Result<()> {
    let mut blocks = client.changes(RECENT).await?;
    blocks.next().await.ok_or(Error::Closed)??;
    Ok(())
}

pub async fn serve_box<E: commonware_runtime::Metrics>(
    context: &E,
    workspace: Workspace,
    client: &Client,
    udp: std::net::UdpSocket,
) -> Result<()> {
    let key = workspace.identity()?;
    let config = halo::Config {
        egress_bytes_per_sec: None,
        udp,
        queues: halo::Queues::default(),
    };
    let halo = Halo::start(context, key.clone(), config)
        .map_err(|error| Error::Address(format!("--halo: {error}")))?;
    let station = Station::open(workspace)?;
    let streams = halo.plane().streams(LANE, 0)?;
    let mut server = Server::new(key, station);
    let mut lost: u64 = 0;
    loop {
        let status = client.status().await?;
        let listing = status
            .roles
            .bound(role::HALO)
            .cloned()
            .map_err(Error::Refused)?;
        let mut relisted = client.changes(&listing).await?;
        let seating = seating(client, &server.key, &status).await?;
        let relays: Vec<PublicKey> = seating
            .seats
            .iter()
            .filter(|seat| seat.validator)
            .map(|seat| seat.key.clone())
            .collect();
        halo.seat(seating.seats);
        halo.plane().lanes(seating.lanes);
        let order: Vec<PublicKey> = halo
            .members()
            .candidates(halo.me(), None)
            .into_iter()
            .filter(|key| relays.contains(key))
            .collect();
        let mut served = false;
        let mut unlisted = false;
        let mut unreached = Vec::new();
        for relay in order {
            let stream = match streams.open(&relay, Class::Member, &[]).await {
                Ok(stream) => stream,
                Err(error) => {
                    unlisted = matches!(error, halo::Error::NoLane(_));
                    let reason = if unlisted { "no_lane" } else { "unreachable" };
                    tracing::debug!(
                        target: TARGET,
                        event = "egress_relay_unreached",
                        %relay,
                        %error,
                        reason,
                        "the box could not open the egress lane to a relay node"
                    );
                    if unlisted {
                        break;
                    }
                    unreached.push(relay);
                    continue;
                }
            };
            let heard = server.session(stream).await;
            lost = if server.took { 1 } else { lost + 1 };
            if lost.is_power_of_two() {
                tracing::info!(
                    target: TARGET,
                    event = "egress_relay_lost",
                    %relay,
                    heard,
                    attempts = lost,
                    "the box lost its relay node; it tries the next"
                );
            }
            served |= heard;
        }
        if unlisted {
            let _ = relisted.next().await;
            continue;
        }
        drop(relisted);
        if !served && unreached.is_empty() {
            return Err(Error::Unrelayed);
        }
        let unlinked: Vec<&PublicKey> = unreached
            .iter()
            .filter(|relay| halo.linked(relay).now_or_never().is_none())
            .collect();
        if served || unlinked.is_empty() {
            next_block(client).await?;
            continue;
        }
        let linked = unlinked
            .into_iter()
            .map(|relay| Box::pin(halo.linked(relay)));
        futures::future::select_all(linked).await;
    }
}

struct Server {
    key: PrivateKey,
    station: Station,
    desk: Desk,
    role: Option<ProgramId>,
    tip: Option<Tip>,
    network: Option<String>,
    took: bool,
    answers: mpsc::UnboundedSender<(u64, Called)>,
    answered: mpsc::UnboundedReceiver<(u64, Called)>,
    calls: Calls<AbortHandle>,
    bodies: Mutex<HashMap<u64, std::result::Result<Vec<u8>, String>>>,
}

impl Server {
    fn new(key: PrivateKey, station: Station) -> Server {
        let (answers, answered) = mpsc::unbounded();
        let desk = Desk::new(station.current().config.served());
        Server {
            key,
            station,
            desk,
            role: None,
            tip: None,
            network: None,
            took: false,
            answers,
            answered,
            calls: Calls::default(),
            bodies: Mutex::new(HashMap::new()),
        }
    }

    async fn session<S: AsyncRead + AsyncWrite + Send>(&mut self, stream: S) -> bool {
        let (mut reader, writer) = tokio::io::split(stream);
        let link = Link {
            writer: tokio::sync::Mutex::new(writer),
            waiting: Mutex::new(Some(HashMap::new())),
            tags: AtomicU64::new(0),
            took: AtomicBool::new(false),
        };
        let (heard, mut hearing) = mpsc::unbounded();
        let reading = async {
            loop {
                let message = match read::<_, ToBox>(&mut reader, u32::MAX as usize).await {
                    Ok(message) => message,
                    Err(_) => break,
                };
                if let ToBox::Taken { tag, verdict } = message {
                    link.took.store(true, Ordering::Relaxed);
                    let waiter = locked(&link.waiting)
                        .as_mut()
                        .and_then(|waiting| waiting.remove(&tag));
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(verdict);
                    }
                    continue;
                }
                if heard.unbounded_send(message).is_err() {
                    break;
                }
            }
            locked(&link.waiting).take();
        };
        let mut served = false;
        let serving = async {
            loop {
                tokio::select! {
                    message = hearing.next() => {
                        let Some(message) = message else {
                            return;
                        };
                        served = true;
                        self.heard(message, &link).await;
                    }
                    Some((id, called)) = self.answered.next() => {
                        self.desk.called(id, called);
                        while let Ok((id, called)) = self.answered.try_recv() {
                            self.desk.called(id, called);
                        }
                        let steps = self.desk.launch();
                        self.steps(steps, &link).await;
                    }
                }
            }
        };
        tokio::select! {
            _ = reading => {}
            _ = serving => {}
        }
        self.took = link.took.load(Ordering::Relaxed);
        served
    }

    async fn heard<W: AsyncWrite + Unpin + Send>(&mut self, message: ToBox, link: &Link<W>) {
        match message {
            ToBox::Hello { network } => self.network = Some(network),
            ToBox::Body { id, body } => {
                locked(&self.bodies).insert(id, body);
            }
            ToBox::Read {
                tip,
                role,
                assigned,
                serving,
                member,
            } => {
                if self.role.as_ref() != Some(&role) {
                    let served = self.station.current().config.served();
                    self.calls.renew(&mut self.desk, served);
                    self.role = Some(role);
                }
                self.tip = Some(tip);
                let steps = self.desk.read(Reading {
                    assigned,
                    serving,
                    member,
                });
                self.steps(steps, link).await;
                locked(&self.bodies).clear();
            }
            ToBox::Unbound => {
                if self.role.take().is_some() {
                    let served = self.station.current().config.served();
                    self.calls.renew(&mut self.desk, served);
                }
            }
            ToBox::Taken { .. } => {}
        }
    }

    async fn steps<W: AsyncWrite + Unpin + Send>(&mut self, steps: Vec<Step>, link: &Link<W>) {
        let (Some(role), Some(tip), Some(network)) = (&self.role, self.tip, &self.network) else {
            return;
        };
        let remote = Remote {
            key: &self.key,
            network,
            tip,
            serving: self.station.current(),
            answers: &self.answers,
            bodies: &self.bodies,
            link,
        };
        for step in steps {
            execute(&remote, &mut self.desk, &mut self.calls, step, role).await;
        }
    }
}

struct Link<W> {
    writer: tokio::sync::Mutex<W>,
    waiting: Mutex<Option<HashMap<u64, oneshot::Sender<Verdict>>>>,
    tags: AtomicU64,
    took: AtomicBool,
}

struct Remote<'a, W> {
    key: &'a PrivateKey,
    network: &'a str,
    tip: Tip,
    serving: Arc<Serving>,
    answers: &'a mpsc::UnboundedSender<(u64, Called)>,
    bodies: &'a Mutex<HashMap<u64, std::result::Result<Vec<u8>, String>>>,
    link: &'a Link<W>,
}

impl<W: AsyncWrite + Unpin + Send> Window for Remote<'_, W> {
    type Call = AbortHandle;

    fn call(&self, row: RequestRow) -> AbortHandle {
        let body = locked(self.bodies).remove(&row.id);
        let serving = self.serving.clone();
        let answers = self.answers.clone();
        let network = self.network.to_owned();
        let call = tokio::spawn(async move {
            let id = row.id;
            let event = |data: Vec<u8>| {
                let _ = answers.unbounded_send((id, Called::Event(data)));
            };
            let called = match body {
                Some(Ok(body)) => {
                    let caller = Caller {
                        chain: &network,
                        program: &row.requester,
                        id,
                    };
                    let request = &row.request;
                    let delivery = &row.delivery;
                    let unretried = &mut call::Unretried;
                    call::answer(
                        serving.as_ref(),
                        &caller,
                        request,
                        delivery,
                        body,
                        unretried,
                        event,
                    )
                    .await
                }
                Some(Err(reason)) => Called::Whole(Answer::Failed(reason)),
                None => Called::Whole(Answer::Failed(
                    "the relay node sent no body for this request".into(),
                )),
            };
            let _ = answers.unbounded_send((id, called));
        });
        call.abort_handle()
    }

    fn sign(&self, op: &Op, role: &ProgramId) -> Result<Vec<u8>> {
        let frame = Frame::sign(
            self.key,
            self.network.as_bytes(),
            self.tip,
            None,
            role,
            abi::encode(op),
        );
        Ok(frame.encode())
    }

    async fn submit(&self, frame: Vec<u8>) -> Sent {
        if let Err(refusal) = node::admissible(frame.clone(), self.network.as_bytes()) {
            return Sent::Refused(refusal);
        }
        let tag = self.link.tags.fetch_add(1, Ordering::Relaxed);
        let (waiter, verdict) = oneshot::channel();
        {
            let mut waiting = locked(&self.link.waiting);
            let Some(waiting) = waiting.as_mut() else {
                return Sent::Untaken;
            };
            waiting.insert(tag, waiter);
        }
        let message = ToNode::Frame {
            tag,
            frame: frame.clone(),
        };
        let written = write(&mut *self.link.writer.lock().await, &message).await;
        if written.is_err() {
            return Sent::Untaken;
        }
        match verdict.await {
            Ok(verdict) => verdict.sent(frame),
            Err(_) => Sent::Untaken,
        }
    }
}

#[cfg(test)]
mod tests;
