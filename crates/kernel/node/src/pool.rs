// The mempool: every frame a node has taken that no applied block has carried and whose anchor
// is still recent, in the order the node took them. Owned by `Node`.

use std::collections::{BTreeMap, HashMap};

use host::{Receipt, Submission, SubmissionId};

#[derive(Clone, Debug)]
pub struct Held {
    pub id: SubmissionId,
    pub frame: Vec<u8>,
    pub submission: Submission,
    pub prediction: Receipt,
    arrival: u64,
}

#[derive(Debug, Default)]
pub struct Pool {
    arrivals: u64,
    held: HashMap<SubmissionId, Held>,
    arrived: BTreeMap<u64, SubmissionId>,
}

impl Pool {
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    pub fn get(&self, id: &SubmissionId) -> Option<&Held> {
        self.held.get(id)
    }

    pub fn admit(&mut self, frame: Vec<u8>, submission: Submission, prediction: Receipt) {
        let id = submission.id();
        if self.held.contains_key(&id) {
            return;
        }
        let arrival = self.arrivals;
        self.arrivals += 1;
        self.arrived.insert(arrival, id);
        let held = Held {
            id,
            frame,
            submission,
            prediction,
            arrival,
        };
        self.held.insert(id, held);
    }

    pub fn predict(&mut self, id: &SubmissionId, prediction: Receipt) {
        if let Some(held) = self.held.get_mut(id) {
            held.prediction = prediction;
        }
    }

    pub fn order(&self) -> impl Iterator<Item = &Held> {
        self.arrived.values().map(|id| &self.held[id])
    }

    pub fn settle<'a>(&mut self, carried: impl IntoIterator<Item = &'a SubmissionId>, next: u64) {
        let expired = self
            .held
            .values()
            .filter(|held| !held.submission.anchor.anchors(next))
            .map(|held| held.id);
        let gone: Vec<SubmissionId> = carried.into_iter().copied().chain(expired).collect();
        for id in gone {
            self.remove(&id);
        }
    }

    fn remove(&mut self, id: &SubmissionId) {
        if let Some(held) = self.held.remove(id) {
            self.arrived.remove(&held.arrival);
        }
    }
}
