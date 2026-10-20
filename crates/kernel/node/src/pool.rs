// The frames a node has taken that no applied block has carried and whose anchor is still recent,
// in the order the node took them. Each held frame is in the node's `Verified` for as long as it
// is held, and only a frame the pool adds goes in. `Mempool` holds it.

use std::collections::{BTreeMap, HashMap};

use abi::Receipt;
use host::{Submission, SubmissionId};

use crate::frame::Checked;
use crate::verified::Verified;

const FIRST_WAIT: u64 = 4;
const LONGEST_WAIT: u64 = 64;

#[derive(Clone, Debug)]
pub struct Held {
    pub id: SubmissionId,
    checked: Checked,
    pub prediction: Option<Receipt>,
    arrival: u64,
    offer: Option<Offer>,
}

impl Held {
    pub fn frame(&self) -> &[u8] {
        self.checked.frame()
    }

    pub fn submission(&self) -> &Submission {
        self.checked.submission()
    }

    pub fn preconfirmed(&self) -> bool {
        self.prediction.is_some()
    }
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

#[derive(Debug)]
pub struct Pool {
    verified: Verified,
    arrivals: u64,
    held: HashMap<SubmissionId, Held>,
    arrived: BTreeMap<u64, SubmissionId>,
}

impl Pool {
    pub fn new(verified: Verified) -> Pool {
        Pool {
            verified,
            arrivals: 0,
            held: HashMap::new(),
            arrived: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    pub fn get(&self, id: &SubmissionId) -> Option<&Held> {
        self.held.get(id)
    }

    pub fn admit(&mut self, checked: Checked, prediction: Option<Receipt>, offered: Option<u64>) {
        let id = checked.submission().id();
        if self.held.contains_key(&id) {
            return;
        }
        self.verified.insert(&checked);
        let arrival = self.arrivals;
        self.arrivals += 1;
        self.arrived.insert(arrival, id);
        let held = Held {
            id,
            checked,
            prediction,
            arrival,
            offer: offered.map(|at| Offer { at, sends: 1 }),
        };
        self.held.insert(id, held);
    }

    pub fn predict(&mut self, id: &SubmissionId, prediction: Receipt) {
        if let Some(held) = self.held.get_mut(id) {
            held.prediction = Some(prediction);
        }
    }

    pub fn order(&self) -> impl Iterator<Item = &Held> {
        self.arrived.values().map(|id| &self.held[id])
    }

    pub fn remove(&mut self, ids: &[SubmissionId]) {
        for id in ids {
            if let Some(held) = self.held.remove(id) {
                self.arrived.remove(&held.arrival);
                self.verified.remove(held.frame());
            }
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
            offered.push(held.frame().to_vec());
        }
        offered
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for held in self.held.values() {
            self.verified.remove(held.frame());
        }
    }
}
