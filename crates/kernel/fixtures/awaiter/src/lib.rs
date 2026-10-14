//! A program whose execute awaits the host: it asks twice, one after the other or together,
//! writes each answer, and outputs both with the height it finished at and the heap it kept.
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Plan {
    pub kept: u32,
    pub together: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Finished {
    pub first: Vec<u8>,
    pub second: Vec<u8>,
    pub height: u64,
    pub kept: u32,
    pub sum: u8,
}

#[cfg(target_arch = "wasm32")]
mod program {
    use core::future::{Future, poll_fn};
    use core::pin::Pin;
    use core::task::Poll;

    use abi::{Env, ProgramReply, Refusal, reason};
    use program::awaiting::{Ask, AwaitingProgram, ask, ctx, env};

    use crate::{Finished, Plan};

    struct Awaiter;

    type Answer = Result<Vec<u8>, Refusal>;

    async fn together(mut a: Ask, mut b: Ask) -> (Answer, Answer) {
        let mut first = None;
        let mut second = None;
        poll_fn(|context| {
            if first.is_none()
                && let Poll::Ready(answer) = Pin::new(&mut a).poll(context)
            {
                first = Some(answer);
            }
            if second.is_none()
                && let Poll::Ready(answer) = Pin::new(&mut b).poll(context)
            {
                second = Some(answer);
            }
            match (first.is_some(), second.is_some()) {
                (true, true) => Poll::Ready(()),
                _ => Poll::Pending,
            }
        })
        .await;
        (first.unwrap(), second.unwrap())
    }

    impl AwaitingProgram for Awaiter {
        async fn execute(_env: Env, payload: Vec<u8>) -> ProgramReply {
            let plan: Plan = abi::decode(&payload)
                .map_err(|fault| Refusal::new(reason::INVALID_INPUT, fault.sentence))?;
            let kept: Vec<u8> = (0..plan.kept).map(|i| i as u8).collect();
            let (first, second) = match plan.together {
                true => {
                    let (first, second) =
                        together(ask(b"first".to_vec()), ask(b"second".to_vec())).await;
                    (first?, second?)
                }
                false => {
                    let first = ask(b"first".to_vec()).await?;
                    ctx().set(b"first".to_vec(), first.clone());
                    let second = ask(b"second".to_vec()).await?;
                    (first, second)
                }
            };
            ctx().set(b"second".to_vec(), second.clone());
            let finished = Finished {
                first,
                second,
                height: env().height,
                kept: kept.len() as u32,
                sum: kept.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            };
            ctx().output(abi::encode(&finished));
            Ok(())
        }
    }

    program::awaiting_program!(Awaiter);
}
