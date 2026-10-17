//! The egress worker's decisions after each block and each thing a call reports: which requests to
//! call, and which answers, events and `Serve` frames to send, a request's frames one at a time
//! in order. Pure; `mod.rs` performs what it decides.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use abi::role::egress::{Answer, Event, Op, RequestRow, StreamEnd};
use abi::{Refusal, reason};

pub struct Reading {
    pub assigned: Vec<RequestRow>,
    pub serving: Vec<String>,
    pub member: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    Request(u64),
    Serve,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Call(RequestRow),
    Send(Purpose),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Called {
    Whole(Answer),
    Event(Vec<u8>),
    End(StreamEnd),
}

impl Called {
    pub fn ending(&self) -> &str {
        match self {
            Called::Whole(Answer::Output(_)) => "output",
            Called::Whole(Answer::Failed(_)) => "failed",
            Called::Event(_) => "event",
            Called::End(StreamEnd::Complete { .. }) => "complete",
            Called::End(StreamEnd::Cut { reason }) => reason,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    Pending(Vec<u8>),
    Stale,
    Carried,
    Refused(Refusal),
}

impl Sent {
    pub fn kind(&self) -> &'static str {
        match self {
            Sent::Pending(_) => "pending",
            Sent::Stale => "stale",
            Sent::Carried => "carried",
            Sent::Refused(_) => "refused",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Sending {
    Unsigned,
    Pending(Vec<u8>),
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Out {
    Answer(Answer),
    Event { seq: u64, data: Vec<u8> },
    End { events: u64, end: StreamEnd },
}

impl Out {
    fn op(&self, id: u64) -> Op {
        match self.clone() {
            Out::Answer(answer) => Op::Answer { id, answer },
            Out::Event { seq, data } => Op::Event(Event { id, seq, data }),
            Out::End { events, end } => Op::EndStream { id, events, end },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    Send,
    Wait,
}

impl Next {
    fn step(self, purpose: Purpose) -> Option<Step> {
        match self {
            Next::Send => Some(Step::Send(purpose)),
            Next::Wait => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Running,
    Ended,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Queued {
    out: Out,
    sending: Sending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Outbox {
    call: Call,
    events: u64,
    queue: VecDeque<Queued>,
}

impl Outbox {
    fn new() -> Outbox {
        Outbox {
            call: Call::Running,
            events: 0,
            queue: VecDeque::new(),
        }
    }

    fn take(&mut self, called: Called) -> Next {
        let running = self.call == Call::Running;
        if !running {
            return Next::Wait;
        }
        let out = match called {
            Called::Whole(answer) => self.end(Out::Answer(answer)),
            Called::Event(data) => self.event(data),
            Called::End(end) => {
                let events = self.events;
                self.end(Out::End { events, end })
            }
        };
        self.queue.push_back(Queued {
            out,
            sending: Sending::Unsigned,
        });
        let first = self.queue.len() == 1;
        match first {
            true => Next::Send,
            false => Next::Wait,
        }
    }

    fn event(&mut self, data: Vec<u8>) -> Out {
        let seq = self.events;
        self.events += 1;
        Out::Event { seq, data }
    }

    fn end(&mut self, out: Out) -> Out {
        self.call = Call::Ended;
        out
    }

    fn too_large(&mut self) -> Next {
        let Some(head) = self.queue.front() else {
            return Next::Wait;
        };
        let cut = |events: u64, reason: &str| Out::End {
            events,
            end: StreamEnd::Cut {
                reason: reason.to_owned(),
            },
        };
        let replacement = match &head.out {
            Out::Answer(_) => Out::Answer(Answer::Failed(
                "the answer is larger than a block carries".to_owned(),
            )),
            Out::Event { seq, .. } => cut(*seq, "event_too_large"),
            Out::End { events, .. } => cut(*events, "end_too_large"),
        };
        self.queue.clear();
        self.queue.push_back(Queued {
            out: replacement,
            sending: Sending::Unsigned,
        });
        self.call = Call::Ended;
        Next::Send
    }

    fn sent(&mut self, sent: Sent) -> Next {
        let too_large =
            matches!(&sent, Sent::Refused(refusal) if refusal.reason == reason::CAPACITY);
        if too_large {
            return self.too_large();
        }
        let Some(head) = self.queue.front_mut() else {
            return Next::Wait;
        };
        match sent {
            Sent::Pending(frame) => head.sending = Sending::Pending(frame),
            Sent::Stale => head.sending = Sending::Unsigned,
            Sent::Carried | Sent::Refused(_) => {
                self.queue.pop_front();
                return match self.queue.front() {
                    Some(_) => Next::Send,
                    None => Next::Wait,
                };
            }
        }
        Next::Wait
    }
}

pub struct Desk {
    wanted: Vec<String>,
    requests: BTreeMap<u64, Outbox>,
    serve: Sending,
}

impl Desk {
    pub fn new(wanted: Vec<String>) -> Desk {
        Desk {
            wanted,
            requests: BTreeMap::new(),
            serve: Sending::Unsigned,
        }
    }

    pub fn read(&mut self, reading: Reading) -> Vec<Step> {
        let mut steps = Vec::new();
        let assigned: BTreeSet<u64> = reading.assigned.iter().map(|row| row.id).collect();
        self.requests.retain(|id, _| assigned.contains(id));
        for row in reading.assigned {
            let id = row.id;
            let Some(outbox) = self.requests.get(&id) else {
                self.requests.insert(id, Outbox::new());
                steps.push(Step::Call(row));
                continue;
            };
            let waiting = outbox.queue.front().is_some();
            if waiting {
                steps.push(Step::Send(Purpose::Request(id)));
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

    pub fn called(&mut self, id: u64, called: Called) -> Option<Step> {
        let next = self.requests.get_mut(&id)?.take(called);
        next.step(Purpose::Request(id))
    }

    pub fn op(&self, purpose: &Purpose) -> Option<Op> {
        match purpose {
            Purpose::Request(id) => {
                let head = self.requests.get(id)?.queue.front()?;
                Some(head.out.op(*id))
            }
            Purpose::Serve => Some(Op::Serve {
                services: self.wanted.clone(),
            }),
        }
    }

    pub fn frame(&self, purpose: &Purpose) -> Option<&[u8]> {
        let sending = match purpose {
            Purpose::Request(id) => &self.requests.get(id)?.queue.front()?.sending,
            Purpose::Serve => &self.serve,
        };
        match sending {
            Sending::Pending(frame) => Some(frame),
            Sending::Unsigned | Sending::Done => None,
        }
    }

    pub fn sent(&mut self, purpose: &Purpose, sent: Sent) -> Option<Step> {
        match purpose {
            Purpose::Request(id) => {
                let next = self.requests.get_mut(id)?.sent(sent);
                next.step(purpose.clone())
            }
            Purpose::Serve => {
                self.serve = match sent {
                    Sent::Pending(frame) => Sending::Pending(frame),
                    Sent::Stale => Sending::Unsigned,
                    Sent::Carried | Sent::Refused(_) => Sending::Done,
                };
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
