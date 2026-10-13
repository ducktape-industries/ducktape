// The mempool: every frame a node has taken that no applied block has carried and whose anchor
// is still recent, in the order the node took them. Owned by `Node`.

use std::collections::{BTreeMap, HashMap};

use host::{Receipt, Submission, SubmissionId};

const FIRST_WAIT: u64 = 4;
const LONGEST_WAIT: u64 = 64;

#[derive(Clone, Debug)]
pub struct Held {
    pub id: SubmissionId,
    pub frame: Vec<u8>,
    pub submission: Submission,
    pub prediction: Receipt,
    pub attached: u64,
    arrival: u64,
    offer: Option<Offer>,
}

#[derive(Clone, Copy, Debug)]
struct Offer {
    at: u64,
    sends: u32,
}

impl Offer {
    fn waited_out(&self, height: u64) -> bool {
        let wait = (FIRST_WAIT << self.sends.saturating_sub(1).min(4)).min(LONGEST_WAIT);
        height.saturating_sub(self.at) >= wait
    }
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

    pub fn admit(
        &mut self,
        frame: Vec<u8>,
        submission: Submission,
        prediction: Receipt,
        attached: u64,
        offered: Option<u64>,
    ) {
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
            attached,
            arrival,
            offer: offered.map(|at| Offer { at, sends: 1 }),
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

    pub fn reoffer(&mut self, height: u64) -> Vec<Vec<u8>> {
        self.offer_where(height, |offer| offer.waited_out(height))
    }

    pub fn reoffer_all(&mut self, height: u64) -> Vec<Vec<u8>> {
        self.offer_where(height, |_| true)
    }

    fn offer_where(&mut self, height: u64, due: impl Fn(&Offer) -> bool) -> Vec<Vec<u8>> {
        let mut offered = Vec::new();
        for id in self.arrived.values() {
            let held = self.held.get_mut(id).expect("every arrival is held");
            let Some(offer) = held.offer.filter(&due) else {
                continue;
            };
            held.offer = Some(Offer {
                at: height,
                sends: offer.sends + 1,
            });
            offered.push(held.frame.clone());
        }
        offered
    }

    fn remove(&mut self, id: &SubmissionId) {
        if let Some(held) = self.held.remove(id) {
            self.arrived.remove(&held.arrival);
        }
    }
}
