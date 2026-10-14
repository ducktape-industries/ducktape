//! A price feed whose refresh awaits provider: it asks for a pair's price, parks until the
//! elected node's answer resolves the call in a later block, and keeps the quote it got.
use borsh::{BorshDeserialize, BorshSerialize};

pub const PROVIDER: &str = "provider";
pub const PRICES: &str = "prices";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Refresh { pair: String },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Ask {
    pub service: String,
    pub input: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Quote {
    pub price: u64,
    pub asked_at: u64,
    pub answered_at: u64,
}

pub fn quote_key(pair: &str) -> Vec<u8> {
    [b"quote/".as_slice(), pair.as_bytes()].concat()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, ProgramReply, Refusal, reason};
    use program::awaiting::{AwaitingProgram, call, ctx, env};

    use crate::{Ask, Op, PRICES, PROVIDER, Quote, quote_key};

    struct Feed;

    impl AwaitingProgram for Feed {
        async fn execute(asked: Env, payload: Vec<u8>) -> ProgramReply {
            let Op::Refresh { pair } = abi::decode(&payload)
                .map_err(|fault| Refusal::new(reason::INVALID_INPUT, fault.sentence))?;
            let ask = Ask {
                service: PRICES.into(),
                input: pair.clone().into_bytes(),
            };
            let price: u64 = call(PROVIDER, &ask).await?;
            let quote = Quote {
                price,
                asked_at: asked.height,
                answered_at: env().height,
            };
            ctx().put(quote_key(&pair), &quote);
            Ok(())
        }
    }

    program::awaiting_program!(Feed);
}
