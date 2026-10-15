// What a block is built from: the pool and the tip it is settled to, behind a lock of their own.
// `Node` admits and settles through it; consensus builds through a `Proposer` while a block applies.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use host::{SubmissionId, Tip};

use crate::block::Block;
use crate::frame::Frame;
use crate::pool::Pool;

pub(crate) struct Pending {
    pub(crate) pool: Pool,
    pub(crate) applied: Tip,
}

#[derive(Clone)]
pub struct Proposer {
    pending: Arc<Mutex<Pending>>,
}

impl Proposer {
    pub(crate) fn new(applied: Tip) -> Proposer {
        let pending = Pending {
            pool: Pool::default(),
            applied,
        };
        Proposer {
            pending: Arc::new(Mutex::new(pending)),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn applied(&self) -> Tip {
        self.lock().applied
    }

    pub fn build<'a>(
        &self,
        parent: Tip,
        unapplied: impl IntoIterator<Item = &'a Block>,
        time: u64,
    ) -> Block {
        let height = parent.height + 1;
        let pending = self.lock();
        let carried = carried_above(pending.applied, unapplied);
        let frames = pending
            .pool
            .order()
            .filter(|held| {
                let recent = held.submission.anchor.anchors(height);
                recent && !carried.contains(&held.id)
            })
            .map(|held| &held.frame);
        Block::packed(parent, time, frames)
    }
}

fn carried_above<'a>(
    applied: Tip,
    unapplied: impl IntoIterator<Item = &'a Block>,
) -> HashSet<SubmissionId> {
    unapplied
        .into_iter()
        .filter(|block| block.height > applied.height)
        .flat_map(|block| &block.frames)
        .filter_map(|frame| Some(Frame::decode(frame).ok()?.body.submission().id()))
        .collect()
}
