//! A program for the kernel's tests that sends each message it is given as a foreign user, keeps
//! what it sent, and keeps each reply's outcome without refusing it.
use abi::{Foreign, ItemRef, Message};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Sent {
    pub foreign: Foreign,
    pub message: Message,
}

pub fn sent(item: &ItemRef) -> Vec<u8> {
    key("sent", item)
}

pub fn done(item: &ItemRef) -> Vec<u8> {
    key("done", item)
}

fn key(kind: &str, item: &ItemRef) -> Vec<u8> {
    let mut key = format!("{kind}/{}/", item.source).into_bytes();
    key.extend_from_slice(&item.item.to_be_bytes());
    key
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::{Cause, Env, Refusal};
    use program::{Execute, Program, Query, Reads};

    use crate::{Sent, done, sent};

    struct CrosspondProbe;

    impl Program for CrosspondProbe {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            if let Cause::Completion { item, outcome } = &env.cause {
                ctx.set(done(item), abi::encode(outcome));
                return Ok(());
            }
            let messages: Vec<Sent> = abi::decode(payload)?;
            let mut items = Vec::new();
            for Sent { foreign, message } in messages {
                let body = message.payload.clone();
                let item = ctx.emit_foreign(foreign, message);
                ctx.set(sent(&item), body);
                items.push(item);
            }
            ctx.output(abi::encode(&items));
            Ok(())
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

    program::program!(CrosspondProbe);
}
