//! The egress worker's decisions after each block and each thing a call reports: which requests to
//! call, and which answers, events and `Serve` frames to send: a request's next frame as soon as the
//! one before is in the pool, carrying every event read since. Pure; `mod.rs` performs them.
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
    Request { id: u64, flight: u64 },
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
    Untaken,
    Refused(Refusal),
}

impl Sent {
    pub fn kind(&self) -> &'static str {
        match self {
            Sent::Pending(_) => "pending",
            Sent::Stale => "stale",
            Sent::Carried => "carried",
            Sent::Untaken => "untaken",
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum Batch {
    Answer(Answer),
    Events(Vec<(u64, Vec<u8>)>),
    End { events: u64, end: StreamEnd },
}

impl Batch {
    fn op(&self, id: u64) -> Op {
        match self.clone() {
            Batch::Answer(answer) => Op::Answer { id, answer },
            Batch::Events(events) => Op::Events(
                events
                    .into_iter()
                    .map(|(seq, data)| Event { id, seq, data })
                    .collect(),
            ),
            Batch::End { events, end } => Op::EndStream { id, events, end },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Running,
    Ended,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Flight {
    batch: Batch,
    sending: Sending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Outbox {
    call: Call,
    events: u64,
    waiting: VecDeque<Out>,
    flights: BTreeMap<u64, Flight>,
    launched: u64,
}

impl Outbox {
    fn new() -> Outbox {
        Outbox {
            call: Call::Running,
            events: 0,
            waiting: VecDeque::new(),
            flights: BTreeMap::new(),
            launched: 0,
        }
    }

    fn take(&mut self, called: Called) {
        let running = self.call == Call::Running;
        if !running {
            return;
        }
        let out = match called {
            Called::Whole(answer) => self.end(Out::Answer(answer)),
            Called::Event(data) => self.event(data),
            Called::End(end) => {
                let events = self.events;
                self.end(Out::End { events, end })
            }
        };
        self.waiting.push_back(out);
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

    fn launch(&mut self) -> Option<u64> {
        let unpooled = self
            .flights
            .values()
            .any(|flight| flight.sending == Sending::Unsigned);
        if unpooled {
            return None;
        }
        let first = self.waiting.pop_front()?;
        let batch = match first {
            Out::Answer(answer) => Batch::Answer(answer),
            Out::End { events, end } => Batch::End { events, end },
            Out::Event { seq, data } => {
                let following = self
                    .waiting
                    .iter()
                    .take_while(|out| matches!(out, Out::Event { .. }))
                    .count();
                let rest = self.waiting.drain(..following).filter_map(|out| match out {
                    Out::Event { seq, data } => Some((seq, data)),
                    Out::Answer(_) | Out::End { .. } => None,
                });
                Batch::Events([(seq, data)].into_iter().chain(rest).collect())
            }
        };
        let flight = self.launched;
        self.launched += 1;
        self.flights.insert(
            flight,
            Flight {
                batch,
                sending: Sending::Unsigned,
            },
        );
        Some(flight)
    }

    fn requeue(&mut self, batch: Batch) {
        let outs = match batch {
            Batch::Answer(answer) => vec![Out::Answer(answer)],
            Batch::Events(events) => events
                .into_iter()
                .map(|(seq, data)| Out::Event { seq, data })
                .collect(),
            Batch::End { events, end } => vec![Out::End { events, end }],
        };
        for out in outs.into_iter().rev() {
            self.waiting.push_front(out);
        }
    }

    fn too_large(&mut self, flight: u64) -> Option<u64> {
        let refused = self.flights.remove(&flight)?;
        let cut = |events: u64, reason: &str| Batch::End {
            events,
            end: StreamEnd::Cut {
                reason: reason.to_owned(),
            },
        };
        let batch = match refused.batch {
            Batch::Events(mut events) if events.len() > 1 => {
                let rest = events.split_off(events.len() / 2);
                self.requeue(Batch::Events(rest));
                Batch::Events(events)
            }
            Batch::Events(events) => {
                self.waiting.clear();
                self.call = Call::Ended;
                cut(
                    events.first().map_or(self.events, |(seq, _)| *seq),
                    "event_too_large",
                )
            }
            Batch::Answer(_) => {
                self.waiting.clear();
                self.call = Call::Ended;
                Batch::Answer(Answer::Failed(
                    "the answer is larger than a block carries".to_owned(),
                ))
            }
            Batch::End { events, .. } => {
                self.waiting.clear();
                self.call = Call::Ended;
                cut(events, "end_too_large")
            }
        };
        self.flights.insert(
            flight,
            Flight {
                batch,
                sending: Sending::Unsigned,
            },
        );
        Some(flight)
    }

    fn sent(&mut self, flight: u64, sent: Sent) -> Option<u64> {
        let too_large =
            matches!(&sent, Sent::Refused(refusal) if refusal.reason == reason::CAPACITY);
        if too_large {
            return self.too_large(flight);
        }
        let sending = &mut self.flights.get_mut(&flight)?.sending;
        match sent {
            Sent::Pending(frame) => {
                *sending = Sending::Pending(frame);
                self.launch()
            }
            Sent::Stale => {
                *sending = Sending::Unsigned;
                Some(flight)
            }
            Sent::Untaken => None,
            Sent::Carried | Sent::Refused(_) => {
                self.flights.remove(&flight);
                self.launch()
            }
        }
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

    pub fn want(&mut self, wanted: Vec<String>) {
        let same = wanted == self.wanted;
        if same {
            return;
        }
        self.wanted = wanted;
        self.serve = Sending::Unsigned;
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
            let flights = outbox
                .flights
                .keys()
                .map(|&flight| Purpose::Request { id, flight });
            steps.extend(flights.map(Step::Send));
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

    pub fn called(&mut self, id: u64, called: Called) {
        if let Some(outbox) = self.requests.get_mut(&id) {
            outbox.take(called);
        }
    }

    pub fn launch(&mut self) -> Vec<Step> {
        self.requests
            .iter_mut()
            .filter_map(|(&id, outbox)| {
                let flight = outbox.launch()?;
                Some(Step::Send(Purpose::Request { id, flight }))
            })
            .collect()
    }

    pub fn op(&self, purpose: &Purpose) -> Option<Op> {
        match purpose {
            Purpose::Request { id, flight } => {
                let flight = self.requests.get(id)?.flights.get(flight)?;
                Some(flight.batch.op(*id))
            }
            Purpose::Serve => Some(Op::Serve {
                services: self.wanted.clone(),
            }),
        }
    }

    pub fn frame(&self, purpose: &Purpose) -> Option<&[u8]> {
        let sending = match purpose {
            Purpose::Request { id, flight } => &self.requests.get(id)?.flights.get(flight)?.sending,
            Purpose::Serve => &self.serve,
        };
        match sending {
            Sending::Pending(frame) => Some(frame),
            Sending::Unsigned | Sending::Done => None,
        }
    }

    pub fn sent(&mut self, purpose: &Purpose, sent: Sent) -> Option<Step> {
        match purpose {
            Purpose::Request { id, flight } => {
                let next = self.requests.get_mut(id)?.sent(*flight, sent)?;
                Some(Step::Send(Purpose::Request {
                    id: *id,
                    flight: next,
                }))
            }
            Purpose::Serve => self.served(sent),
        }
    }

    fn served(&mut self, sent: Sent) -> Option<Step> {
        match sent {
            Sent::Pending(frame) => {
                self.serve = Sending::Pending(frame);
                None
            }
            Sent::Stale => {
                self.serve = Sending::Unsigned;
                Some(Step::Send(Purpose::Serve))
            }
            Sent::Untaken => None,
            Sent::Carried | Sent::Refused(_) => {
                self.serve = Sending::Done;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
