//! The guest's side of the request/response channel.
//!
//! [`request`] asks once and resolves on the first answer; [`subscribe`]
//! asks once and yields every answer until the host closes the stream. Both
//! hand the host a [`Request`] through the next frame. The guest never
//! blocks: the driver polls the app's tasks on every tick, so a task waiting
//! on the host simply stays pending until a response event arrives.
//!
//! [`notify`] is the third shape: it asks and never listens, for things whose
//! answer nobody wants ([`log`]).
//!
//! A request's `kind` is `<capability>.<operation>`. The host refuses a
//! capability the app's manifest did not declare, and the refusal arrives
//! as the `Err` of the answer — an ordinary error the app's handler routes.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::Stream;

use crate::wire::Request;

/// What one answer carries; the host's refusal is the `Err`.
pub type Answer = Result<Vec<u8>, String>;

#[derive(Default)]
struct Slot {
    answers: VecDeque<Answer>,
    closed: bool,
    waker: Option<Waker>,
}

#[derive(Default)]
struct Registry {
    next_id: u64,
    outbox: Vec<Request>,
    pending: HashMap<u64, Arc<Mutex<Slot>>>,
    cancels: Vec<u64>,
}

impl Registry {
    fn ask(&mut self, kind: &str, payload: &[u8]) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.outbox.push(Request {
            id,
            kind: kind.to_string(),
            payload: payload.to_vec(),
        });
        id
    }
}

thread_local! {
    // One registry per thread = one per driver: a wasm module has one
    // thread, and every native test drives its own app on its own thread.
    static REGISTRY: RefCell<Registry> = RefCell::default();
}

fn open(kind: &str, payload: &[u8]) -> (u64, Arc<Mutex<Slot>>) {
    let slot = Arc::new(Mutex::new(Slot::default()));
    let id = REGISTRY.with_borrow_mut(|registry| {
        let id = registry.ask(kind, payload);
        registry.pending.insert(id, slot.clone());
        id
    });
    (id, slot)
}

/// Stops waiting for `id`. Still pending means the host is still working on
/// it and has to be told; already fulfilled means there is nothing to cancel.
/// Runs from a `Drop`, so a thread tearing down its registry is not an error.
fn close(id: u64) {
    let _ = REGISTRY.try_with(|registry| {
        let mut registry = registry.borrow_mut();
        if registry.pending.remove(&id).is_some() {
            registry.cancels.push(id);
        }
    });
}

/// Asks the host for one thing.
pub fn request(kind: &str, payload: &[u8]) -> Response {
    let (id, slot) = open(kind, payload);
    Response { id, slot }
}

/// Asks the host for a stream of things — timer ticks, bus messages.
pub fn subscribe(kind: &str, payload: &[u8]) -> Subscription {
    let (id, slot) = open(kind, payload);
    Subscription { id, slot }
}

/// Tells the host something and keeps no slot for the answer, which is
/// therefore dropped when it comes.
pub fn notify(kind: &str, payload: &[u8]) {
    REGISTRY.with_borrow_mut(|registry| registry.ask(kind, payload));
}

/// Prints from inside a module: `println!` has nowhere to go in wasm.
pub fn log(message: impl AsRef<str>) {
    notify("host.log", message.as_ref().as_bytes());
}

/// The host's colour mode, now and whenever it changes: every item is
/// `light` or `dark`. Needs no capability — an app that cannot follow the
/// host's dark mode is the one thing every app should be allowed to fix.
pub fn theme() -> Subscription {
    subscribe("host.theme", &[])
}

/// The host's eventual answer to a [`request`].
pub struct Response {
    id: u64,
    slot: Arc<Mutex<Slot>>,
}

impl Drop for Response {
    fn drop(&mut self) {
        close(self.id);
    }
}

impl Future for Response {
    type Output = Answer;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Answer> {
        let mut slot = self.slot.lock().expect("response slot");
        match slot.answers.pop_front() {
            Some(answer) => Poll::Ready(answer),
            None if slot.closed => Poll::Ready(Err("the host closed the request".into())),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// Every answer the host sends to a [`subscribe`], until it closes.
pub struct Subscription {
    id: u64,
    slot: Arc<Mutex<Slot>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        close(self.id);
    }
}

impl Stream for Subscription {
    type Item = Answer;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Answer>> {
        let mut slot = self.slot.lock().expect("subscription slot");
        match slot.answers.pop_front() {
            Some(answer) => Poll::Ready(Some(answer)),
            None if slot.closed => Poll::Ready(None),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// Everything asked since the last frame, in order.
pub(crate) fn drain_outbox() -> Vec<Request> {
    REGISTRY.with_borrow_mut(|registry| std::mem::take(&mut registry.outbox))
}

/// Everything abandoned since the last frame.
pub(crate) fn drain_cancels() -> Vec<u64> {
    REGISTRY.with_borrow_mut(|registry| std::mem::take(&mut registry.cancels))
}

/// Delivers one answer; an id nobody waits for is dropped.
pub(crate) fn fulfill(id: u64, answer: Answer, done: bool) {
    let slot = REGISTRY.with_borrow_mut(|registry| {
        if done {
            registry.pending.remove(&id)
        } else {
            registry.pending.get(&id).cloned()
        }
    });
    if let Some(slot) = slot {
        let mut slot = slot.lock().expect("answer slot");
        slot.answers.push_back(answer);
        slot.closed |= done;
        if let Some(waker) = slot.waker.take() {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_request_is_cancelled_once() {
        let response = request("host.echo", b"hi");
        let id = drain_outbox()[0].id;
        assert!(drain_cancels().is_empty());

        drop(response);
        assert_eq!(drain_cancels(), vec![id]);
        assert!(drain_cancels().is_empty());
    }

    #[test]
    fn an_answered_request_cancels_nothing() {
        let response = request("host.echo", b"hi");
        let id = drain_outbox()[0].id;
        fulfill(id, Ok(Vec::new()), true);

        drop(response);
        assert!(drain_cancels().is_empty());
    }

    #[test]
    fn notify_asks_and_keeps_no_slot() {
        notify("host.log", b"hello");
        let sent = drain_outbox();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, "host.log");
        // Nothing waits for it: the answer is dropped, not a panic.
        fulfill(sent[0].id, Ok(Vec::new()), true);
        assert!(drain_cancels().is_empty());
    }
}
