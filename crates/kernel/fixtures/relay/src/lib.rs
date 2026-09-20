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
    use abi::{Cause, Message, Refusal, reason};
    use guest::Program;

    use crate::{FAIL, done, got, sent};

    struct Relay;

    impl Program for Relay {
        fn execute(payload: &[u8]) -> Result<(), Refusal> {
            match guest::env().cause {
                Cause::Direct => send(payload),
                Cause::Delivery(item) => receive(&item, payload),
                Cause::Completion { item, outcome } => {
                    guest::set(done(&item), abi::encode(&outcome));
                    Ok(())
                }
            }
        }

        fn query(request: &[u8]) -> Result<Vec<u8>, Refusal> {
            Ok(abi::encode(&guest::get(request)))
        }
    }

    fn send(payload: &[u8]) -> Result<(), Refusal> {
        let message: Message = abi::decode(payload)?;
        let body = message.payload.clone();
        let item = match message.reply {
            true => guest::call(message.target, message.payload),
            false => guest::emit(message.target, message.payload),
        };
        guest::set(sent(&item), body);
        guest::output(abi::encode(&item));
        Ok(())
    }

    fn receive(item: &abi::ItemRef, payload: &[u8]) -> Result<(), Refusal> {
        let asked_to_fail = payload == FAIL;
        if asked_to_fail {
            return Err(Refusal::new(reason::INVALID_INPUT, "asked to fail"));
        }
        guest::set(got(item), payload.to_vec());
        guest::event(payload.to_vec());
        guest::output(payload.iter().rev().copied().collect::<Vec<u8>>());
        Ok(())
    }

    guest::program!(Relay);
}
