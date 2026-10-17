//! A program others call: it answers a call at once or with who called it, refuses it, or defers
//! it and answers in a later submission that resolves the ticket through the awaits role.
use abi::role::awaits::{Answer, Ticket};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Ask {
    Now(Vec<u8>),
    Sender,
    Later,
    Refuse(String),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Answer { ticket: Ticket, answer: Answer },
}

pub fn owed_key(ticket: Ticket) -> Vec<u8> {
    [b"owed/".as_slice(), &ticket.to_be_bytes()].concat()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::{Cause, Env, Refusal, reason};
    use program::{Execute, Program, Query, Reads};

    use crate::{Ask, Op, owed_key};

    struct Oracle;

    fn called(ctx: &mut Execute, env: &Env, ticket: u64, payload: &[u8]) -> Result<(), Refusal> {
        match abi::decode(payload)? {
            Ask::Now(bytes) => {
                ctx.output(bytes);
                Ok(())
            }
            Ask::Sender => {
                ctx.output(abi::encode(&env.sender));
                Ok(())
            }
            Ask::Later => {
                ctx.defer()?;
                ctx.set(owed_key(ticket), Vec::new());
                Ok(())
            }
            Ask::Refuse(sentence) => Err(Refusal::new(reason::INVALID_INPUT, sentence)),
        }
    }

    fn submitted(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
        let Op::Answer { ticket, answer } = abi::decode(payload)?;
        let owed = ctx.get(owed_key(ticket)).is_some();
        if !owed {
            return Err(Refusal::new(
                reason::NOT_FOUND,
                format!("nothing owed on {ticket}"),
            ));
        }
        ctx.delete(owed_key(ticket));
        ctx.resolve(env, ticket, answer)
    }

    impl Program for Oracle {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match env.cause {
                Cause::Call { ticket } => called(ctx, env, ticket, payload),
                _ => submitted(ctx, env, payload),
            }
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            ctx.respond(abi::encode(&ctx.get(request)));
            Ok(())
        }
    }

    program::program!(Oracle);
}
