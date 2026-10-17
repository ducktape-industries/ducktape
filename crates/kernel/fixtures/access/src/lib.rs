//! An access program for the kernel's tests: the lists owners set, checked by the role's
//! rules against the owners and defaults each program answers for its own keys.
use abi::ProgramId;
use abi::role::access::{Need, Ownership, Party};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Op {
    Check {
        program: ProgramId,
        actor: Vec<Party>,
        needs: Vec<Need>,
    },
    SetList {
        program: ProgramId,
        key: String,
        list: Option<Ownership>,
    },
}

pub fn list_key(program: &str, key: &str) -> Vec<u8> {
    format!("{program}/{key}").into_bytes()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Action, Need, Ownership, Party, Resource, writers};
    use abi::{Env, Origin, Principal, Refusal, Scan, reason};
    use program::{Execute, Program, Query, Reads};

    use crate::{Op, list_key};

    struct Access;

    fn actor_of(env: &Env) -> Vec<Party> {
        let account = match env.sender {
            Some(Principal::Account(number)) => Some(Party::Account(number)),
            Some(Principal::System) | None => None,
        };
        let program = match &env.origin {
            Origin::Program(program) => Some(Party::Program(program.clone())),
            Origin::External(_) | Origin::System => None,
        };
        account.into_iter().chain(program).collect()
    }

    fn check(ctx: &mut Execute, program: &str, actor: &[Party], need: Need) -> Result<(), Refusal> {
        let listed = writers(
            &need.key,
            |at| ctx.record::<Ownership>(list_key(program, at)),
            |at| ctx.resource(program, at),
        )?;
        if !listed.allows(actor) {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("{actor:?} may not act on {program}'s {}", need.key),
            ));
        }
        match need.action {
            Action::Write => {}
            Action::Delete => {
                let key = list_key(program, &need.key);
                let mut under = key.clone();
                under.push(b'/');
                for entry in ctx.scan(Scan::prefix(&under)) {
                    ctx.delete(entry.key);
                }
                ctx.delete(key);
            }
        }
        Ok(())
    }

    fn set_list(
        ctx: &mut Execute,
        env: &Env,
        program: &str,
        key: &str,
        list: Option<Ownership>,
    ) -> Result<(), Refusal> {
        let Resource { owner, .. } = ctx.resource(program, key)?;
        let owns = owner.is_some_and(|owner| actor_of(env).contains(&owner));
        if !owns {
            return Err(Refusal::new(reason::UNAUTHORIZED, "only its owner"));
        }
        let at = list_key(program, key);
        match list {
            Some(list) => ctx.put(at, &list),
            None => ctx.delete(at),
        }
        Ok(())
    }

    impl Program for Access {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Op::Check {
                    program,
                    actor,
                    needs,
                } => {
                    if env.origin != Origin::System {
                        return Err(Refusal::new(reason::UNAUTHORIZED, "the kernel's"));
                    }
                    for need in needs {
                        check(ctx, &program, &actor, need)?;
                    }
                    Ok(())
                }
                Op::SetList { program, key, list } => set_list(ctx, env, &program, &key, list),
            }
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let (program, key): (String, String) = abi::decode(request)?;
            let list = ctx.record::<Ownership>(list_key(&program, &key))?;
            ctx.reply(&list);
            Ok(())
        }
    }

    program::program!(Access);
}
