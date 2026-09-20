//! the worker seam — the host-owned, NON-DETERMINISTIC lane beside the drain.
//!
//! the host's within-block drain is the DETERMINISTIC re-entry lane: `emit_msg`
//! follow-ups re-dispatch inside ONE block, one root-hash. this seam is the
//! non-deterministic lane: a block may emit [`Event`]s, and each event that a
//! [`Worker`] claims produces a follow-up op submitted as a SEPARATE block —
//! because on a real node it is a separate consensus transaction (the
//! oracle-as-op). that split — deterministic hops stay local and free, only
//! genuine external edges pay a round — is the whole design.
//!
//! this seam is domain-agnostic: it knows `Event` and `Msg`, nothing about
//! sagas. a [`Worker`] try-decodes an event it recognizes and, off to the
//! side (non-deterministically, in the real world), computes a result and
//! returns the op that carries it back. modules stay pure; only the worker is
//! impure, and the seam lives HERE on the host side, never inside a module
//! crate — so a module never depends on it. the drive loop itself lives with
//! each binary (validator drain, noded, simnode), which all follow the same
//! shape: submit, offer each emitted event to every worker, submit each
//! claimed follow-up as its own block. events a worker does NOT claim are the
//! plain observability stream — one lane, two consumer classes.

use std::collections::VecDeque;

use sdk::{Event, Msg};

/// errors from driving workers.
#[derive(Debug)]
pub enum Error {
    /// the host rejected a submitted op (deterministic, block rolled back).
    Host(sdk::Error),
    /// a node-local block-boundary fault — the host registry is indeterminate
    /// and the caller must fail-stop (see [`crate::FatalError`]).
    Fatal(crate::FatalError),
    /// a worker failed to produce its result.
    Worker(String),
}

impl From<sdk::Error> for Error {
    fn from(e: sdk::Error) -> Self {
        Error::Host(e)
    }
}

impl From<crate::SubmitError> for Error {
    fn from(e: crate::SubmitError) -> Self {
        match e {
            crate::SubmitError::Rejected(e) => Error::Host(e),
            crate::SubmitError::Fatal(f) => Error::Fatal(f),
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Host(e) => write!(f, "host error: {e}"),
            Error::Fatal(e) => write!(f, "{e}"),
            Error::Worker(m) => write!(f, "worker error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// what a [`Worker`] did with an offered event. three-way on purpose:
/// "not my event" (keep offering) and "mine, deliberately not run" (stop
/// offering, nothing to submit) are different outcomes — collapsing them
/// would misreport an assignment-skipped request as an unclaimed drop.
#[derive(Debug)]
pub enum WorkOutcome {
    /// try-decode failed: not this worker's event — offer it to the next.
    NotMine,
    /// this worker's event, handled: submit the follow-up op if present.
    /// `None` is a deliberate no-op — e.g. the work is leased to another
    /// node's key and this host must not spawn it.
    Handled(Option<Msg>),
}

/// a host-owned, NON-DETERMINISTIC worker behind the event seam. given an
/// [`Event`] it recognizes, it does the off-consensus work (an LLM call, a fetch,
/// a commit) and returns the follow-up op that carries the result back through the
/// NORMAL submit path — the oracle-as-transaction. [`WorkOutcome::NotMine`] means
/// try-decode routing failed, so the caller can offer each event to every
/// worker until one claims it.
///
/// the worker never gets a handle to any module: it CANNOT mutate state directly.
/// its only channel back into the state machine is the `Msg` it returns, which the
/// caller submits as an ordinary op. that is the oracle pattern enforced by type.
#[async_trait::async_trait(?Send)]
pub trait Worker {
    async fn run(&self, event: &Event) -> Result<WorkOutcome, Error>;
}

/// the follow-up ops one round of [`offer`] produced, plus the events no worker
/// claimed. `follows` are submitted each as their own block (by the caller, or
/// by [`drive`]); `unclaimed` is the caller's diagnostic stream — a module's
/// only log channel (a wasm guest cannot log), so the caller surfaces it
/// through its own note seam, and a decodable-but-unhandled one means a saga is
/// stuck Pending.
pub struct Offered {
    pub follows: Vec<Msg>,
    pub unclaimed: Vec<Event>,
}

/// offer one block's events to the workers, in order: the first worker to claim
/// an event handles it (`Handled(Some)` yields a follow-up op, `Handled(None)`
/// is a deliberate skip — e.g. work leased to another node's key); `NotMine`
/// falls through to the next worker. a worker ERROR is logged and the event
/// treated as claimed — an errored event is not an unclaimed one, so it is not
/// double-reported. events no worker claims come back in [`Offered::unclaimed`].
///
/// this is the routing the validator drain, the noded submit lane, and the sim
/// once shared three copies of; the lane-flavored bits (which height stamps the
/// notes, how a follow-up is submitted) stay with each caller.
pub async fn offer(workers: &[Box<dyn Worker>], events: Vec<Event>) -> Offered {
    let mut follows = Vec::new();
    let mut unclaimed = Vec::new();
    for eff in events {
        let mut claimed = false;
        for w in workers {
            match w.run(&eff).await {
                Ok(WorkOutcome::Handled(follow)) => {
                    follows.extend(follow);
                    claimed = true;
                    break;
                }
                Ok(WorkOutcome::NotMine) => {}
                Err(err) => {
                    tracing::warn!(
                        target: "ducktape::modules",
                        source = %eff.source,
                        error = %err,
                        "worker failed to handle a module event"
                    );
                    claimed = true;
                    break;
                }
            }
        }
        if !claimed {
            unclaimed.push(eff);
        }
    }
    Offered { follows, unclaimed }
}

/// the caller's own submit path, abstracted so the reactor [`drive`] loop is
/// shared across the block-per-op daemon and the sim's auto mode. a lane
/// applies one worker follow-up op as its OWN block and returns the block's
/// emitted events (offered to the workers for the next round); `pending` reports
/// whether the committed dispatch mailbox still holds an undelivered result a
/// Nudge block must flush. a deterministic rejection is the lane's to absorb
/// (log it, return no events); only a fatal block-boundary fault propagates.
#[async_trait::async_trait(?Send)]
pub trait Lane {
    async fn submit(&mut self, follow: Msg) -> Result<Vec<Event>, Error>;
    async fn pending(&self) -> bool;
}

/// the shared reactor loop. offer `initial` events to the workers, submit each
/// worker follow-up through `lane` (each its own block, its events offered back
/// for the next round), and keep draining while a follow-up OR a pending
/// delivery remains — appending the Nudge that flushes a stranded mailbox when
/// nothing else is queued. returns every unclaimed event across all rounds,
/// for the caller to surface through its own log seam.
pub async fn drive(
    workers: &[Box<dyn Worker>],
    initial: Vec<Event>,
    lane: &mut dyn Lane,
) -> Result<Vec<Event>, Error> {
    let mut queue: VecDeque<Msg> = VecDeque::new();
    let Offered {
        follows,
        mut unclaimed,
    } = offer(workers, initial).await;
    queue.extend(follows);
    loop {
        let Some(follow) = queue.pop_front() else {
            // the never-pop-stack tail: a result committed into the dispatch
            // mailbox delivers in a LATER block, and a settle loop ticks no
            // other blocks — nudge one flush block per stranded delivery.
            if !lane.pending().await {
                break;
            }
            queue.push_back(Msg {
                target: dispatch::DEFAULT_DISPATCH_TARGET.into(),
                payload: dispatch::encode_msg(&dispatch::DispatchMsg::Nudge {}),
            });
            continue;
        };
        let events = lane.submit(follow).await?;
        let Offered {
            follows,
            unclaimed: more,
        } = offer(workers, events).await;
        queue.extend(follows);
        unclaimed.extend(more);
    }
    Ok(unclaimed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    /// claims events whose `source` matches `trigger`, returning `follow`;
    /// everything else is `NotMine`.
    struct StubWorker {
        trigger: &'static str,
        follow: Option<Msg>,
    }

    #[async_trait::async_trait(?Send)]
    impl Worker for StubWorker {
        async fn run(&self, event: &Event) -> Result<WorkOutcome, Error> {
            if event.source == self.trigger {
                Ok(WorkOutcome::Handled(self.follow.clone()))
            } else {
                Ok(WorkOutcome::NotMine)
            }
        }
    }

    /// records every submitted follow-up, echoes a scripted event set back on
    /// each submit (to re-trigger workers), and reports `pending` until it has
    /// absorbed `nudge_budget` submits.
    struct StubLane {
        submitted: Vec<Msg>,
        echo: Vec<Event>,
        nudge_budget: usize,
    }

    #[async_trait::async_trait(?Send)]
    impl Lane for StubLane {
        async fn submit(&mut self, follow: Msg) -> Result<Vec<Event>, Error> {
            self.submitted.push(follow);
            Ok(self.echo.clone())
        }
        async fn pending(&self) -> bool {
            self.submitted.len() < self.nudge_budget
        }
    }

    fn event(source: &str) -> Event {
        Event {
            source: source.into(),
            payload: Vec::new(),
        }
    }

    fn msg(target: &str) -> Msg {
        Msg {
            target: target.into(),
            payload: Vec::new(),
        }
    }

    /// offer routes each event to the FIRST claiming worker in order, collects
    /// its follow-up, and returns the events no worker claimed.
    #[test]
    fn offer_routes_and_reports_unclaimed() {
        let workers: Vec<Box<dyn Worker>> = vec![
            Box::new(StubWorker {
                trigger: "a",
                follow: Some(msg("x")),
            }),
            Box::new(StubWorker {
                trigger: "b",
                follow: Some(msg("y")),
            }),
        ];
        let out = block_on(offer(&workers, vec![event("a"), event("b"), event("c")]));
        assert_eq!(
            out.follows
                .iter()
                .map(|m| m.target.clone())
                .collect::<Vec<_>>(),
            vec!["x", "y"]
        );
        assert_eq!(
            out.unclaimed
                .iter()
                .map(|e| e.source.clone())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
    }

    /// a `Handled(None)` claim is a deliberate skip: the event is neither a
    /// follow-up nor unclaimed.
    #[test]
    fn offer_handled_none_is_a_silent_claim() {
        let workers: Vec<Box<dyn Worker>> = vec![Box::new(StubWorker {
            trigger: "a",
            follow: None,
        })];
        let out = block_on(offer(&workers, vec![event("a")]));
        assert!(out.follows.is_empty());
        assert!(out.unclaimed.is_empty());
    }

    /// drive nudges once per pending delivery when nothing else is queued, and
    /// each Nudge targets the dispatch mailbox flush.
    #[test]
    fn drive_nudges_until_the_mailbox_drains() {
        let workers: Vec<Box<dyn Worker>> = Vec::new();
        let mut lane = StubLane {
            submitted: Vec::new(),
            echo: Vec::new(),
            nudge_budget: 3,
        };
        let unclaimed = block_on(drive(&workers, Vec::new(), &mut lane)).expect("drive");
        assert!(unclaimed.is_empty());
        assert_eq!(
            lane.submitted.len(),
            3,
            "one Nudge per pending check until drained"
        );
        let nudge = dispatch::encode_msg(&dispatch::DispatchMsg::Nudge {});
        for m in &lane.submitted {
            assert_eq!(m.target, dispatch::DEFAULT_DISPATCH_TARGET);
            assert_eq!(m.payload, nudge);
        }
    }

}
