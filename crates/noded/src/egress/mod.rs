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
    role: ProgramId,
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
    let bound = daemon
        .node
        .lock()
        .await
        .host()
        .roles()
        .get(role::EGRESS)
        .cloned();
    let Some(role) = bound else {
        let unused = !config.served().is_empty();
        if unused {
            tracing::warn!(
                target: TARGET,
                event = "egress_unbound",
                reason = "no_egress_role",
                "this node's egress.toml is unused: the network binds no egress role"
            );
        }
        return;
    };
    let (answers, mut answered) = mpsc::unbounded();
    let mut desk = Desk::new(config.served());
    let worker = Worker {
        daemon: daemon.clone(),
        role,
        config: Arc::new(config),
        clients: Arc::new(clients),
        answers,
    };
    let mut tips = daemon.tips.subscribe();
    let mut unread: u64 = 0;
    loop {
        tokio::select! {
            changed = tips.changed() => {
                if changed.is_err() {
                    return;
                }
                let reading = match worker.read().await {
                    Ok(reading) => reading,
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
                for step in desk.read(reading) {
                    worker.execute(&mut desk, step).await;
                }
            }
            Some((id, answer)) = answered.next() => {
                if let Some(step) = desk.answered(id, answer) {
                    worker.execute(&mut desk, step).await;
                }
            }
        }
    }
}

impl<E: Context> Worker<E> {
    async fn ask(&self, node: &Node<E>, query: &Query) -> Result<Reply> {
        let answer = node
            .query(
                Layer::Confirmed,
                Origin::System,
                &self.role,
                abi::encode(query),
            )
            .await?
            .map_err(Error::Refused)?;
        abi::decode(&answer).map_err(Error::Decode)
    }

    async fn requests(&self, node: &Node<E>, query: &Query) -> Result<Vec<RequestRow>> {
        match self.ask(node, query).await? {
            Reply::Requests(rows) => Ok(rows),
            Reply::Node(_) | Reply::Serving(_) => Err(Error::Corrupt(
                "the egress program answered a request listing with something else".into(),
            )),
        }
    }

    async fn read(&self) -> Result<Reading> {
        let node = self.daemon.node.lock().await;
        let key = self.daemon.identity.clone();
        let assigned = self
            .requests(&node, &Query::Assigned { key: key.clone() })
            .await?;
        let Reply::Node(row) = self.ask(&node, &Query::Node { key: key.clone() }).await? else {
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
        Ok(Reading {
            assigned,
            serving,
            member,
        })
    }

    async fn execute(&self, desk: &mut Desk, step: Step) {
        match step {
            Step::Call(row) => self.call(row),
            Step::Send(purpose) => self.send(desk, purpose).await,
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

    async fn send(&self, desk: &mut Desk, purpose: Purpose) {
        let frame = match desk.frame(&purpose) {
            Some(frame) => frame.to_vec(),
            None => {
                let Some(op) = desk.op(&purpose) else {
                    return;
                };
                match self.sign(&op).await {
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

    async fn sign(&self, op: &Op) -> Result<Vec<u8>> {
        let tip = self.daemon.node.lock().await.tip()?;
        let frame = Frame::sign(
            &self.daemon.key,
            &self.daemon.descriptor.id(),
            tip,
            None,
            &self.role,
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
