use abi::{Env, HostOp, HostReply};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Step {
    Op(HostOp),
    Env,
    Spin,
    Grow(u32),
    Fail(String),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Host(HostReply),
    Env(Env),
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, Refusal};
    use guest::{Execute, Program, Query, Reads};

    use crate::{Reply, Step};

    struct Probe;

    impl Program for Probe {
        fn init(ctx: &mut Execute, env: &Env, params: &[u8]) -> Result<(), Refusal> {
            Self::execute(ctx, env, params)
        }

        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let replies = run(ctx, env, payload)?;
            ctx.output(abi::encode(&replies));
            Ok(())
        }

        fn query(ctx: &mut Query, env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let replies = run(ctx, env, request)?;
            ctx.respond(abi::encode(&replies));
            Ok(())
        }
    }

    fn run(ctx: &impl Reads, env: &Env, script: &[u8]) -> Result<Vec<Reply>, Refusal> {
        let steps: Vec<Step> = abi::decode(script)?;
        let mut replies = Vec::new();
        for step in steps {
            match step {
                Step::Op(op) => replies.push(Reply::Host(ctx.host(&op))),
                Step::Env => replies.push(Reply::Env(env.clone())),
                Step::Spin => loop {
                    core::hint::black_box(());
                },
                Step::Grow(pages) => {
                    let bytes = vec![1u8; pages as usize * 65536];
                    core::hint::black_box(&bytes);
                }
                Step::Fail(sentence) => return Err(Refusal::new("probe", sentence)),
            }
        }
        Ok(replies)
    }

    guest::program!(Probe);
}
