use abi::{ProgramId, role::registry};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Change {
    Set(registry::Entry),
    Remove(ProgramId),
    Hook(ProgramId),
    Unhook(ProgramId),
}

pub fn key(program: &str) -> Vec<u8> {
    format!("p/{program}").into_bytes()
}

pub fn hook_key(program: &str) -> Vec<u8> {
    format!("h/{program}").into_bytes()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, Refusal, Scan, role::registry};
    use guest::{Execute, Program, Query, Reads};

    use crate::{Change, hook_key, key};

    struct Modules;

    impl Program for Modules {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: registry::Genesis = abi::decode(params)?;
            for entry in genesis.programs {
                ctx.set(key(&entry.program), abi::encode(&entry));
            }
            for hook in genesis.hooks {
                ctx.set(hook_key(&hook), Vec::new());
            }
            Ok(())
        }

        fn execute(ctx: &mut Execute, _env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Change::Set(entry) => ctx.set(key(&entry.program), abi::encode(&entry)),
                Change::Remove(program) => ctx.delete(key(&program)),
                Change::Hook(program) => ctx.set(hook_key(&program), Vec::new()),
                Change::Unhook(program) => ctx.delete(hook_key(&program)),
            }
            Ok(())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let reply = match abi::decode(request)? {
                registry::Query::At(_) => registry::Reply::Programs(
                    ctx.scan(Scan::prefix(b"p/"))
                        .into_iter()
                        .map(|entry| abi::decode(&entry.value))
                        .collect::<Result<Vec<registry::Entry>, Refusal>>()?,
                ),
                registry::Query::Hooks(_) => registry::Reply::Hooks(
                    ctx.scan(Scan::prefix(b"h/"))
                        .into_iter()
                        .map(|entry| String::from_utf8_lossy(&entry.key[2..]).into_owned())
                        .collect(),
                ),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    guest::program!(Modules);
}
