//! The identity role as the kernel asks it, over a table of keys: founded
//! with the keys each account holds (`Vec<(key, account)>`), and a signed
//! execute adds one more holding. The system's `RegisterProgram` gives a
//! program the next account from [`PROGRAMS_FROM`], refused while the key
//! `#closed/<program>` holds a nonzero number. A program runs the account the key
//! `#runs/<program>` holds. It keeps no profiles.

/// The first account number a program is given.
pub const PROGRAMS_FROM: u64 = 1000;

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::{Env, Origin, Refusal, reason, role::identity};
    use program::{Execute, Program, Query, Reads};

    const NEXT: &[u8] = b"#next";

    fn program_key(program: &str) -> Vec<u8> {
        [b"#program/", program.as_bytes()].concat()
    }

    struct Identity;

    impl Program for Identity {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let held: Vec<(Vec<u8>, identity::AccountNumber)> = abi::decode(params)?;
            for (key, account) in held {
                ctx.set(key, abi::encode(&account));
            }
            Ok(())
        }

        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            if env.origin == Origin::System {
                let identity::Op::RegisterProgram { program } = abi::decode(payload)?;
                let closed: Option<u64> = ctx.record([b"#closed/", program.as_bytes()].concat())?;
                if closed.is_some_and(|number| number != 0) {
                    return Err(Refusal::new("closed", program));
                }
                let key = program_key(&program);
                if ctx.get(&key).is_none() {
                    let number: u64 = ctx.record(NEXT)?.unwrap_or(super::PROGRAMS_FROM);
                    ctx.put(NEXT, &(number + 1));
                    ctx.put(key, &number);
                }
                return Ok(());
            }
            let (key, account): (Vec<u8>, identity::AccountNumber) = abi::decode(payload)?;
            ctx.set(key, abi::encode(&account));
            if account == identity::AccountNumber::MAX {
                return Err(Refusal::new(
                    reason::INVALID_INPUT,
                    "no account is numbered so",
                ));
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
            let key = match abi::decode(request)? {
                identity::Query::Account(key) => key,
                identity::Query::OfProgram(program) => program_key(&program),
                identity::Query::Runs { program, account } => {
                    let runs: Option<u64> = ctx.record([b"#runs/", program.as_bytes()].concat())?;
                    if runs != Some(account) {
                        return Err(Refusal::new(reason::UNAUTHORIZED, program));
                    }
                    ctx.respond(abi::encode(&identity::Reply::Account(Some(account))));
                    return Ok(());
                }
                identity::Query::Profile(_) | identity::Query::Profiles { .. } => {
                    return Err(Refusal::new(
                        reason::UNSUPPORTED,
                        "this identity names no one",
                    ));
                }
            };
            let account = ctx.get(&key).map(|bytes| abi::decode(&bytes)).transpose()?;
            ctx.respond(abi::encode(&identity::Reply::Account(account)));
            Ok(())
        }
    }

    program::program!(Identity);
}
