use abi::{ItemRef, Message};
use borsh::{BorshDeserialize, BorshSerialize};

/// What relay does with a payload, sent to it directly or as a message.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Script {
    /// Keeps the bytes under [`got`], events them and outputs them reversed.
    Note(Vec<u8>),
    /// Emits each message (its payload a `Script`), keeps each under
    /// [`sent`] and outputs their items.
    Send(Vec<Message>),
    /// Refuses. A sender that wanted a reply keeps the outcome under
    /// [`done`], and refuses the reply too when the failure was `loud`.
    Fail { sentence: String, loud: bool },
}

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
    use abi::{Cause, Env, ItemRef, Outcome, Refusal, reason};
    use guest::{Execute, Program, Query, Reads};

    use crate::{Script, done, got, sent};

    struct Relay;

    impl Program for Relay {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match &env.cause {
                Cause::Direct => run(ctx, None, payload),
                Cause::Message(item) => run(ctx, Some(item), payload),
                Cause::Completion { item, outcome } => {
                    ctx.set(done(item), abi::encode(outcome));
                    let asked: Option<Script> =
                        ctx.get(sent(item)).and_then(|b| abi::decode(&b).ok());
                    let loud = matches!(asked, Some(Script::Fail { loud: true, .. }));
                    match outcome {
                        Outcome::Rejected(refusal) if loud => Err(refusal.clone()),
                        _ => Ok(()),
                    }
                }
            }
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            ctx.respond(abi::encode(&ctx.get(request)));
            Ok(())
        }
    }

    fn run(ctx: &mut Execute, item: Option<&ItemRef>, payload: &[u8]) -> Result<(), Refusal> {
        match abi::decode(payload)? {
            Script::Note(bytes) => {
                if let Some(item) = item {
                    ctx.set(got(item), bytes.clone());
                }
                ctx.event(bytes.clone());
                ctx.output(bytes.iter().rev().copied().collect::<Vec<u8>>());
            }
            Script::Send(messages) => {
                let mut items = Vec::new();
                for message in messages {
                    let body = message.payload.clone();
                    let item = ctx.emit(message.target, message.payload, message.reply);
                    ctx.set(sent(&item), body);
                    items.push(item);
                }
                ctx.output(abi::encode(&items));
            }
            Script::Fail { sentence, .. } => {
                return Err(Refusal::new(reason::INVALID_INPUT, sentence));
            }
        }
        Ok(())
    }

    guest::program!(Relay);
}
