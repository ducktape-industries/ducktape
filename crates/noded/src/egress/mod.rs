//! The node's egress worker: after each applied block it reads the program bound to the egress
//! role and answers and serves through it with the node's key. Spawned by `run.rs`.
mod call;
mod calls;
mod config;
mod desk;
pub mod lane;
mod station;
mod stream;

use std::sync::Arc;

use abi::role::egress::{Answer, Op, Query, Reply, RequestRow};
use abi::role::scheduler;
use abi::{BlobId, Origin, ProgramId, Refusal, reason, role};
use commonware_runtime::Handle;
use futures::StreamExt as _;
use futures::channel::mpsc;
use host::{Layer, Tip};
use node::{Frame, Preconfirm, Source, Taken};
use tokio::sync::watch;

pub use config::{Config, Refused, Withheld, withheld};
pub use station::{Planned, Station, Tally};

use crate::egress::call::Caller;
use crate::egress::calls::{Call, Calls};
use crate::egress::desk::{Called, Desk, Purpose, Reading, Sent, Step};
use crate::{Context, Daemon, Error, Result};

const TARGET: &str = "ducktape::egress";

struct Worker<E: Context> {
    daemon: Arc<Daemon<E>>,
    answers: mpsc::UnboundedSender<(u64, Called)>,
}

pub fn spawn<E: Context>(daemon: Arc<Daemon<E>>) -> Handle<()> {
    daemon.context.child("egress").spawn(move |_| work(daemon))
}

async fn work<E: Context>(daemon: Arc<Daemon<E>>) {
    let (answers, mut answered) = mpsc::unbounded();
    let mut reloads = daemon.egress.subscribe();
    let mut desk = Desk::new(reloads.borrow_and_update().config.served());
    let worker = Worker {
        daemon: daemon.clone(),
        answers,
    };
    let mut calls = Calls::default();
    let mut tips = daemon.tips.subscribe();
    let mut unread: u64 = 0;
    let mut bound = Bound::Unread;
    loop {
        tokio::select! {
            changed = tips.changed() => {
                if changed.is_err() {
                    return;
                }
                let read = match read_for(&daemon, daemon.identity.clone()).await {
                    Ok(read) => read,
                    Err(error) => {
                        unread += 1;
                        if unread.is_power_of_two() {
                            tracing::warn!(
                                target: TARGET,
                                event = "egress_unread",
                                attempts = unread,
                                reason = "egress_query_failed",
                                %error,
                                "the egress worker could not read the egress program"
                            );
                        }
                        continue;
                    }
                };
                unread = 0;
                let (role, reading) = match read {
                    Some((role, _, reading)) => (role, reading),
                    None => {
                        let serving = daemon.egress.current();
                        if bound.unbound(&serving.config) {
                            calls.renew(&mut desk, serving.config.served());
                        }
                        continue;
                    }
                };
                if bound.bound(&role) {
                    calls.renew(&mut desk, daemon.egress.current().config.served());
                }
                for step in desk.read(reading) {
                    execute(&worker, &mut desk, &mut calls, step, &role).await;
                }
            }
            Some((id, called)) = answered.next() => {
                let Bound::To(role) = &bound else {
                    continue;
                };
                desk.called(id, called);
                while let Ok((id, called)) = answered.try_recv() {
                    desk.called(id, called);
                }
                for step in desk.launch() {
                    execute(&worker, &mut desk, &mut calls, step, role).await;
                }
            }
            reloaded = reloads.changed() => {
                if reloaded.is_err() {
                    return;
                }
                desk.want(reloads.borrow_and_update().config.served());
            }
        }
    }
}

enum Bound {
    Unread,
    Unbound,
    To(ProgramId),
}

impl Bound {
    /// Records that the tip binds no egress role; true when that is a move, which empties the
    /// desk.
    fn unbound(&mut self, config: &Config) -> bool {
        match std::mem::replace(self, Bound::Unbound) {
            Bound::Unread => {
                let unused = !config.served().is_empty();
                if unused {
                    tracing::warn!(
                        target: TARGET,
                        event = "egress_unbound",
                        reason = "no_egress_role",
                        "this node's egress.toml is unused: the network binds no egress role"
                    );
                }
                true
            }
            Bound::To(was) => {
                tracing::warn!(
                    target: TARGET,
                    event = "egress_unbound",
                    reason = "unbound_by_vote",
                    was = %was,
                    "the network unbound its egress role; this node answers no more requests"
                );
                true
            }
            Bound::Unbound => false,
        }
    }

    /// Records that the tip binds `role`; true when that is a move, which empties the desk.
    fn bound(&mut self, role: &ProgramId) -> bool {
        let same = matches!(self, Bound::To(was) if was == role);
        if same {
            return false;
        }
        tracing::info!(
            target: TARGET,
            event = "egress_bound",
            role = %role,
            "the network binds its egress role; this node answers through it"
        );
        *self = Bound::To(role.clone());
        true
    }
}

fn role<E: Context>(source: &host::Source<'_, E>) -> Option<ProgramId> {
    source.roles().get(role::EGRESS).cloned()
}

async fn ask<E: Context>(
    daemon: &Daemon<E>,
    source: &host::Source<'_, E>,
    role: &ProgramId,
    query: &Query,
) -> Result<Reply> {
    let now = daemon.now();
    let answer = source
        .query(
            Layer::Confirmed,
            now,
            Origin::System,
            role,
            abi::encode(query),
        )
        .await?
        .map_err(Error::Refused)?;
    abi::decode(&answer).map_err(Error::Decode)
}

async fn requests<E: Context>(
    daemon: &Daemon<E>,
    source: &host::Source<'_, E>,
    role: &ProgramId,
    query: &Query,
) -> Result<Vec<RequestRow>> {
    match ask(daemon, source, role, query).await? {
        Reply::Requests(rows) => Ok(rows),
        Reply::Node(_) | Reply::Serving(_) => Err(Error::Corrupt(
            "the egress program answered a request listing with something else".into(),
        )),
    }
}

async fn read_for<E: Context>(
    daemon: &Daemon<E>,
    key: Vec<u8>,
) -> Result<Option<(ProgramId, Tip, Reading)>> {
    let view = daemon.view()?;
    let source = view.source();
    let Some(role) = role(&source) else {
        return Ok(None);
    };
    let assigned = requests(
        daemon,
        &source,
        &role,
        &Query::Assigned { key: key.clone() },
    )
    .await?;
    let Reply::Node(row) = ask(daemon, &source, &role, &Query::Node { key: key.clone() }).await?
    else {
        return Err(Error::Corrupt(
            "the egress program answered a node with something else".into(),
        ));
    };
    let epoch = daemon.network.epoch_after(view.tip().height);
    let member = source
        .epoch_members(epoch)?
        .is_some_and(|members| members.iter().any(|member| member.key == key));
    let mut serving = row.map(|row| row.services).unwrap_or_default();
    serving.sort();
    let reading = Reading {
        assigned,
        serving,
        member,
    };
    Ok(Some((role, view.tip(), reading)))
}

trait Window {
    type Call: Call;
    fn call(&self, row: RequestRow) -> Self::Call;
    fn sign(&self, op: &Op, role: &ProgramId) -> Result<Vec<u8>>;
    fn submit(&self, frame: Vec<u8>) -> impl Future<Output = Sent> + Send;
}

async fn execute<W: Window>(
    window: &W,
    desk: &mut Desk,
    calls: &mut Calls<W::Call>,
    step: Step,
    role: &ProgramId,
) {
    let mut next = Some(step);
    while let Some(step) = next {
        next = match step {
            Step::Call(row) => {
                let id = row.id;
                calls.started(id, window.call(row));
                None
            }
            Step::Abandon(id) => {
                if calls.abandon(id) {
                    tracing::debug!(target: TARGET, event = "egress_abandoned", id);
                }
                None
            }
            Step::Send(purpose) => send(window, desk, purpose, role).await,
        };
    }
}

async fn send(
    window: &impl Window,
    desk: &mut Desk,
    purpose: Purpose,
    role: &ProgramId,
) -> Option<Step> {
    let frame = match desk.frame(&purpose) {
        Some(frame) => frame.to_vec(),
        None => match window.sign(&desk.op(&purpose)?, role) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::debug!(
                    target: TARGET,
                    event = "egress_unsigned",
                    ?purpose,
                    reason = "node_stopped",
                    %error,
                    "the egress worker could not anchor a frame"
                );
                return None;
            }
        },
    };
    let sent = window.submit(frame).await;
    tracing::trace!(
        target: TARGET,
        event = "egress_submitted",
        ?purpose,
        sent = sent.kind(),
    );
    if let Sent::Refused(refusal) = &sent {
        tracing::warn!(
            target: TARGET,
            event = "egress_refused",
            ?purpose,
            reason = %refusal.reason,
            sentence = %refusal.sentence,
            "the node refused an egress frame of its own"
        );
    }
    desk.sent(&purpose, sent)
}

impl<E: Context> Window for Worker<E> {
    type Call = Handle<()>;

    fn call(&self, row: RequestRow) -> Handle<()> {
        let daemon = self.daemon.clone();
        let serving = self.daemon.egress.current();
        let answers = self.answers.clone();
        tracing::debug!(
            target: TARGET,
            event = "egress_calling",
            id = row.id,
            service = row.request.service(),
        );
        self.daemon
            .context
            .child("egress_call")
            .spawn(move |_| async move {
                let id = row.id;
                let event = |data: Vec<u8>| {
                    let _ = answers.unbounded_send((id, Called::Event(data)));
                };
                let called = match body(&daemon, row.request.parts()).await {
                    Ok(body) => {
                        let caller = Caller {
                            chain: &daemon.descriptor.network,
                            program: &row.requester,
                            id,
                        };
                        let request = &row.request;
                        let delivery = &row.delivery;
                        let mut blocks = Until {
                            tips: daemon.tips.subscribe(),
                            daemon: daemon.clone(),
                            expiry: row.expiry,
                        };
                        let blocks = &mut blocks;
                        call::answer(
                            serving.as_ref(),
                            &caller,
                            request,
                            delivery,
                            body,
                            blocks,
                            event,
                        )
                        .await
                    }
                    Err(reason) => Called::Whole(Answer::Failed(reason)),
                };
                tracing::debug!(
                    target: TARGET,
                    event = "egress_called",
                    id,
                    ending = called.ending(),
                );
                let _ = answers.unbounded_send((id, called));
            })
    }

    fn sign(&self, op: &Op, role: &ProgramId) -> Result<Vec<u8>> {
        let view = self.daemon.view()?;
        let frame = Frame::sign(
            &self.daemon.key,
            &self.daemon.descriptor.id(),
            view.tip(),
            None,
            role,
            abi::encode(op),
        );
        Ok(frame.encode())
    }

    async fn submit(&self, frame: Vec<u8>) -> Sent {
        let asked = self.daemon.context.current();
        let submitted = self
            .daemon
            .submit(frame.clone(), Source::Client, Preconfirm::Off)
            .await;
        let waited = self
            .daemon
            .context
            .current()
            .duration_since(asked)
            .unwrap_or_default();
        let sent = taken(&self.daemon, submitted, frame);
        tracing::trace!(
            target: TARGET,
            event = "egress_pooled",
            sent = sent.kind(),
            waited_ms = waited.as_millis() as u64,
        );
        sent
    }
}

struct Until<E: Context> {
    tips: watch::Receiver<u64>,
    daemon: Arc<Daemon<E>>,
    expiry: scheduler::EntryId,
}

impl<E: Context> call::Blocks for Until<E> {
    fn tip(&mut self) -> u64 {
        *self.tips.borrow_and_update()
    }

    async fn expiry(&mut self) -> Option<u64> {
        let view = self.daemon.view().ok()?;
        let source = view.source();
        let scheduler = source.roles().get(role::SCHEDULER).cloned()?;
        let query = abi::encode(&scheduler::Query::Entry { id: self.expiry });
        let now = self.daemon.now();
        let answer = source
            .query(Layer::Confirmed, now, Origin::System, &scheduler, query)
            .await
            .ok()?
            .ok()?;
        match abi::decode(&answer).ok()? {
            scheduler::Reply::Entry(entry) => entry.map(|entry| entry.height),
            scheduler::Reply::Due { .. } => None,
        }
    }

    async fn changed(&mut self) -> bool {
        self.tips.changed().await.is_ok()
    }
}

fn taken<E: Context>(
    daemon: &Daemon<E>,
    taken: Result<std::result::Result<Taken, Refusal>>,
    frame: Vec<u8>,
) -> Sent {
    match taken {
        Ok(Ok(Taken::New(_))) => {
            daemon.relay.send([frame.as_slice()]);
            Sent::Pending(frame)
        }
        Ok(Ok(Taken::Held(_))) => Sent::Pending(frame),
        Ok(Err(refusal)) => match refusal.reason.as_str() {
            reason::ANCHOR => Sent::Stale,
            reason::DUPLICATE => Sent::Carried,
            _ => Sent::Refused(refusal),
        },
        Err(error) => {
            tracing::warn!(
                target: TARGET,
                event = "egress_unsubmitted",
                reason = "node_failed",
                %error,
                "the node failed to take an egress frame; it is sent again next block"
            );
            Sent::Untaken
        }
    }
}

async fn body<E: Context>(
    daemon: &Daemon<E>,
    parts: &[BlobId],
) -> std::result::Result<Vec<u8>, String> {
    for part in parts {
        if !daemon.blobs.store().has(part) {
            daemon.blobs.fetch(*part, halo::Class::Client).await;
        }
    }
    stored(daemon, parts)
}

fn stored<E: Context>(
    daemon: &Daemon<E>,
    parts: &[BlobId],
) -> std::result::Result<Vec<u8>, String> {
    let mut body = Vec::new();
    for part in parts {
        let blob = daemon
            .blobs
            .store()
            .get(part)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("part {} is not on this node", abi::hex(part.digest())))?;
        body.extend(blob.body);
    }
    Ok(body)
}
