//! The node's reads worker: after each applied block it answers the `$reads` rows the epoch
//! assigns to this node's key, each query run off the block path and its reply put as a blob, with
//! `Answer`, then `Ready` once the blobs role holds that blob. Spawned by `run.rs`.
mod preparation;

use std::collections::BTreeMap;
use std::sync::Arc;

use abi::role::reads::{self, Entry, Op, Row, State};
use abi::{Origin, Refusal, reason};
use commonware_runtime::Handle;
use futures::StreamExt as _;
use futures::channel::mpsc;
use host::{ANCHOR_WINDOW, Layer};
use node::{Frame, Preconfirm, Source, Taken};

use crate::{Context, Daemon, Error, Result, offload};

const TARGET: &str = "ducktape::reads";

type Answered = (u64, Option<(Op, Option<Framed>)>);

struct Framed {
    entries: Vec<blob::contract::Entry>,
    framed: Vec<u8>,
}

#[derive(Default)]
struct Slot {
    asking: Option<Handle<()>>,
    op: Option<Op>,
    reply: Option<Framed>,
    frame: Option<Vec<u8>>,
    refused: bool,
}

pub fn spawn<E: Context>(daemon: Arc<Daemon<E>>) -> Handle<()> {
    daemon.context.child("reads").spawn(move |_| work(daemon))
}

async fn work<E: Context>(daemon: Arc<Daemon<E>>) {
    let (answers, mut answered) = mpsc::unbounded();
    let mut tips = daemon.tips.subscribe();
    let mut slots: BTreeMap<u64, Slot> = BTreeMap::new();
    let mut unread: u64 = 0;
    loop {
        tokio::select! {
            changed = tips.changed() => match changed {
                Ok(()) => tipped(&daemon, &answers, &mut slots, &mut unread).await,
                Err(_) => return,
            },
            Some(heard) = answered.next() => took(&daemon, &mut slots, heard).await,
        }
    }
}

async fn tipped<E: Context>(
    daemon: &Arc<Daemon<E>>,
    answers: &mpsc::UnboundedSender<Answered>,
    slots: &mut BTreeMap<u64, Slot>,
    unread: &mut u64,
) {
    let Err(error) = pass(daemon, answers, slots).await else {
        *unread = 0;
        return;
    };
    *unread += 1;
    if unread.is_power_of_two() {
        tracing::warn!(
            target: TARGET,
            event = "reads_unread",
            attempts = *unread,
            reason = "reads_query_failed",
            %error,
            "the reads worker could not read the open reads"
        );
    }
}

async fn took<E: Context>(daemon: &Daemon<E>, slots: &mut BTreeMap<u64, Slot>, heard: Answered) {
    let (id, answer) = heard;
    let Some(slot) = slots.get_mut(&id) else {
        return;
    };
    slot.asking = None;
    let (op, reply) = answer.unzip();
    slot.op = op;
    slot.reply = reply.flatten();
    slot.frame = None;
    send(daemon, id, slot).await;
}

async fn pass<E: Context>(
    daemon: &Arc<Daemon<E>>,
    answers: &mpsc::UnboundedSender<Answered>,
    slots: &mut BTreeMap<u64, Slot>,
) -> Result<()> {
    let view = daemon.view()?;
    let source = view.source();
    let next = view.tip().height + 1;
    let mut mine = BTreeMap::new();
    for (id, row) in source.reads()? {
        let assigned = source.reader(id, next)?;
        if assigned.as_deref() == Some(daemon.identity.as_slice()) {
            mine.insert(id, row);
        }
    }
    let mut preparations = BTreeMap::new();
    for (id, row) in source.preparations()? {
        let assigned = source.reader(id, next)?;
        if assigned.as_deref() == Some(daemon.identity.as_slice()) {
            preparations.insert(id, row);
        }
    }
    slots.retain(|id, slot| {
        let open = mine.contains_key(id) || preparations.contains_key(id);
        if let (false, Some(asking)) = (open, &slot.asking) {
            asking.abort();
        }
        open
    });
    let now = daemon.now();
    for (id, row) in mine {
        let slot = slots.entry(id).or_default();
        match row.state.clone() {
            State::Asked => {
                let idle = slot.asking.is_none() && slot.op.is_none();
                if idle {
                    slot.asking = Some(ask(daemon, id, row, answers.clone()));
                }
            }
            State::Putting { read, .. } => {
                let readied = matches!(slot.op, Some(Op::Ready { .. }));
                if !readied {
                    if !source.holds(&read.entry.id, now).await? {
                        continue;
                    }
                    slot.op = Some(Op::Ready { id });
                    slot.reply = None;
                    slot.frame = None;
                    slot.refused = false;
                }
            }
        }
        send(daemon, id, slot).await;
    }
    for (id, row) in preparations {
        let slot = slots.entry(id).or_default();
        match row.state.clone() {
            abi::role::reads::PreparationState::Asked => {
                let idle = slot.asking.is_none() && slot.op.is_none();
                if idle {
                    slot.asking = Some(preparation::ask(daemon, id, row, answers.clone()));
                }
            }
            abi::role::reads::PreparationState::Putting { bundle, .. } => {
                let readied = matches!(slot.op, Some(Op::Ready { .. }));
                if !readied {
                    if !source.preparation_held(bundle, now).await? {
                        continue;
                    }
                    slot.op = Some(Op::Ready { id });
                    slot.reply = None;
                    slot.frame = None;
                    slot.refused = false;
                }
            }
        }
        send(daemon, id, slot).await;
    }
    Ok(())
}

fn ask<E: Context>(
    daemon: &Arc<Daemon<E>>,
    id: u64,
    row: Row,
    answers: mpsc::UnboundedSender<Answered>,
) -> Handle<()> {
    tracing::debug!(target: TARGET, event = "read_asked", id, program = %row.target);
    let reader = daemon.clone();
    daemon.context.child("read").spawn(move |_| async move {
        let answered = match answer(&reader, row).await {
            Ok((at, outcome, reply)) => Some((Op::Answer { id, at, outcome }, reply)),
            Err(error) => {
                tracing::warn!(
                    target: TARGET,
                    event = "read_unrun",
                    id,
                    reason = "node_failed",
                    %error,
                    "the node could not run a read; it asks again next block"
                );
                None
            }
        };
        let _ = answers.unbounded_send((id, answered));
    })
}

async fn answer<E: Context>(
    daemon: &Daemon<E>,
    row: Row,
) -> Result<(u64, std::result::Result<Entry, Refusal>, Option<Framed>)> {
    let view = daemon.view()?;
    let at = view.tip().height;
    let time = daemon.now();
    let Row {
        requester,
        target,
        query,
        ..
    } = row;
    let program = target.clone();
    let queried = offload(move || {
        let source = view.source();
        let origin = Origin::Program(requester);
        tokio::runtime::Handle::current().block_on(source.queried(
            Layer::Confirmed,
            time,
            origin,
            &program,
            query,
        ))
    })
    .await??;
    let _ = daemon.queried.send(crate::census::Queried {
        program: target,
        fuel: queried.fuel,
        ran_out: queried.ran_out,
    });
    let reply = match queried.answer {
        Ok(reply) => reply,
        Err(refusal) => return Ok((at, Err(refusal), None)),
    };
    let framed = match blob::frame(reads::KIND, &reply) {
        Ok(framed) => framed,
        Err(refusal) => return Ok((at, Err(refusal), None)),
    };
    let entry = blob::contract::Entry::of(&framed)
        .ok_or_else(|| Error::Corrupt("a framed reply names no blob".into()))?;
    let read = Entry {
        id: entry.id,
        len: entry.len,
        kind: entry.kind.clone(),
    };
    Ok((
        at,
        Ok(read),
        Some(Framed {
            entries: vec![entry],
            framed,
        }),
    ))
}

fn sign<E: Context>(daemon: &Daemon<E>, op: &Op) -> Result<(Vec<u8>, u64)> {
    let view = daemon.view()?;
    let anchor = view.tip();
    let frame = Frame::sign(
        &daemon.key,
        &daemon.descriptor.id(),
        anchor,
        None,
        reads::PROGRAM,
        abi::encode(op),
    );
    let last_landing = anchor.height.saturating_add(ANCHOR_WINDOW);
    Ok((frame.encode(), last_landing))
}

async fn staged<E: Context>(
    daemon: &Daemon<E>,
    reply: Option<&Framed>,
    last_landing: u64,
) -> std::result::Result<Option<blob::service::Staged>, Refusal> {
    let Some(Framed { entries, framed }) = reply else {
        return Ok(None);
    };
    let staged = daemon
        .blobs
        .stage(entries, &mut framed.as_slice(), last_landing)
        .await?;
    Ok(Some(staged))
}

fn unstage(staged: Option<blob::service::Staged>) {
    if let Some(staged) = staged {
        staged.refused();
    }
}

async fn send<E: Context>(daemon: &Daemon<E>, id: u64, slot: &mut Slot) {
    let Some(op) = &slot.op else {
        return;
    };
    if slot.refused {
        return;
    }
    let (frame, staged) = match slot.frame.take() {
        Some(frame) => (frame, None),
        None => {
            let (frame, last_landing) = match sign(daemon, op) {
                Ok(signed) => signed,
                Err(error) => {
                    tracing::debug!(
                        target: TARGET,
                        event = "read_unsigned",
                        id,
                        reason = "node_stopped",
                        %error,
                        "the reads worker could not anchor a frame"
                    );
                    return;
                }
            };
            match staged(daemon, slot.reply.as_ref(), last_landing).await {
                Ok(staged) => (frame, staged),
                Err(refusal) => {
                    tracing::warn!(
                        target: TARGET,
                        event = "read_unstaged",
                        id,
                        reason = %refusal.reason,
                        sentence = %refusal.sentence,
                        "the node could not stage a read's reply; it asks again next block"
                    );
                    slot.op = None;
                    slot.reply = None;
                    return;
                }
            }
        }
    };
    let taken = daemon
        .submit(frame.clone(), Source::Client, Preconfirm::Off)
        .await;
    slot.frame = match taken {
        Ok(Ok(Taken::New(_))) => {
            daemon.relay.send([frame.as_slice()]);
            Some(frame)
        }
        Ok(Ok(Taken::Held(_))) => Some(frame),
        Ok(Err(refusal)) => match refusal.reason.as_str() {
            reason::ANCHOR => {
                unstage(staged);
                None
            }
            reason::DUPLICATE => Some(frame),
            _ => {
                tracing::warn!(
                    target: TARGET,
                    event = "read_refused",
                    id,
                    reason = %refusal.reason,
                    sentence = %refusal.sentence,
                    "the node refused a reads frame of its own"
                );
                unstage(staged);
                slot.refused = true;
                None
            }
        },
        Err(error) => {
            tracing::warn!(
                target: TARGET,
                event = "read_untaken",
                id,
                reason = "node_failed",
                %error,
                "the node failed to take a reads frame; it is sent again next block"
            );
            None
        }
    };
}
