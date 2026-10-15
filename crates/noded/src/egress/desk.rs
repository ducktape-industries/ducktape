//! What the egress worker does after a block, decided from what it read and what it holds: call
//! each request newly elected to this node, send each computed answer, expire each request past
//! its deadline, and send `Serve` while the chain's record of this node differs from its config.
//! Every send repeats each block until a block carries it; the desk never performs one.
use std::collections::{BTreeMap, BTreeSet};

use abi::role::egress::{Answer, Op, RequestRow};
use abi::{Refusal, reason};

pub struct Reading {
    pub assigned: Vec<RequestRow>,
    pub overdue: Vec<RequestRow>,
    pub serving: Vec<String>,
    pub member: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    Answer(u64),
    Expire(u64),
    Serve,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Call(RequestRow),
    Send(Purpose),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    Pending(Vec<u8>),
    Stale,
    Carried,
    Refused(Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Sending {
    Unsigned,
    Pending(Vec<u8>),
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Answering {
    Calling,
    Answered { answer: Answer, sending: Sending },
}

pub struct Desk {
    wanted: Vec<String>,
    answers: BTreeMap<u64, Answering>,
    expiries: BTreeMap<u64, Sending>,
    serve: Sending,
}

fn live(sending: &Sending) -> bool {
    !matches!(sending, Sending::Done)
}

impl Desk {
    pub fn new(wanted: Vec<String>) -> Desk {
        Desk {
            wanted,
            answers: BTreeMap::new(),
            expiries: BTreeMap::new(),
            serve: Sending::Unsigned,
        }
    }

    pub fn read(&mut self, reading: Reading) -> Vec<Step> {
        let mut steps = Vec::new();
        let assigned: BTreeSet<u64> = reading.assigned.iter().map(|row| row.id).collect();
        self.answers.retain(|id, _| assigned.contains(id));
        for row in reading.assigned {
            let id = row.id;
            match self.answers.get(&id) {
                None => {
                    self.answers.insert(id, Answering::Calling);
                    steps.push(Step::Call(row));
                }
                Some(Answering::Answered {
                    sending: Sending::Unsigned | Sending::Pending(_),
                    ..
                }) => steps.push(Step::Send(Purpose::Answer(id))),
                Some(
                    Answering::Calling
                    | Answering::Answered {
                        sending: Sending::Done,
                        ..
                    },
                ) => {}
            }
        }
        let overdue: BTreeSet<u64> = reading.overdue.iter().map(|row| row.id).collect();
        self.expiries.retain(|id, _| overdue.contains(id));
        for id in overdue {
            let sending = self.expiries.entry(id).or_insert(Sending::Unsigned);
            if live(sending) {
                steps.push(Step::Send(Purpose::Expire(id)));
            }
        }
        let settled = reading.serving == self.wanted;
        match (settled, reading.member, &self.serve) {
            (true, _, _) => self.serve = Sending::Unsigned,
            (false, true, Sending::Unsigned | Sending::Pending(_)) => {
                steps.push(Step::Send(Purpose::Serve));
            }
            (false, false, _) | (false, true, Sending::Done) => {}
        }
        steps
    }

    pub fn answered(&mut self, id: u64, answer: Answer) -> Option<Step> {
        let calling = self.answers.get(&id) == Some(&Answering::Calling);
        if !calling {
            return None;
        }
        let sending = Sending::Unsigned;
        self.answers
            .insert(id, Answering::Answered { answer, sending });
        Some(Step::Send(Purpose::Answer(id)))
    }

    pub fn op(&self, purpose: &Purpose) -> Option<Op> {
        match purpose {
            Purpose::Answer(id) => match self.answers.get(id)? {
                Answering::Answered { answer, .. } => Some(Op::Answer {
                    id: *id,
                    answer: answer.clone(),
                }),
                Answering::Calling => None,
            },
            Purpose::Expire(id) => Some(Op::Expire { id: *id }),
            Purpose::Serve => Some(Op::Serve {
                services: self.wanted.clone(),
            }),
        }
    }

    pub fn frame(&self, purpose: &Purpose) -> Option<&[u8]> {
        match self.sending(purpose)? {
            Sending::Pending(frame) => Some(frame),
            Sending::Unsigned | Sending::Done => None,
        }
    }

    fn sending(&self, purpose: &Purpose) -> Option<&Sending> {
        match purpose {
            Purpose::Answer(id) => match self.answers.get(id)? {
                Answering::Answered { sending, .. } => Some(sending),
                Answering::Calling => None,
            },
            Purpose::Expire(id) => self.expiries.get(id),
            Purpose::Serve => Some(&self.serve),
        }
    }

    pub fn sent(&mut self, purpose: &Purpose, sent: Sent) {
        let too_large = matches!(&sent, Sent::Refused(refusal) if refusal.reason == reason::CAPACITY);
        let next = match sent {
            Sent::Pending(frame) => Sending::Pending(frame),
            Sent::Stale => Sending::Unsigned,
            Sent::Carried | Sent::Refused(_) => Sending::Done,
        };
        match purpose {
            Purpose::Answer(id) => {
                let Some(Answering::Answered { answer, sending }) = self.answers.get_mut(id) else {
                    return;
                };
                if too_large {
                    *answer = Answer::Failed(
                        "the answer is larger than a block carries".to_owned(),
                    );
                    *sending = Sending::Unsigned;
                    return;
                }
                *sending = next;
            }
            Purpose::Expire(id) => {
                if let Some(sending) = self.expiries.get_mut(id) {
                    *sending = next;
                }
            }
            Purpose::Serve => self.serve = next,
        }
    }
}

#[cfg(test)]
mod tests;
