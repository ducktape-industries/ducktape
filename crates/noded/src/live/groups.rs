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

/// How a group's objects reach a writer: what was handed is queued, or the group is dropped
/// short for falling behind.
pub enum Handed {
    Queued,
    Dropped,
}

#[derive(Default)]
pub struct Groups {
    open: HashMap<(Alias, u64), mpsc::Sender<GroupOut>>,
    /// Groups dropped short for falling behind: the rest of their objects are dropped quiet.
    abandoned: HashSet<(Alias, u64)>,
}

impl Groups {
    /// One object on its group's stream, opening a writer for a new group through `spawn`.
    pub fn object(
        &mut self,
        header: GroupHeader,
        body: Bytes,
        spawn: impl FnOnce(GroupHeader, mpsc::Receiver<GroupOut>),
    ) -> Handed {
        self.hand(header, GroupOut::Object(body), spawn)
    }

    /// A whole cached group as one queue item on its stream; a still-open group (`end: None`)
    /// keeps its stream for the live objects that follow, a closed one ends here.
    pub fn batch(
        &mut self,
        header: GroupHeader,
        objects: Arc<Vec<Bytes>>,
        end: Option<bool>,
        spawn: impl FnOnce(GroupHeader, mpsc::Receiver<GroupOut>),
    ) -> Handed {
        let handed = self.hand(header, GroupOut::Batch(objects), spawn);
        if let (Handed::Queued, Some(complete)) = (&handed, end) {
            self.end(header.alias, header.group, complete);
        }
        handed
    }

    /// The group ends: its writer finishes or resets the stream, and the group may begin again.
    pub fn end(&mut self, alias: Alias, group: u64, complete: bool) {
        self.abandoned.remove(&(alias, group));
        if let Some(sending) = self.open.remove(&(alias, group)) {
            let _ = sending.try_send(GroupOut::End { complete });
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
            return Handed::Dropped;
        }
        let sending = self.open.entry(key).or_insert_with(|| {
            let (sending, queue) = mpsc::channel(GROUP_QUEUE);
            spawn(header, queue);
            sending
        });
        if sending.try_send(out).is_ok() {
            return Handed::Queued;
        }
        self.open.remove(&key);
        self.abandoned.insert(key);
        Handed::Dropped
    }
}
