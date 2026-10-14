//! Programs whose `execute` is a Rust future: a `call` to another program parks the run until
//! that program's answer resumes it, in a later run on a restored image of this memory.
use core::future::Future;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::cell::RefCell;
use std::collections::BTreeMap;

use abi::role::awaits::{Answer, Ticket};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::context::{Execute, Query, host};
use crate::{
    Env, HostOp, HostReply, Invocation, ProgramCall, ProgramId, ProgramReply, Refusal, reason,
};

#[link(wasm_import_module = "ducktape")]
unsafe extern "C" {
    fn host_park();
}

pub trait AwaitingProgram: 'static {
    fn execute(ctx: Ctx, payload: Vec<u8>) -> impl Future<Output = ProgramReply> + 'static;

    fn query(_ctx: &mut Query, _env: &Env, _request: &[u8]) -> ProgramReply {
        Err(Refusal::new(
            reason::UNSUPPORTED,
            "this program answers no query",
        ))
    }
}

type Root = Pin<Box<dyn Future<Output = ProgramReply>>>;

thread_local! {
    static ROOT: RefCell<Option<Root>> = const { RefCell::new(None) };
    static ANSWERS: RefCell<BTreeMap<Ticket, Answer>> = const { RefCell::new(BTreeMap::new()) };
    static ENV: RefCell<Option<Env>> = const { RefCell::new(None) };
}

pub struct Ctx {
    execute: Execute,
}

impl Deref for Ctx {
    type Target = Execute;

    fn deref(&self) -> &Execute {
        &self.execute
    }
}

impl DerefMut for Ctx {
    fn deref_mut(&mut self) -> &mut Execute {
        &mut self.execute
    }
}

impl Ctx {
    pub fn env(&self) -> Env {
        ENV.with(|env| env.borrow().clone())
            .expect("every run sets its env before it polls")
    }

    pub fn call_raw(&self, target: impl Into<ProgramId>, payload: Vec<u8>) -> Call {
        Call::Unsent {
            target: target.into(),
            payload,
        }
    }

    pub async fn call<O: BorshSerialize, R: BorshDeserialize>(
        &self,
        target: impl Into<ProgramId>,
        op: &O,
    ) -> Result<R, Refusal> {
        let answer = self.call_raw(target, abi::encode(op)).await?;
        abi::decode(&answer).map_err(|fault| Refusal::new(reason::PROTOCOL, fault.sentence))
    }
}

pub enum Call {
    Unsent { target: ProgramId, payload: Vec<u8> },
    Waiting(Ticket),
}

fn issue(target: ProgramId, payload: Vec<u8>) -> Result<Ticket, Refusal> {
    match host(&HostOp::Await { target, payload }) {
        HostReply::Ticket(ticket) => Ok(ticket),
        HostReply::Refused(refusal) => Err(refusal),
        other => panic!("host answered {other:?} where a ticket was expected"),
    }
}

impl Future for Call {
    type Output = Answer;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Answer> {
        let this = self.get_mut();
        let ticket = match this {
            Call::Waiting(ticket) => *ticket,
            Call::Unsent { target, payload } => {
                let issued = issue(core::mem::take(target), core::mem::take(payload));
                match issued {
                    Ok(ticket) => {
                        *this = Call::Waiting(ticket);
                        ticket
                    }
                    Err(refusal) => return Poll::Ready(Err(refusal)),
                }
            }
        };
        match ANSWERS.with(|answers| answers.borrow_mut().remove(&ticket)) {
            Some(answer) => Poll::Ready(answer),
            None => Poll::Pending,
        }
    }
}

fn poll(mut root: Root) -> ProgramReply {
    let mut context = Context::from_waker(Waker::noop());
    match root.as_mut().poll(&mut context) {
        Poll::Ready(reply) => reply,
        Poll::Pending => {
            ROOT.with(|parked| *parked.borrow_mut() = Some(root));
            unsafe { host_park() };
            Ok(())
        }
    }
}

fn run<P: AwaitingProgram>(invocation: Invocation) -> ProgramReply {
    let Invocation { env, call } = invocation;
    ENV.with(|current| *current.borrow_mut() = Some(env.clone()));
    match call {
        ProgramCall::Execute(payload) => {
            let ctx = Ctx {
                execute: Execute::new(),
            };
            poll(Box::pin(P::execute(ctx, payload)))
        }
        ProgramCall::Resume { ticket, answer } => {
            ANSWERS.with(|answers| answers.borrow_mut().insert(ticket, answer));
            let Some(root) = ROOT.with(|parked| parked.borrow_mut().take()) else {
                return Err(Refusal::new(reason::PROTOCOL, "nothing is parked"));
            };
            poll(root)
        }
        ProgramCall::Query(request) => P::query(&mut Query::new(), &env, &request),
        ProgramCall::Init(_) => Ok(()),
        ProgramCall::Hook(_) => Err(Refusal::new(
            reason::UNSUPPORTED,
            "an awaiting program is no hook",
        )),
    }
}

pub fn entry<P: AwaitingProgram>(ptr: u32, len: u32) -> u64 {
    let request = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
    let reply = match abi::decode::<Invocation>(&request) {
        Ok(invocation) => run::<P>(invocation),
        Err(fault) => Err(Refusal::new(reason::PROTOCOL, fault.sentence)),
    };
    crate::exports::leak(abi::encode(&reply))
}
