// The `/v1/changes` and `/v1/watch` fan-out: one task owns every socket's subscriber. A changes
// socket gets each block's word in its own queue; a watch socket pulls what the blocks since its
// last pull wrote, merged into one, so no socket and no block waits on another.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use abi::ProgramId;
use consensus::Network;
use futures::channel::{mpsc as queue, oneshot as cut};
use state::Writes;
use tokio::sync::{mpsc, oneshot};

use crate::Context;
use crate::view::View;
use crate::wire::Change;

pub(crate) struct Subscriber<E: Context> {
    socket: u64,
    program: ProgramId,
    keys: bool,
    delivery: Delivery<E>,
    since: u64,
    read: Arc<AtomicU64>,
    cut: cut::Sender<()>,
}

enum Delivery<E: Context> {
    Queued(queue::UnboundedSender<Published>),
    Pulled(Pulled<E>),
}

struct Pulled<E: Context> {
    merged: Option<Box<Merged<E>>>,
    asked: Option<oneshot::Sender<Merged<E>>>,
}

pub(crate) struct Subscription {
    pub(crate) changes: queue::UnboundedReceiver<Published>,
    pub(crate) read: Arc<AtomicU64>,
    pub(crate) cut: cut::Receiver<()>,
}

pub(crate) struct Pull<E: Context> {
    socket: u64,
    changes: Changes<E>,
    pub(crate) read: Arc<AtomicU64>,
    pub(crate) cut: cut::Receiver<()>,
}

struct Seated {
    socket: u64,
    read: Arc<AtomicU64>,
    cut: cut::Receiver<()>,
}

pub(crate) enum Published {
    Change(Change),
    Ping(u64),
}

pub(crate) struct Wrote<E: Context> {
    pub(crate) height: u64,
    pub(crate) root: abi::Root,
    pub(crate) writes: Arc<Writes>,
    pub(crate) view: Option<Arc<View<E>>>,
}

pub(crate) struct Merged<E: Context> {
    pub(crate) height: u64,
    pub(crate) root: abi::Root,
    pub(crate) view: Option<Arc<View<E>>>,
    pub(crate) writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    pub(crate) written: host::Written,
    pub(crate) closes: bool,
}

pub(crate) struct Take<E: Context> {
    socket: u64,
    reply: oneshot::Sender<Merged<E>>,
}

pub(crate) enum Event<E: Context> {
    Subscribe(Subscriber<E>),
    Wrote(Wrote<E>),
    Take(Take<E>),
    HangUp(oneshot::Sender<()>),
    Unasked,
}

enum Flow {
    Fanning,
    HangingUp(oneshot::Sender<()>),
    Closed,
}

pub(crate) struct Changes<E: Context> {
    inbox: mpsc::UnboundedSender<Event<E>>,
    sockets: Arc<AtomicU64>,
}

impl<E: Context> Clone for Changes<E> {
    fn clone(&self) -> Self {
        Changes {
            inbox: self.inbox.clone(),
            sockets: self.sockets.clone(),
        }
    }
}

pub(crate) struct Fanout<E: Context> {
    inbox: mpsc::UnboundedReceiver<Event<E>>,
    network: Network,
    tip: u64,
    subscribers: BTreeMap<u64, Subscriber<E>>,
}

impl<E: Context> Changes<E> {
    pub(crate) fn new(network: Network, tip: u64) -> (Changes<E>, Fanout<E>) {
        let (inbox, events) = mpsc::unbounded_channel();
        let fanout = Fanout {
            inbox: events,
            network,
            tip,
            subscribers: BTreeMap::new(),
        };
        let changes = Changes {
            inbox,
            sockets: Arc::new(AtomicU64::new(0)),
        };
        (changes, fanout)
    }

    pub(crate) fn subscribe(&self, program: ProgramId, keys: bool) -> Subscription {
        let (sender, changes) = queue::unbounded();
        let seated = self.seat(program, keys, Delivery::Queued(sender));
        Subscription {
            changes,
            read: seated.read,
            cut: seated.cut,
        }
    }

    pub(crate) fn pull(&self, program: ProgramId, keys: bool) -> Pull<E> {
        let pulled = Pulled {
            merged: None,
            asked: None,
        };
        let seated = self.seat(program, keys, Delivery::Pulled(pulled));
        Pull {
            socket: seated.socket,
            changes: self.clone(),
            read: seated.read,
            cut: seated.cut,
        }
    }

    fn seat(&self, program: ProgramId, keys: bool, delivery: Delivery<E>) -> Seated {
        let socket = self.sockets.fetch_add(1, Ordering::Relaxed);
        let (cut, dropped) = cut::channel();
        let read = Arc::new(AtomicU64::new(0));
        let subscriber = Subscriber {
            socket,
            program,
            keys,
            delivery,
            since: 0,
            read: read.clone(),
            cut,
        };
        if let Err(unsent) = self.inbox.send(Event::Subscribe(subscriber)) {
            refuse(unsent.0);
        }
        Seated {
            socket,
            read,
            cut: dropped,
        }
    }

    pub(crate) fn wrote(&self, wrote: Wrote<E>) {
        let _ = self.inbox.send(Event::Wrote(wrote));
    }

    pub(crate) async fn hang_up(&self) {
        let (reply, hung_up) = oneshot::channel();
        if self.inbox.send(Event::HangUp(reply)).is_ok() {
            let _ = hung_up.await;
        }
    }
}

impl<E: Context> Pull<E> {
    pub(crate) fn next(&self) -> oneshot::Receiver<Merged<E>> {
        let (reply, merged) = oneshot::channel();
        let take = Take {
            socket: self.socket,
            reply,
        };
        let _ = self.changes.inbox.send(Event::Take(take));
        merged
    }
}

fn refuse<E: Context>(event: Event<E>) {
    let Event::Subscribe(subscriber) = event else {
        return;
    };
    let _ = subscriber.cut.send(());
}

pub(crate) async fn fan<E: Context>(mut fanout: Fanout<E>) {
    loop {
        let event = fanout.inbox.recv().await.unwrap_or(Event::Unasked);
        let flow = match event {
            Event::Subscribe(subscriber) => subscribed(&mut fanout, subscriber),
            Event::Wrote(wrote) => written(&mut fanout, wrote),
            Event::Take(take) => taken(&mut fanout, take),
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

fn subscribed<E: Context>(fanout: &mut Fanout<E>, mut subscriber: Subscriber<E>) -> Flow {
    subscriber.since = fanout.tip;
    fanout.subscribers.insert(subscriber.socket, subscriber);
    Flow::Fanning
}

fn written<E: Context>(fanout: &mut Fanout<E>, wrote: Wrote<E>) -> Flow {
    let height = wrote.height;
    fanout.tip = height;
    let closes = fanout.network.closes_an_epoch(height);
    let last = height
        .checked_sub(fanout.network.epoch_length)
        .filter(|_| closes);
    fanout.subscribers.retain(|_, subscriber| {
        let unanswered = last.is_some_and(|last| {
            subscriber.since < last && subscriber.read.load(Ordering::Relaxed) < last
        });
        let gone = subscriber.cut.is_canceled();
        if unanswered || gone {
            return false;
        }
        let Subscriber {
            program,
            keys,
            delivery,
            ..
        } = subscriber;
        match delivery {
            Delivery::Queued(sender) => queued(sender, program, *keys, &wrote, closes),
            Delivery::Pulled(pulled) => pulled.add(program, *keys, &wrote, closes),
        }
    });
    Flow::Fanning
}

fn queued<E: Context>(
    sender: &queue::UnboundedSender<Published>,
    program: &str,
    keys: bool,
    wrote: &Wrote<E>,
    closes: bool,
) -> bool {
    let height = wrote.height;
    let Some(writes) = wrote.writes.programs.get(program) else {
        return sender.unbounded_send(Published::Ping(height)).is_ok();
    };
    let change = Change {
        height,
        root: wrote.root,
        writes: writes
            .iter()
            .map(|(key, written)| (key.clone(), shown(keys, written)))
            .collect(),
    };
    let sent = sender.unbounded_send(Published::Change(change));
    sent.is_ok() && (!closes || sender.unbounded_send(Published::Ping(height)).is_ok())
}

fn shown(keys: bool, value: &Option<Vec<u8>>) -> Option<Vec<u8>> {
    match keys {
        true => None,
        false => value.clone(),
    }
}

impl<E: Context> Pulled<E> {
    fn add(&mut self, program: &str, keys: bool, wrote: &Wrote<E>, closes: bool) -> bool {
        let mut merged = self
            .merged
            .take()
            .unwrap_or_else(|| Box::new(Merged::at(wrote)));
        merged.add(program, keys, wrote, closes);
        let Some(asked) = self.asked.take() else {
            self.merged = Some(merged);
            return true;
        };
        asked.send(*merged).is_ok()
    }

    fn take(&mut self, reply: oneshot::Sender<Merged<E>>) {
        let Some(merged) = self.merged.take() else {
            self.asked = Some(reply);
            return;
        };
        let _ = reply.send(*merged);
    }
}

impl<E: Context> Merged<E> {
    fn at(wrote: &Wrote<E>) -> Merged<E> {
        Merged {
            height: wrote.height,
            root: wrote.root,
            view: None,
            writes: BTreeMap::new(),
            written: host::Written::new(),
            closes: false,
        }
    }

    fn add(&mut self, program: &str, keys: bool, wrote: &Wrote<E>, closes: bool) {
        self.height = wrote.height;
        self.root = wrote.root;
        self.view = wrote.view.clone();
        self.closes |= closes;
        for (written, slots) in &wrote.writes.programs {
            let keys_written = slots.keys().cloned();
            self.written
                .entry(written.clone())
                .or_default()
                .extend(keys_written);
        }
        let Some(slots) = wrote.writes.programs.get(program) else {
            return;
        };
        for (key, slot) in slots {
            self.writes.insert(key.clone(), shown(keys, slot));
        }
    }
}

fn taken<E: Context>(fanout: &mut Fanout<E>, take: Take<E>) -> Flow {
    let subscriber = fanout.subscribers.get_mut(&take.socket);
    let Some(Subscriber {
        delivery: Delivery::Pulled(pulled),
        ..
    }) = subscriber
    else {
        return Flow::Fanning;
    };
    pulled.take(take.reply);
    Flow::Fanning
}

fn hang_up<E: Context>(mut fanout: Fanout<E>, reply: oneshot::Sender<()>) {
    for subscriber in fanout.subscribers.into_values() {
        let _ = subscriber.cut.send(());
    }
    fanout.inbox.close();
    while let Ok(event) = fanout.inbox.try_recv() {
        refuse(event);
    }
    let _ = reply.send(());
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::time::Duration;

    use consensus::{Cadence, Network, Timeouts};
    use state::Writes;

    use super::{Changes, Wrote, fan};

    type E = commonware_runtime::tokio::Context;

    fn wrote(height: u64, writes: &[(&str, &str)]) -> Wrote<E> {
        let mut programs: BTreeMap<String, BTreeMap<Vec<u8>, Option<Vec<u8>>>> = BTreeMap::new();
        for (program, key) in writes {
            let value = Some(height.to_le_bytes().to_vec());
            programs
                .entry(program.to_string())
                .or_default()
                .insert(key.as_bytes().to_vec(), value);
        }
        Wrote {
            height,
            root: abi::Root([0; 32]),
            writes: Arc::new(Writes {
                programs,
                local: Default::default(),
            }),
            view: None,
        }
    }

    #[test]
    fn a_watch_socket_that_pulls_late_gets_every_block_since_merged_into_one_then_waits_for_the_next()
     {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let cadence = Cadence::new(Duration::from_millis(250), Timeouts::default()).unwrap();
        let network = Network {
            epoch_length: 1_000,
            cadence,
        };
        let (changes, fanout) = Changes::<E>::new(network, 0);
        runtime.spawn(fan(fanout));
        runtime.block_on(async {
            let pull = changes.pull("probe".into(), false);
            changes.wrote(wrote(1, &[("probe", "a"), ("other", "x")]));
            changes.wrote(wrote(2, &[("probe", "a"), ("probe", "b")]));
            let first = pull.next();
            let second = pull.next();
            changes.wrote(wrote(3, &[("other", "y")]));
            changes.hang_up().await;
            let merged = first
                .await
                .expect("a pull with blocks waiting is answered at once");
            assert_eq!(merged.height, 2);
            let newest = Some(2u64.to_le_bytes().to_vec());
            let writes = BTreeMap::from([(b"a".to_vec(), newest.clone()), (b"b".to_vec(), newest)]);
            assert_eq!(merged.writes, writes);
            let written = host::Written::from([
                ("other".to_owned(), BTreeSet::from([b"x".to_vec()])),
                (
                    "probe".to_owned(),
                    BTreeSet::from([b"a".to_vec(), b"b".to_vec()]),
                ),
            ]);
            assert_eq!(merged.written, written);
            let merged = second
                .await
                .expect("a pull with no block waiting is answered by the next one");
            assert_eq!(merged.height, 3);
            assert!(merged.writes.is_empty());
        });
    }
}
