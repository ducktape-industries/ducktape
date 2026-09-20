use std::collections::VecDeque;

use crate::{Orderer, Result};

#[derive(Default)]
pub struct InstantOrderer {
    view: u64,
    delivered: Vec<(u64, Vec<u8>)>,
}

impl Orderer for InstantOrderer {
    fn proposes(&self) -> bool {
        true
    }

    async fn submit(&mut self, proposal: Vec<u8>) -> Result<()> {
        self.view += 1;
        self.delivered.push((self.view, proposal));
        Ok(())
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        std::mem::take(&mut self.delivered)
    }

    fn in_flight(&self) -> usize {
        0
    }

    fn certificate_at_or_below(&self, _view: u64) -> Option<(u64, Vec<u8>)> {
        None
    }
}

#[derive(Default)]
pub struct StepOrderer {
    view: u64,
    queued: VecDeque<Vec<u8>>,
    delivered: Vec<(u64, Vec<u8>)>,
}

impl StepOrderer {
    pub fn queued(&self) -> usize {
        self.queued.len()
    }

    pub fn release(&mut self, count: usize) -> usize {
        let mut released = 0;
        while released < count {
            let Some(proposal) = self.queued.pop_front() else {
                break;
            };
            self.view += 1;
            self.delivered.push((self.view, proposal));
            released += 1;
        }
        released
    }

    pub fn release_all(&mut self) -> usize {
        self.release(self.queued.len())
    }
}

impl Orderer for StepOrderer {
    fn proposes(&self) -> bool {
        true
    }

    async fn submit(&mut self, proposal: Vec<u8>) -> Result<()> {
        self.queued.push_back(proposal);
        Ok(())
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        std::mem::take(&mut self.delivered)
    }

    fn in_flight(&self) -> usize {
        self.queued.len()
    }

    fn certificate_at_or_below(&self, _view: u64) -> Option<(u64, Vec<u8>)> {
        None
    }
}
