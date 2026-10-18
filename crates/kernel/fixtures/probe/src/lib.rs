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

pub const HOOK: &[u8] = b"#hook";

// The probe's tables, borsh `Vec<Table>`: `s/` shared, and `o/` owned of the
// kind `probe.item`; every other key is owned with no kind.
pub const TABLES: [u8; 32] = [
    2, 0, 0, 0, 2, 0, 0, 0, 115, 47, 0, 2, 0, 0, 0, 111, 47, 1, 10, 0, 0, 0, 112, 114, 111, 98,
    101, 46, 105, 116, 101, 109,
];
pub const NEEDS: &[u8] = b"#needs";

pub fn resource_key(key: &str) -> Vec<u8> {
    format!("#resource/{key}").into_bytes()
}

pub fn seen(height: u64, position: u32) -> Vec<u8> {
    [
        b"#seen/".as_slice(),
        &height.to_be_bytes(),
        &position.to_be_bytes(),
    ]
    .concat()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::{Env, Refusal, hook};
    use program::{Execute, Program, Query, Reads};

    use crate::{HOOK, NEEDS, Reply, Step, resource_key, seen};

    struct Probe;

    #[used]
    #[unsafe(link_section = "ducktape.tables")]
    static TABLES: [u8; crate::TABLES.len()] = crate::TABLES;

    impl Program for Probe {
        fn init(ctx: &mut Execute, env: &Env, params: &[u8]) -> Result<(), Refusal> {
            Self::execute(ctx, env, params)
        }

        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let replies = run(ctx, env, payload)?;
            ctx.output(abi::encode(&replies));
            Ok(())
        }

        fn access(ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(ctx.record(NEEDS)?.unwrap_or_default())
        }

        fn resource(ctx: &mut Query, _env: &Env, key: &str) -> Result<Resource, Refusal> {
            Ok(ctx.record(resource_key(key))?.unwrap_or_default())
        }

        fn query(ctx: &mut Query, env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let replies = run(ctx, env, request)?;
            ctx.respond(abi::encode(&replies));
            Ok(())
        }

        fn hook(ctx: &mut Execute, env: &Env, record: hook::Record) -> Result<(), Refusal> {
            if let Some(script) = ctx.get(HOOK) {
                run(ctx, env, &script)?;
            }
            ctx.set(seen(record.height, record.position), abi::encode(&record));
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

    program::program!(Probe);
}
