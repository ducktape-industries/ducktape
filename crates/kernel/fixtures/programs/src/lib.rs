use abi::{ProgramId, role::programs};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Change {
    Set(programs::Entry),
    Remove(ProgramId),
    Hook(ProgramId),
    Unhook(ProgramId),
    Fill {
        role: String,
        program: Option<ProgramId>,
    },
    Rename {
        from: ProgramId,
        to: ProgramId,
    },
}

pub const ROLES: &[u8] = b"roles";

pub fn key(program: &str) -> Vec<u8> {
    format!("p/{program}").into_bytes()
}

pub fn hook_key(program: &str) -> Vec<u8> {
    format!("h/{program}").into_bytes()
}

pub fn rename_key(program: &str) -> Vec<u8> {
    format!("r/{program}").into_bytes()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::{Env, ProgramId, Refusal, Roles, Scan, reason, role::programs};
    use program::{Execute, Program, Query, Reads};

    use crate::{Change, ROLES, hook_key, key, rename_key};

    fn refill(
        ctx: &mut Execute,
        keep: impl Fn(&str, &ProgramId) -> bool,
        add: Option<(String, ProgramId)>,
    ) -> Result<(), Refusal> {
        let roles: Roles = ctx.record(ROLES)?.unwrap_or_default();
        let roles: Roles = roles
            .iter()
            .filter(|(role, program)| keep(role, program))
            .map(|(role, program)| (role.to_owned(), program.clone()))
            .chain(add)
            .collect();
        ctx.put(ROLES, &roles);
        Ok(())
    }

    struct Programs;

    impl Program for Programs {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: programs::Genesis = abi::decode(params)?;
            for entry in genesis.programs {
                ctx.set(key(&entry.program), abi::encode(&entry));
            }
            for hook in genesis.hooks {
                ctx.set(hook_key(&hook), Vec::new());
            }
            ctx.put(ROLES, &genesis.roles);
            Ok(())
        }

        fn execute(ctx: &mut Execute, _env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Change::Set(entry) => ctx.set(key(&entry.program), abi::encode(&entry)),
                Change::Remove(program) => {
                    ctx.delete(key(&program));
                    refill(ctx, |_, named| *named != program, None)?;
                }
                Change::Hook(program) => ctx.set(hook_key(&program), Vec::new()),
                Change::Unhook(program) => ctx.delete(hook_key(&program)),
                Change::Fill { role, program } => {
                    let row = program.map(|program| (role.clone(), program));
                    refill(ctx, |named, _| named != role, row)?;
                }
                Change::Rename { from, to } => {
                    let Some(bytes) = ctx.get(&key(&from)) else {
                        return Err(Refusal::new(reason::NOT_FOUND, from));
                    };
                    let mut entry: programs::Entry = abi::decode(&bytes)?;
                    entry.program = to.clone();
                    ctx.delete(key(&from));
                    ctx.set(key(&to), abi::encode(&entry));
                    ctx.set(rename_key(&from), to.into_bytes());
                }
            }
            Ok(())
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let reply = match abi::decode(request)? {
                programs::Query::At(_) => programs::Reply::Programs(
                    ctx.scan(Scan::prefix(b"p/"))
                        .into_iter()
                        .map(|entry| abi::decode(&entry.value))
                        .collect::<Result<Vec<programs::Entry>, Refusal>>()?,
                ),
                programs::Query::Roles => {
                    programs::Reply::Roles(ctx.record(ROLES)?.unwrap_or_default())
                }
                programs::Query::Hooks(_) => programs::Reply::Hooks(
                    ctx.scan(Scan::prefix(b"h/"))
                        .into_iter()
                        .map(|entry| String::from_utf8_lossy(&entry.key[2..]).into_owned())
                        .collect(),
                ),
                programs::Query::Renames(_) => programs::Reply::Renames(
                    ctx.scan(Scan::prefix(b"r/"))
                        .into_iter()
                        .map(|entry| {
                            let from = String::from_utf8_lossy(&entry.key[2..]).into_owned();
                            (from, String::from_utf8_lossy(&entry.value).into_owned())
                        })
                        .collect(),
                ),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    program::program!(Programs);
}
