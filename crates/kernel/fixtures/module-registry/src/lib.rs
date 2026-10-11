use abi::{ProgramId, role::registry};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Change {
    Set(registry::Entry),
    Remove(ProgramId),
}

pub fn key(program: &str) -> Vec<u8> {
    format!("p/{program}").into_bytes()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, Refusal, Scan, role::registry};
    use guest::{Execute, Program, Query, Reads};

    use crate::{Change, key};

    struct Modules;

    impl Program for Modules {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: registry::Genesis = abi::decode(params)?;
            for entry in genesis.programs {
                ctx.set(key(&entry.program), abi::encode(&entry));
            }
            Ok(())
        }

        fn execute(ctx: &mut Execute, _env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Change::Set(entry) => ctx.set(key(&entry.program), abi::encode(&entry)),
                Change::Remove(program) => ctx.delete(key(&program)),
            }
            Ok(())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let registry::Query::At(_) = abi::decode(request)?;
            let programs = ctx
                .scan(Scan::prefix(b"p/"))
                .into_iter()
                .map(|entry| abi::decode(&entry.value))
                .collect::<Result<Vec<registry::Entry>, Refusal>>()?;
            ctx.respond(abi::encode(&registry::Reply::Programs(programs)));
            Ok(())
        }
    }

    guest::program!(Modules);
}
