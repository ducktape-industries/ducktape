//! Programs whose `execute` is a Rust future: an `ask` parks the frame until the host resumes
//! it with the ticket's answer, in a later run on a restored image of this memory.
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::context::{Execute, host};
use crate::{Env, HostOp, HostReply, Invocation, ProgramCall, ProgramReply, Refusal, reason};

#[link(wasm_import_module = "ducktape")]
unsafe extern "C" {
    fn host_park();
}

pub trait AwaitingProgram: 'static {
    fn execute(env: Env, payload: Vec<u8>) -> impl Future<Output = ProgramReply> + 'static;
}

type Root = Pin<Box<dyn Future<Output = ProgramReply>>>;

thread_local! {
    static ROOT: RefCell<Option<Root>> = const { RefCell::new(None) };
    static ANSWERS: RefCell<BTreeMap<u64, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
    static ENV: RefCell<Option<Env>> = const { RefCell::new(None) };
}

pub fn env() -> Env {
    ENV.with(|env| env.borrow().clone())
        .expect("every run sets the env before it polls")
}

pub fn ctx() -> Execute {
    Execute::new()
}

pub fn ask(request: Vec<u8>) -> Ask {
    Ask::Unsent(request)
}

pub enum Ask {
    Unsent(Vec<u8>),
    Waiting(u64),
}

fn issue(request: Vec<u8>) -> Result<u64, Refusal> {
    match host(&HostOp::Await(request)) {
        HostReply::Ticket(ticket) => Ok(ticket),
        HostReply::Refused(refusal) => Err(refusal),
        other => panic!("host answered {other:?} where a ticket was expected"),
    }
}

impl Future for Ask {
    type Output = Result<Vec<u8>, Refusal>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let ticket = match this {
            Ask::Waiting(ticket) => *ticket,
            Ask::Unsent(request) => match issue(core::mem::take(request)) {
                Ok(ticket) => {
                    *this = Ask::Waiting(ticket);
                    ticket
                }
                Err(refusal) => return Poll::Ready(Err(refusal)),
            },
        };
        match ANSWERS.with(|answers| answers.borrow_mut().remove(&ticket)) {
            Some(answer) => Poll::Ready(Ok(answer)),
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
        ProgramCall::Execute(payload) => poll(Box::pin(P::execute(env, payload))),
        ProgramCall::Resume { ticket, answer } => {
            ANSWERS.with(|answers| answers.borrow_mut().insert(ticket, answer));
            let Some(root) = ROOT.with(|parked| parked.borrow_mut().take()) else {
                return Err(Refusal::new(reason::PROTOCOL, "nothing is parked"));
            };
            poll(root)
        }
        ProgramCall::Init(_) => Ok(()),
        ProgramCall::Query(_) | ProgramCall::Hook(_) => Err(Refusal::new(
            reason::UNSUPPORTED,
            "an awaiting program only executes",
        )),
    }
}

pub fn call<P: AwaitingProgram>(ptr: u32, len: u32) -> u64 {
    let request = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
    let reply = match abi::decode::<Invocation>(&request) {
        Ok(invocation) => run::<P>(invocation),
        Err(fault) => Err(Refusal::new(reason::PROTOCOL, fault.sentence)),
    };
    crate::exports::leak(abi::encode(&reply))
}
