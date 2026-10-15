//! The open group streams of one egress, to a client or to a peer: which groups have a writer,
//! which were dropped short, and the one queue item each object, batch or end is to a writer.
//! The writer tasks differ (a client's is paced and prioritised, a peer's writes the lane's
//! end word), so each egress spawns its own; the queueing is this one place.

use std::cmp::Ordering;
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
/// a hole in the group's objects (the lane counts it; the client egress has no counter of its
/// own); or dropped quiet, as the rest of a group dropped before.
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

#[derive(Default)]
pub struct Groups {
    open: HashMap<(Alias, u64), Open>,
    /// Groups dropped short: the rest of their objects are dropped quiet, until they end.
    abandoned: HashSet<(Alias, u64)>,
    /// Groups an item of which was dropped on the way here, by the index of the item missing:
    /// each is dropped short when its stream stands at that index, since what would come next
    /// never does.
    cuts: HashMap<(Alias, u64), u32>,
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
            return self.dropped(key);
        }
        match self.hand(header, GroupOut::Object(body), spawn) {
            Handed::Queued => self.advanced(key, 1),
            short @ (Handed::Dropped | Handed::Abandoned) => short,
        }
    }

    /// A whole cached group as one queue item on its stream, from its object 0; a still-open
    /// group (`end: None`) keeps its stream for the live objects that follow, a closed one ends
    /// here, whole by construction, so a cut held for it is over. A group already on a stream
    /// is not replayed onto it: that would double it, so it is cut short instead.
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
            self.cuts.remove(&key);
        }
        let count = objects.len() as u32;
        match (self.hand(header, GroupOut::Batch(objects), spawn), end) {
            (Handed::Queued, Some(complete)) => {
                self.ended(key, complete);
                Handed::Queued
            }
            (Handed::Queued, None) => self.advanced(key, count),
            (short @ (Handed::Dropped | Handed::Abandoned), _) => short,
        }
    }

    /// The group ends, `objects` objects long: a stream that carried them all finishes, one
    /// short of them (an object of it was dropped on the way here) is reset, and either way
    /// the group may begin again.
    pub fn end(&mut self, alias: Alias, group: u64, complete: bool, objects: u32) {
        let key = (alias, group);
        let whole = self.open.get(&key).is_none_or(|open| open.next == objects);
        if whole {
            self.ended(key, complete);
        } else {
            let _ = self.dropped(key);
        }
    }

    /// An item of the group was dropped before it reached here: `at` is its index (an
    /// object's; 0 for a replay; the object count for an end). The group is dropped short when
    /// its stream stands at `at`, now or when it gets there, since what would come next never
    /// does; a stream already past `at` had the item from a replay, and the cut is stale.
    pub fn cut(&mut self, alias: Alias, group: u64, at: u32) {
        let key = (alias, group);
        let next = self.open.get(&key).map_or(0, |open| open.next);
        match next.cmp(&at) {
            Ordering::Less => {
                let held = self.cuts.entry(key).or_insert(at);
                *held = at.min(*held);
            }
            Ordering::Equal => {
                let _ = self.dropped(key);
            }
            Ordering::Greater => {}
        }
    }

    /// The track behind `alias` is over for this peer: nothing of its groups stays open or
    /// held.
    pub fn forget(&mut self, alias: Alias) {
        self.open.retain(|(of, _), _| *of != alias);
        self.abandoned.retain(|(of, _)| *of != alias);
        self.cuts.retain(|(of, _), _| *of != alias);
    }

    /// The group is over: its writer finishes or resets the stream, nothing of it is held, and
    /// the group may begin again.
    fn ended(&mut self, key: (Alias, u64), complete: bool) {
        self.abandoned.remove(&key);
        self.cuts.remove(&key);
        if let Some(open) = self.open.remove(&key) {
            let _ = open.sending.try_send(GroupOut::End { complete });
        }
    }

    /// The stream took `count` more objects. Standing now at the index a cut names, the group
    /// is dropped short; past it, the cut was stale.
    fn advanced(&mut self, key: (Alias, u64), count: u32) -> Handed {
        let Some(open) = self.open.get_mut(&key) else {
            return Handed::Queued;
        };
        open.next += count;
        let next = open.next;
        let Some(at) = self.cuts.get(&key).copied() else {
            return Handed::Queued;
        };
        match next.cmp(&at) {
            Ordering::Less => Handed::Queued,
            Ordering::Equal => self.dropped(key),
            Ordering::Greater => {
                self.cuts.remove(&key);
                Handed::Queued
            }
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
        self.dropped(key)
    }

    /// The group is dropped short: its writer, if any, resets the stream, and the rest of the
    /// group is dropped quiet until it ends.
    fn dropped(&mut self, key: (Alias, u64)) -> Handed {
        if self.abandoned.contains(&key) {
            return Handed::Abandoned;
        }
        self.open.remove(&key);
        self.cuts.remove(&key);
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
        groups.end(1, 5, true, 5);
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
    fn a_cut_at_the_index_missing_drops_the_group_when_its_stream_stands_there() {
        let mut groups = Groups::default();
        let (spawned, spawn) = counting();
        let body = Bytes::from_static(b"x");
        let g5 = GroupHeader { alias: 1, group: 5 };
        let g6 = GroupHeader { alias: 1, group: 6 };
        // g5's end, after two objects, was dropped; its cut overtakes g5's objects, and g6
        // opens before them.
        groups.cut(1, 5, 2);
        assert!(matches!(
            groups.object(g6, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        assert!(matches!(
            groups.object(g5, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        assert!(
            matches!(groups.object(g5, 1, body.clone(), &spawn), Handed::Dropped),
            "the stream stands at the index missing: nothing more comes, and it is reset"
        );
        assert!(matches!(
            read(&mut spawned.borrow_mut()[1]),
            (2, Err(mpsc::error::TryRecvError::Disconnected))
        ));
        assert!(matches!(
            groups.object(g5, 2, body.clone(), &spawn),
            Handed::Abandoned
        ));
        // A cut behind an open stream is stale: a replay had the item.
        groups.cut(1, 6, 0);
        assert!(matches!(
            groups.object(g6, 1, body.clone(), &spawn),
            Handed::Queued
        ));
        assert!(groups.cuts.is_empty());
        // An end short of its objects resets the stream; one at its count finishes it.
        let g7 = GroupHeader { alias: 1, group: 7 };
        assert!(matches!(
            groups.object(g7, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        groups.end(1, 7, true, 2);
        assert!(matches!(
            read(&mut spawned.borrow_mut()[2]),
            (1, Err(mpsc::error::TryRecvError::Disconnected))
        ));
        let g8 = GroupHeader { alias: 1, group: 8 };
        assert!(matches!(
            groups.object(g8, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        assert!(matches!(
            groups.object(g8, 1, body.clone(), &spawn),
            Handed::Queued
        ));
        groups.end(1, 8, true, 2);
        assert!(matches!(
            read(&mut spawned.borrow_mut()[3]),
            (2, Ok(GroupOut::End { complete: true }))
        ));
        // A whole replay of a cut group is taken, cut and all.
        groups.cut(1, 9, 0);
        let g9 = GroupHeader { alias: 1, group: 9 };
        assert!(matches!(
            groups.batch(g9, Arc::new(vec![body.clone(), body]), Some(true), &spawn),
            Handed::Queued
        ));
        assert!(matches!(
            read(&mut spawned.borrow_mut()[4]),
            (1, Ok(GroupOut::End { complete: true }))
        ));
        assert!(groups.cuts.is_empty() && !groups.abandoned.contains(&(1, 9)));
    }

    #[test]
    fn a_track_forgotten_leaves_nothing_of_its_groups() {
        let mut groups = Groups::default();
        let (spawned, spawn) = counting();
        let body = Bytes::from_static(b"x");
        assert!(matches!(
            groups.object(GroupHeader { alias: 1, group: 5 }, 0, body.clone(), &spawn),
            Handed::Queued
        ));
        groups.cut(1, 6, 3);
        assert!(matches!(
            groups.object(GroupHeader { alias: 2, group: 5 }, 3, body, &spawn),
            Handed::Dropped
        ));
        groups.forget(1);
        assert!(matches!(
            read(&mut spawned.borrow_mut()[0]),
            (1, Err(mpsc::error::TryRecvError::Disconnected))
        ));
        assert!(groups.open.is_empty() && groups.cuts.is_empty());
        assert_eq!(groups.abandoned.len(), 1, "another track's hold stays");
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
