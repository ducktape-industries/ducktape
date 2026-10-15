//! The open group streams of one egress, to a client or to a peer: which groups have a writer,
//! which were dropped short, and the one queue item each object, batch or end is to a writer.
//! The writer tasks differ (a client's is paced and prioritised, a peer's writes the lane's
//! end word), so each egress spawns its own; the queueing is this one place.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use live_wire::{Alias, GroupHeader};
use tokio::sync::mpsc;

/// Objects queued per group stream before its writer; past it the group is dropped short.
const GROUP_QUEUE: usize = 256;

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

/// How a group's objects reach a writer: queued; dropped now, for a queue that overflowed or
/// a hole in the group's objects (the egress is counted behind); or dropped quiet, as the rest
/// of a group dropped before.
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

#[derive(Default)]
pub struct Groups {
    open: HashMap<(Alias, u64), Open>,
    /// Groups dropped short: the rest of their objects are dropped quiet.
    abandoned: HashSet<(Alias, u64)>,
}

impl Groups {
    /// One object on its group's stream, opening a writer for a new group through `spawn`. The
    /// receiver numbers a stream's objects from 0 in the order read, so a stream is opened only
    /// at a group's object 0 and carries the objects in order: a group first seen past 0 (this
    /// egress is new, the one that carried the start was cut) or an object out of order (one
    /// before it was dropped on the way here) drops the group short, and it is reset.
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
            return self.cut(key);
        }
        let handed = self.hand(header, GroupOut::Object(body), spawn);
        if let (Handed::Queued, Some(open)) = (&handed, self.open.get_mut(&key)) {
            open.next += 1;
        }
        handed
    }

    /// A whole cached group as one queue item on its stream, from its object 0; a still-open
    /// group (`end: None`) keeps its stream for the live objects that follow, a closed one ends
    /// here.
    pub fn batch(
        &mut self,
        header: GroupHeader,
        objects: Arc<Vec<Bytes>>,
        end: Option<bool>,
        spawn: impl FnOnce(GroupHeader, mpsc::Receiver<GroupOut>),
    ) -> Handed {
        let key = (header.alias, header.group);
        let count = objects.len() as u32;
        let handed = self.hand(header, GroupOut::Batch(objects), spawn);
        match (&handed, end) {
            (Handed::Queued, Some(complete)) => self.end(header.alias, header.group, complete),
            (Handed::Queued, None) => {
                if let Some(open) = self.open.get_mut(&key) {
                    open.next += count;
                }
            }
            (Handed::Dropped | Handed::Abandoned, _) => {}
        }
        handed
    }

    /// The group ends: its writer finishes or resets the stream, and the group may begin again.
    pub fn end(&mut self, alias: Alias, group: u64, complete: bool) {
        self.abandoned.remove(&(alias, group));
        if let Some(open) = self.open.remove(&(alias, group)) {
            let _ = open.sending.try_send(GroupOut::End { complete });
        }
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
            let (sending, queue) = mpsc::channel(GROUP_QUEUE);
            spawn(header, queue);
            Open { sending, next: 0 }
        });
        if open.sending.try_send(out).is_ok() {
            return Handed::Queued;
        }
        self.cut(key)
    }

    /// The group is dropped short: its writer, if any, resets the stream, and the rest of the
    /// group is dropped quiet until it ends.
    fn cut(&mut self, key: (Alias, u64)) -> Handed {
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

    /// The queues a counting writer spawn was handed, kept open as a writer would keep them.
    type Spawned = std::rc::Rc<std::cell::RefCell<Vec<mpsc::Receiver<GroupOut>>>>;

    fn counting() -> (Spawned, impl Fn(GroupHeader, mpsc::Receiver<GroupOut>)) {
        let queues = Spawned::default();
        let kept = queues.clone();
        (queues, move |_, queue| kept.borrow_mut().push(queue))
    }

    #[test]
    fn a_group_first_seen_past_its_object_0_is_dropped_until_it_ends() {
        let mut groups = Groups::default();
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

    #[test]
    fn an_object_out_of_order_cuts_its_group_short() {
        let mut groups = Groups::default();
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
            groups.object(header, 3, body, &spawn),
            Handed::Abandoned
        ));
    }
}
