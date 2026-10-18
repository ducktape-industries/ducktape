//! An ownership program for the kernel's tests: it allows each run the kernel's guard reports,
//! keeping the last guard and a count, refuses a key a test denies, and can be told to send a
//! message while it guards, which the kernel refuses.
use abi::ProgramId;
use abi::role::ownership::Guard;
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Op {
    Guard(Guard),
    Deny { program: ProgramId, key: Vec<u8> },
    Chatty,
}

pub const LAST: &[u8] = b"last";
pub const CALLS: &[u8] = b"calls";
pub const CHATTY: &[u8] = b"chatty";

pub fn denied(program: &str, key: &[u8]) -> Vec<u8> {
    [b"deny/".as_slice(), program.as_bytes(), b"/", key].concat()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::role::ownership::Guard;
    use abi::{Env, Origin, Refusal, reason};
    use program::{Execute, Program, Query, Reads};

    use crate::{CALLS, CHATTY, LAST, Op, denied};

    struct Ownership;

    fn guard(ctx: &mut Execute, env: &Env, guard: Guard) -> Result<(), Refusal> {
        let by_the_kernel = env.origin == Origin::System;
        if !by_the_kernel {
            return Err(Refusal::new(reason::UNAUTHORIZED, "the kernel guards"));
        }
        for write in &guard.writes {
            let refused = ctx.get(denied(&guard.program, &write.key)).is_some();
            if refused {
                return Err(Refusal::new(
                    reason::UNAUTHORIZED,
                    format!("{:?} may not write {:?}", guard.sender, write.key),
                ));
            }
        }
        let calls: u64 = ctx.record(CALLS)?.unwrap_or(0);
        ctx.put(CALLS, &(calls + 1));
        ctx.put(LAST, &guard);
        if ctx.get(CHATTY).is_some() {
            ctx.emit(env.me.clone(), Vec::new(), false);
        }
        Ok(())
    }

    impl Program for Ownership {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Op::Guard(asked) => guard(ctx, env, asked),
                Op::Deny { program, key } => {
                    ctx.set(denied(&program, &key), Vec::new());
                    Ok(())
                }
                Op::Chatty => {
                    ctx.set(CHATTY, Vec::new());
                    Ok(())
                }
            }
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
        }

        fn query(_ctx: &mut Query, _env: &Env, _request: &[u8]) -> Result<(), Refusal> {
            Ok(())
        }
    }

    program::program!(Ownership);
}
