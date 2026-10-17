//! The open group streams of one egress, to a client or to a peer: which groups have a writer,
//! which were dropped short, and the one queue item each object, batch or end is to a writer.
//! The writer tasks differ in how their stream opens and ends (a client's is paced and
//! prioritised, a peer's writes the lane's end word), so each egress spawns its own around the
//! one writing loop here; the queueing is this one place too.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use live_wire::{Alias, GroupHeader};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;

use super::session::framed;

/// What a group writer takes from its queue next.
pub enum GroupOut {
    Object(Bytes),
    /// A whole cached group's objects, written in one go: a replay is one queue item, not one
    /// per object.
    Batch(Arc<Vec<Bytes>>),
    End {
        complete: bool,
    },
}

/// One group written onto its open stream: the header, then each object as it is queued, as
/// length-framed bytes. `true` when the group completed and the stream is the caller's to
/// finish; `false` when it is to reset: the group was cut short, its queue closed, or a write
/// failed.
pub async fn written(
    stream: &mut (impl AsyncWrite + Unpin),
    header: &GroupHeader,
    queue: &mut mpsc::Receiver<GroupOut>,
) -> bool {
    if stream.write_all(&abi::encode(header)).await.is_err() {
        return false;
    }
    loop {
        match queue.recv().await {
            Some(GroupOut::Object(body)) => {
                if framed(stream, &body).await.is_err() {
                    return false;
                }
            }
            Some(GroupOut::Batch(objects)) => {
                for body in objects.iter() {
                    if framed(stream, body).await.is_err() {
                        return false;
                    }
                }
            }
            Some(GroupOut::End { complete }) => return complete,
            None => return false,
        }
    }
}

/// How a group's objects reach a writer: queued; dropped now, for a queue that overflowed or a
/// group whose start this egress never carried (the lane counts it; the client egress has no
/// counter of its own); or dropped quiet, as the rest of a group dropped before.
#[must_use]
pub enum Handed {
    Queued,
    Dropped,
    Abandoned,
}

/// One open group stream: its writer's queue and the object the group's next object must be.
struct Open {
    sending: mpsc::Sender<GroupOut>,
    next: u32,
}

pub struct Groups {
    /// Objects queued per group stream before its writer; past it the group is dropped short.
    queue: usize,
    open: HashMap<(Alias, u64), Open>,
    /// Groups dropped short: the rest of their objects are dropped quiet, until they end.
    abandoned: HashSet<(Alias, u64)>,
}

impl Groups {
    pub fn new(queue: usize) -> Groups {
        Groups {
            queue,
            open: HashMap::new(),
            abandoned: HashSet::new(),
        }
    }

    /// One object on its group's stream, opening a writer for a new group through `spawn`. The
    /// receiver numbers a stream's objects from 0 in the order read, so a stream is opened only
    /// at a group's object 0: a group first seen past it (this egress is new, the one that
    /// carried the start was cut) is dropped short.
    pub fn object(
        &mut self,
        header: GroupHeader,
        object: u32,
        body: Bytes,
        spawn: impl FnOnce(GroupHeader, mpsc::Receiver<GroupOut>),
    ) -> Handed {
        let key = (header.alias, header.group);
        let expected = self.open.get(&key).map_or(0, |open| open.next);
        if object != expected {
            return self.dropped(key);
        }
        match self.hand(header, GroupOut::Object(body), spawn) {
            Handed::Queued => self.advanced(key, 1),
            short @ (Handed::Dropped | Handed::Abandoned) => short,
        }
    }

    /// A whole cached group as one queue item on its stream, from its object 0; a still-open
    /// group (`end: None`) keeps its stream for the live objects that follow, a closed one ends
    /// here, and whole, so a group dropped short before may begin again through it. A group
    /// already on a stream is not replayed onto it: that would double it, so it is cut short
    /// instead.
    pub fn batch(
        &mut self,
        header: GroupHeader,
        objects: Arc<Vec<Bytes>>,
        end: Option<bool>,
        spawn: impl FnOnce(GroupHeader, mpsc::Receiver<GroupOut>),
    ) -> Handed {
        let key = (header.alias, header.group);
        if self.open.contains_key(&key) {
            return self.dropped(key);
        }
        if end.is_some() {
            self.abandoned.remove(&key);
        }
        let count = objects.len() as u32;
        match (self.hand(header, GroupOut::Batch(objects), spawn), end) {
            (Handed::Queued, Some(complete)) => {
                self.end(header.alias, header.group, complete);
                Handed::Queued
            }
            (Handed::Queued, None) => self.advanced(key, count),
            (short @ (Handed::Dropped | Handed::Abandoned), _) => short,
        }
    }

    /// The group is over: its writer finishes or resets the stream, nothing of it is held, and
    /// the group may begin again.
    pub fn end(&mut self, alias: Alias, group: u64, complete: bool) {
        let key = (alias, group);
        self.abandoned.remove(&key);
        if let Some(open) = self.open.remove(&key) {
            let _ = open.sending.try_send(GroupOut::End { complete });
        }
    }

    /// The track behind `alias` is over for this peer: nothing of its groups stays open or
    /// held.
    pub fn forget(&mut self, alias: Alias) {
        self.open.retain(|(of, _), _| *of != alias);
        self.abandoned.retain(|(of, _)| *of != alias);
    }

    /// The stream took `count` more objects.
    fn advanced(&mut self, key: (Alias, u64), count: u32) -> Handed {
        if let Some(open) = self.open.get_mut(&key) {
            open.next += count;
        }
        Handed::Queued
    }

    fn hand(
        &mut self,
        header: GroupHeader,
        out: GroupOut,
        spawn: impl FnOnce(GroupHeader, mpsc::Receiver<GroupOut>),
    ) -> Handed {
        let key = (header.alias, header.group);
        if self.abandoned.contains(&key) {
            return Handed::Abandoned;
        }
        let open = self.open.entry(key).or_insert_with(|| {
            let (sending, queue) = mpsc::channel(self.queue);
            spawn(header, queue);
            Open { sending, next: 0 }
        });
        if open.sending.try_send(out).is_ok() {
            return Handed::Queued;
        }
        self.dropped(key)
    }

    /// The group is dropped short: its writer, if any, resets the stream, and the rest of the
    /// group is dropped quiet until it ends.
    fn dropped(&mut self, key: (Alias, u64)) -> Handed {
        if self.abandoned.contains(&key) {
            return Handed::Abandoned;
        }
        self.open.remove(&key);
        self.abandoned.insert(key);
        Handed::Dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUEUE: usize = 4;

    /// The queues a counting writer spawn was handed, kept open as a writer would keep them.
    type Spawned = std::rc::Rc<std::cell::RefCell<Vec<mpsc::Receiver<GroupOut>>>>;

    fn counting() -> (Spawned, impl Fn(GroupHeader, mpsc::Receiver<GroupOut>)) {
        let queues = Spawned::default();
        let kept = queues.clone();
        (queues, move |_, queue| kept.borrow_mut().push(queue))
    }

    #[test]
    fn a_group_first_seen_past_its_object_0_is_dropped_until_it_ends() {
        let mut groups = Groups::new(QUEUE);
        let header = GroupHeader { alias: 1, group: 5 };
        let (spawned, spawn) = counting();
        let body = Bytes::from_static(b"x");
        assert!(matches!(
            groups.object(header, 3, body.clone(), &spawn),
            Handed::Dropped
        ));
        assert!(matches!(
            groups.object(header, 4, body.clone(), &spawn),
            Handed::Abandoned
        ));
        assert_eq!(
            spawned.borrow().len(),
            0,
            "no stream for a group whose start this egress never carried"
        );
        groups.end(1, 5, true);
        let next = GroupHeader { alias: 1, group: 6 };
        assert!(matches!(
            groups.object(next, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        assert!(matches!(
            groups.object(next, 1, body, &spawn),
            Handed::Queued
        ));
        assert_eq!(spawned.borrow().len(), 1, "one stream, opened at object 0");
    }

    /// What a writer's queue holds, read out: `Ok` items until it closes or empties.
    fn read(
        queue: &mut mpsc::Receiver<GroupOut>,
    ) -> (usize, Result<GroupOut, mpsc::error::TryRecvError>) {
        let mut items = 0;
        loop {
            match queue.try_recv() {
                Ok(GroupOut::Object(_) | GroupOut::Batch(_)) => items += 1,
                other => return (items, other),
            }
        }
    }

    #[test]
    fn a_groups_queue_that_overflows_drops_that_group_short_and_its_end_is_quiet() {
        let mut groups = Groups::new(QUEUE);
        let (spawned, spawn) = counting();
        let body = Bytes::from_static(b"x");
        let g5 = GroupHeader { alias: 1, group: 5 };
        for object in 0..QUEUE as u32 {
            assert!(matches!(
                groups.object(g5, object, body.clone(), &spawn),
                Handed::Queued
            ));
        }
        assert!(
            matches!(
                groups.object(g5, QUEUE as u32, body.clone(), &spawn),
                Handed::Dropped
            ),
            "the object past the queue drops the group, which is reset"
        );
        assert!(matches!(
            read(&mut spawned.borrow_mut()[0]),
            (QUEUE, Err(mpsc::error::TryRecvError::Disconnected))
        ));
        groups.end(1, 5, true);
        assert!(
            !groups.abandoned.contains(&(1, 5)),
            "its end lets the group begin again"
        );
        // An end at a group's count finishes it; a whole replay of a dropped group is taken.
        let g8 = GroupHeader { alias: 1, group: 8 };
        assert!(matches!(
            groups.object(g8, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        groups.end(1, 8, true);
        assert!(matches!(
            read(&mut spawned.borrow_mut()[1]),
            (1, Ok(GroupOut::End { complete: true }))
        ));
        let g9 = GroupHeader { alias: 1, group: 9 };
        assert!(matches!(
            groups.object(g9, 3, body.clone(), &spawn),
            Handed::Dropped
        ));
        assert!(matches!(
            groups.batch(g9, Arc::new(vec![body.clone(), body]), Some(true), &spawn),
            Handed::Queued
        ));
        assert!(matches!(
            read(&mut spawned.borrow_mut()[2]),
            (1, Ok(GroupOut::End { complete: true }))
        ));
        assert!(!groups.abandoned.contains(&(1, 9)));
    }

    #[test]
    fn a_track_forgotten_leaves_nothing_of_its_groups() {
        let mut groups = Groups::new(QUEUE);
        let (spawned, spawn) = counting();
        let body = Bytes::from_static(b"x");
        assert!(matches!(
            groups.object(GroupHeader { alias: 1, group: 5 }, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        assert!(matches!(
            groups.object(GroupHeader { alias: 2, group: 5 }, 3, body, &spawn),
            Handed::Dropped
        ));
        groups.forget(1);
        assert!(matches!(
            read(&mut spawned.borrow_mut()[0]),
            (1, Err(mpsc::error::TryRecvError::Disconnected))
        ));
        assert!(groups.open.is_empty());
        assert_eq!(groups.abandoned.len(), 1, "another track's hold stays");
    }

    #[test]
    fn an_object_out_of_order_cuts_its_group_short() {
        let mut groups = Groups::new(QUEUE);
        let header = GroupHeader { alias: 1, group: 5 };
        let (_, spawn) = counting();
        let body = Bytes::from_static(b"x");
        assert!(matches!(
            groups.object(header, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        // Object 1 was dropped on the way here: 2 would be read as 1.
        assert!(matches!(
            groups.object(header, 2, body.clone(), &spawn),
            Handed::Dropped
        ));
        assert!(matches!(
            groups.object(header, 3, body.clone(), &spawn),
            Handed::Abandoned
        ));
        // A replay of a group already on a stream would double it: cut, not appended.
        let open = GroupHeader { alias: 1, group: 6 };
        assert!(matches!(
            groups.object(open, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        let replay = Arc::new(vec![body]);
        assert!(matches!(
            groups.batch(open, replay, None, &spawn),
            Handed::Dropped
        ));
    }
}
