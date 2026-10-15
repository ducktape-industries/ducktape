//! The node's egress worker: after each applied block it reads the program bound to the egress
//! role and answers and serves through it with the node's key. Spawned by `run.rs`.
mod call;
mod config;
mod desk;

use std::sync::Arc;

use abi::role::egress::{Answer, Op, Query, Reply, RequestRow};
use abi::{BlobId, Origin, ProgramId, reason, role};
use commonware_runtime::Handle;
use futures::StreamExt as _;
use futures::channel::mpsc;
use host::Layer;
use node::{Frame, Node, Preconfirm, Source, Taken};

pub use call::Clients;
pub use config::Config;

use crate::egress::call::Caller;
use crate::egress::desk::{Desk, Purpose, Reading, Sent, Step};
use crate::{Context, Daemon, Error, Result};

const TARGET: &str = "ducktape::egress";

struct Worker<E: Context> {
    daemon: Arc<Daemon<E>>,
    config: Arc<Config>,
    clients: Arc<Clients>,
    answers: mpsc::UnboundedSender<(u64, Answer)>,
}

pub fn spawn<E: Context>(daemon: Arc<Daemon<E>>, config: Config, clients: Clients) -> Handle<()> {
    daemon
        .context
        .child("egress")
        .spawn(move |_| work(daemon, config, clients))
}

async fn work<E: Context>(daemon: Arc<Daemon<E>>, config: Config, clients: Clients) {
    let (answers, mut answered) = mpsc::unbounded();
    let mut desk = Desk::new(config.served());
    let worker = Worker {
        daemon: daemon.clone(),
        config: Arc::new(config),
        clients: Arc::new(clients),
        answers,
    };
    let mut tips = daemon.tips.subscribe();
    let mut unread: u64 = 0;
    // the egress role at the last tip read: a vote binds, rebinds or unbinds it
    let mut bound: Option<ProgramId> = None;
    loop {
        tokio::select! {
            changed = tips.changed() => {
                if changed.is_err() {
                    return;
                }
                let read = match worker.read().await {
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
                    Some(read) => read,
                    None => {
                        if bound.take().is_some() || !worker.config.served().is_empty() {
                            tracing::warn!(
                                target: TARGET,
                                event = "egress_unbound",
                                reason = "no_egress_role",
                                "this node's egress.toml is unused: the network binds no egress role"
                            );
                        }
                        desk = Desk::new(worker.config.served());
                        continue;
                    }
                };
                let rebound = bound.as_ref() != Some(&role);
                if rebound {
                    tracing::info!(target: TARGET, event = "egress_bound", role = %role);
                    desk = Desk::new(worker.config.served());
                    bound = Some(role.clone());
                }
                for step in desk.read(reading) {
                    worker.execute(&mut desk, step, &role).await;
                }
            }
            Some((id, answer)) = answered.next() => {
                let Some(role) = &bound else {
                    continue;
                };
                if let Some(step) = desk.answered(id, answer) {
                    worker.execute(&mut desk, step, role).await;
                }
            }
        }
    }
}

impl<E: Context> Worker<E> {
    /// The egress role at the node's tip, if the network binds one.
    fn role(node: &Node<E>) -> Option<ProgramId> {
        node.host().roles().get(role::EGRESS).cloned()
    }

    async fn ask(&self, node: &Node<E>, role: &ProgramId, query: &Query) -> Result<Reply> {
        let answer = node
            .query(Layer::Confirmed, Origin::System, role, abi::encode(query))
            .await?
            .map_err(Error::Refused)?;
        abi::decode(&answer).map_err(Error::Decode)
    }

    async fn requests(
        &self,
        node: &Node<E>,
        role: &ProgramId,
        query: &Query,
    ) -> Result<Vec<RequestRow>> {
        match self.ask(node, role, query).await? {
            Reply::Requests(rows) => Ok(rows),
            Reply::Node(_) | Reply::Serving(_) => Err(Error::Corrupt(
                "the egress program answered a request listing with something else".into(),
            )),
        }
    }

    /// The egress role and what it holds for this node at the tip; `None` when the network
    /// binds no egress role there.
    async fn read(&self) -> Result<Option<(ProgramId, Reading)>> {
        let node = self.daemon.node.lock().await;
        let Some(role) = Self::role(&node) else {
            return Ok(None);
        };
        let key = self.daemon.identity.clone();
        let assigned = self
            .requests(&node, &role, &Query::Assigned { key: key.clone() })
            .await?;
        let Reply::Node(row) = self
            .ask(&node, &role, &Query::Node { key: key.clone() })
            .await?
        else {
            return Err(Error::Corrupt(
                "the egress program answered a node with something else".into(),
            ));
        };
        let epoch = self.daemon.network.epoch_after(node.tip()?.height);
        let member = node
            .epoch_members(epoch)?
            .is_some_and(|members| members.iter().any(|member| member.key == key));
        let mut serving = row.map(|row| row.services).unwrap_or_default();
        serving.sort();
        let reading = Reading {
            assigned,
            serving,
            member,
        };
        Ok(Some((role, reading)))
    }

    async fn execute(&self, desk: &mut Desk, step: Step, role: &ProgramId) {
        match step {
            Step::Call(row) => self.call(row),
            Step::Send(purpose) => self.send(desk, purpose, role).await,
        }
    }

    fn call(&self, row: RequestRow) {
        let daemon = self.daemon.clone();
        let config = self.config.clone();
        let clients = self.clients.clone();
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
                let answer = match body(&daemon, row.request.parts()).await {
                    Ok(body) => {
                        let caller = Caller {
                            chain: &daemon.descriptor.network,
                            program: &row.requester,
                            id: row.id,
                        };
                        call::answer(&clients, &config, &caller, &row.request, body).await
                    }
                    Err(reason) => Answer::Failed(reason),
                };
                let _ = answers.unbounded_send((row.id, answer));
            });
    }

    async fn send(&self, desk: &mut Desk, purpose: Purpose, role: &ProgramId) {
        let frame = match desk.frame(&purpose) {
            Some(frame) => frame.to_vec(),
            None => {
                let Some(op) = desk.op(&purpose) else {
                    return;
                };
                match self.sign(&op, role).await {
                    Ok(frame) => frame,
                    Err(error) => {
                        tracing::warn!(
                            target: TARGET,
                            event = "egress_unsigned",
                            ?purpose,
                            reason = "tip_unread",
                            %error,
                            "the egress worker could not anchor a frame; it tries again next block"
                        );
                        return;
                    }
                }
            }
        };
        let sent = self.submit(frame).await;
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
        desk.sent(&purpose, sent);
    }

    async fn sign(&self, op: &Op, role: &ProgramId) -> Result<Vec<u8>> {
        let tip = self.daemon.node.lock().await.tip()?;
        let frame = Frame::sign(
            &self.daemon.key,
            &self.daemon.descriptor.id(),
            tip,
            None,
            role,
            abi::encode(op),
        );
        Ok(frame.encode())
    }

    async fn submit(&self, frame: Vec<u8>) -> Sent {
        let mut node = self.daemon.node.lock().await;
        let taken = node
            .submit(frame.clone(), Source::Client, Preconfirm::Off)
            .await;
        drop(node);
        match taken {
            Ok(Ok(Taken::New(_))) => {
                self.daemon.relay.send([frame.as_slice()]);
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
                    "the node failed to take an egress frame; it is signed again next block"
                );
                Sent::Stale
            }
        }
    }
}

async fn body<E: Context>(
    daemon: &Daemon<E>,
    parts: &[BlobId],
) -> std::result::Result<Vec<u8>, String> {
    let mut body = Vec::new();
    for part in parts {
        if !daemon.blobs.store().has(part) {
            daemon.blobs.fetch(*part, halo::Class::Client).await;
        }
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
