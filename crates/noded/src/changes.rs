// The `/v1/changes` fan-out: one task owns every socket's subscriber and hands each block's word to
// each socket's own queue. Sockets subscribe, the writer publishes and the node hangs up over its
// channel, so no socket and no block waits on another.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use abi::ProgramId;
use consensus::Network;
use futures::channel::{mpsc as queue, oneshot as cut};
use state::Writes;
use tokio::sync::{mpsc, oneshot};

use crate::wire::Change;

pub(crate) struct Subscriber {
    program: ProgramId,
    keys: bool,
    sender: queue::UnboundedSender<Published>,
    since: u64,
    read: Arc<AtomicU64>,
    cut: cut::Sender<()>,
}

pub(crate) struct Subscription {
    pub(crate) changes: queue::UnboundedReceiver<Published>,
    pub(crate) read: Arc<AtomicU64>,
    pub(crate) cut: cut::Receiver<()>,
}

pub(crate) enum Published {
    Change(Change),
    Ping(u64),
}

pub(crate) struct Wrote {
    pub(crate) height: u64,
    pub(crate) root: abi::Root,
    pub(crate) writes: Arc<Writes>,
}

pub(crate) enum Event {
    Subscribe(Subscriber),
    Wrote(Wrote),
    HangUp(oneshot::Sender<()>),
    Unasked,
}

enum Flow {
    Fanning,
    HangingUp(oneshot::Sender<()>),
    Closed,
}

#[derive(Clone)]
pub(crate) struct Changes(mpsc::UnboundedSender<Event>);

pub(crate) struct Fanout {
    inbox: mpsc::UnboundedReceiver<Event>,
    network: Network,
    tip: u64,
    subscribers: Vec<Subscriber>,
}

impl Changes {
    pub(crate) fn new(network: Network, tip: u64) -> (Changes, Fanout) {
        let (inbox, events) = mpsc::unbounded_channel();
        let fanout = Fanout {
            inbox: events,
            network,
            tip,
            subscribers: Vec::new(),
        };
        (Changes(inbox), fanout)
    }

    pub(crate) fn subscribe(&self, program: ProgramId, keys: bool) -> Subscription {
        let (sender, changes) = queue::unbounded();
        let (cut, dropped) = cut::channel();
        let read = Arc::new(AtomicU64::new(0));
        let subscription = Subscription {
            changes,
            read: read.clone(),
            cut: dropped,
        };
        let subscriber = Subscriber {
            program,
            keys,
            sender,
            since: 0,
            read,
            cut,
        };
        if let Err(unsent) = self.0.send(Event::Subscribe(subscriber)) {
            refuse(unsent.0);
        }
        subscription
    }

    pub(crate) fn wrote(&self, wrote: Wrote) {
        let _ = self.0.send(Event::Wrote(wrote));
    }

    pub(crate) async fn hang_up(&self) {
        let (reply, hung_up) = oneshot::channel();
        if self.0.send(Event::HangUp(reply)).is_ok() {
            let _ = hung_up.await;
        }
    }
}

fn refuse(event: Event) {
    let Event::Subscribe(subscriber) = event else {
        return;
    };
    let _ = subscriber.cut.send(());
}

pub(crate) async fn fan(mut fanout: Fanout) {
    loop {
        let event = fanout.inbox.recv().await.unwrap_or(Event::Unasked);
        let flow = match event {
            Event::Subscribe(subscriber) => subscribed(&mut fanout, subscriber),
            Event::Wrote(wrote) => written(&mut fanout, wrote),
            Event::HangUp(reply) => Flow::HangingUp(reply),
            Event::Unasked => Flow::Closed,
        };
        match flow {
            Flow::Fanning => {}
            Flow::HangingUp(reply) => return hang_up(fanout, reply),
            Flow::Closed => return,
        }
    }
}

fn subscribed(fanout: &mut Fanout, mut subscriber: Subscriber) -> Flow {
    subscriber.since = fanout.tip;
    fanout.subscribers.push(subscriber);
    Flow::Fanning
}

fn written(fanout: &mut Fanout, wrote: Wrote) -> Flow {
    let height = wrote.height;
    fanout.tip = height;
    let closes = fanout.network.closes_an_epoch(height);
    let last = height.checked_sub(fanout.network.epoch_length);
    fanout.subscribers.retain(|subscriber| {
        if let Some(last) = last.filter(|_| closes)
            && subscriber.since < last
            && subscriber.read.load(Ordering::Relaxed) < last
        {
            return false;
        }
        let Some(writes) = wrote.writes.programs.get(&subscriber.program) else {
            return subscriber
                .sender
                .unbounded_send(Published::Ping(height))
                .is_ok();
        };
        let value = |value: &Option<Vec<u8>>| match subscriber.keys {
            true => None,
            false => value.clone(),
        };
        let change = Change {
            height,
            root: wrote.root,
            writes: writes
                .iter()
                .map(|(key, written)| (key.clone(), value(written)))
                .collect(),
        };
        let sent = subscriber.sender.unbounded_send(Published::Change(change));
        sent.is_ok()
            && (!closes
                || subscriber
                    .sender
                    .unbounded_send(Published::Ping(height))
                    .is_ok())
    });
    Flow::Fanning
}

fn hang_up(mut fanout: Fanout, reply: oneshot::Sender<()>) {
    for subscriber in fanout.subscribers {
        let _ = subscriber.cut.send(());
    }
    fanout.inbox.close();
    while let Ok(event) = fanout.inbox.try_recv() {
        refuse(event);
    }
    let _ = reply.send(());
}
