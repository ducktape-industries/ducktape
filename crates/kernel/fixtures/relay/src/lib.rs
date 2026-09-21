use abi::ItemRef;

pub const FAIL: &[u8] = b"fail";

pub fn sent(item: &ItemRef) -> Vec<u8> {
    key("sent", item)
}

pub fn got(item: &ItemRef) -> Vec<u8> {
    key("got", item)
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
    use abi::{Cause, Env, ItemRef, Message, Refusal, reason};
    use guest::{Execute, Program, Query, Reads};

    use crate::{FAIL, done, got, sent};

    struct Relay;

    impl Program for Relay {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match &env.cause {
                Cause::Direct => send(ctx, payload),
                Cause::Delivery(item) => receive(ctx, item, payload),
                Cause::Completion { item, outcome } => {
                    ctx.set(done(item), abi::encode(outcome));
                    Ok(())
                }
            }
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            ctx.respond(abi::encode(&ctx.get(request)));
            Ok(())
        }
    }

    fn send(ctx: &mut Execute, payload: &[u8]) -> Result<(), Refusal> {
        let message: Message = abi::decode(payload)?;
        let body = message.payload.clone();
        let item = match message.reply {
            true => ctx.call(message.target, message.payload),
            false => ctx.emit(message.target, message.payload),
        };
        ctx.set(sent(&item), body);
        ctx.output(abi::encode(&item));
        Ok(())
    }

    fn receive(ctx: &mut Execute, item: &ItemRef, payload: &[u8]) -> Result<(), Refusal> {
        let asked_to_fail = payload == FAIL;
        if asked_to_fail {
            return Err(Refusal::new(reason::INVALID_INPUT, "asked to fail"));
        }
        ctx.set(got(item), payload.to_vec());
        ctx.event(payload.to_vec());
        ctx.output(payload.iter().rev().copied().collect::<Vec<u8>>());
        Ok(())
    }

    guest::program!(Relay);
}
