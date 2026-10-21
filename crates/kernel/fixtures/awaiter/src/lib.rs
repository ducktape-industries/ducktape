//! A program whose execute awaits: it calls `target` with each ask, as itself or as an account it
//! runs, one after another or all together, keeps each answer, and outputs them with the height it
//! finished at, the client it read then and the heap it kept.
use abi::role::awaits::Answer;
use abi::role::identity::AccountNumber;
use borsh::{BorshDeserialize, BorshSerialize};

pub const ANSWERS: &[u8] = b"answers";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Plan {
    pub target: String,
    pub acting: Option<AccountNumber>,
    pub asks: Vec<Vec<u8>>,
    pub together: bool,
    pub kept: u32,
    pub fail: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Finished {
    pub answers: Vec<Answer>,
    pub height: u64,
    pub kept: u32,
    pub sum: u8,
    pub client: Option<String>,
}

#[cfg(target_arch = "wasm32")]
mod program {
    use core::future::{Future, poll_fn};
    use core::pin::Pin;
    use core::task::Poll;

    use abi::Env;
    use abi::role::access::{Need, Resource};
    use abi::role::awaits::Answer;
    use abi::{ProgramReply, Refusal, reason};
    use program::awaiting::{AwaitingProgram, Call, Ctx};
    use program::{Query, Reads as _};

    use crate::{ANSWERS, Finished, Plan};

    struct Awaiter;

    fn ask(ctx: &Ctx, plan: &Plan, payload: Vec<u8>) -> Call {
        let target = plan.target.clone();
        match plan.acting {
            Some(account) => ctx.call_raw_as(account, target, payload),
            None => ctx.call_raw(target, payload),
        }
    }

    async fn together(calls: Vec<Call>) -> Vec<Answer> {
        let mut calls: Vec<(Call, Option<Answer>)> =
            calls.into_iter().map(|call| (call, None)).collect();
        poll_fn(|context| {
            for (call, answer) in calls.iter_mut() {
                if answer.is_none()
                    && let Poll::Ready(ready) = Pin::new(call).poll(context)
                {
                    *answer = Some(ready);
                }
            }
            let all = calls.iter().all(|(_, answer)| answer.is_some());
            match all {
                true => Poll::Ready(()),
                false => Poll::Pending,
            }
        })
        .await;
        calls.into_iter().filter_map(|(_, answer)| answer).collect()
    }

    async fn run(mut ctx: Ctx, plan: Plan) -> ProgramReply {
        let kept: Vec<u8> = (0..plan.kept).map(|i| i as u8).collect();
        let answers = match plan.together {
            true => {
                let calls = plan
                    .asks
                    .iter()
                    .map(|payload| ask(&ctx, &plan, payload.clone()))
                    .collect();
                together(calls).await
            }
            false => {
                let mut answers = Vec::new();
                for payload in plan.asks.clone() {
                    let answer = ask(&ctx, &plan, payload).await;
                    answers.push(answer);
                    ctx.put(ANSWERS, &answers);
                }
                answers
            }
        };
        ctx.put(ANSWERS, &answers);
        if plan.fail {
            return Err(Refusal::new(reason::INVALID_INPUT, "asked to fail"));
        }
        let finished = Finished {
            answers,
            height: ctx.env().height,
            kept: kept.len() as u32,
            sum: kept.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            client: ctx.client(),
        };
        ctx.output(abi::encode(&finished));
        Ok(())
    }

    impl AwaitingProgram for Awaiter {
        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
        }

        async fn execute(ctx: Ctx, payload: Vec<u8>) -> ProgramReply {
            let plan: Plan = abi::decode(&payload)
                .map_err(|fault| Refusal::new(reason::INVALID_INPUT, fault.sentence))?;
            run(ctx, plan).await
        }
    }

    program::awaiting_program!(Awaiter);
}
