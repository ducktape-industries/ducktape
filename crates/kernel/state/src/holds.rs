// What a node keeps for the joiners syncing from it: each open hold names the op a program's
// commitment keeps from, and a commitment prunes no further than the lowest one held. The
// writer reads the floors without a lock; only taking and dropping a hold takes one.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use abi::ProgramId;
use arc_swap::ArcSwap;

#[derive(Clone, Default)]
pub struct Holds {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    sessions: Mutex<Sessions>,
    floors: ArcSwap<BTreeMap<ProgramId, u64>>,
}

#[derive(Default)]
struct Sessions {
    next: u64,
    held: BTreeMap<u64, BTreeMap<ProgramId, u64>>,
}

impl Holds {
    pub fn hold(&self, starts: BTreeMap<ProgramId, u64>) -> Hold {
        let mut sessions = self.sessions();
        let session = sessions.next;
        sessions.next += 1;
        sessions.held.insert(session, starts);
        self.shared.floors.store(Arc::new(lowest(&sessions.held)));
        Hold {
            holds: self.clone(),
            session,
        }
    }

    pub fn floor(&self, program: &str) -> Option<u64> {
        self.shared.floors.load().get(program).copied()
    }

    fn release(&self, session: u64) {
        let mut sessions = self.sessions();
        sessions.held.remove(&session);
        self.shared.floors.store(Arc::new(lowest(&sessions.held)));
    }

    fn sessions(&self) -> MutexGuard<'_, Sessions> {
        self.shared
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Hold {
    holds: Holds,
    session: u64,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.holds.release(self.session);
    }
}

fn lowest(held: &BTreeMap<u64, BTreeMap<ProgramId, u64>>) -> BTreeMap<ProgramId, u64> {
    let mut floors: BTreeMap<ProgramId, u64> = BTreeMap::new();
    for starts in held.values() {
        for (program, start) in starts {
            let floor = floors.entry(program.clone()).or_insert(*start);
            *floor = (*floor).min(*start);
        }
    }
    floors
}

#[cfg(test)]
mod tests;
