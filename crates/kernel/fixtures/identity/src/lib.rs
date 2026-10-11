//! The identity role as the kernel asks it, over a table of keys: founded
//! with the keys each account holds (`Vec<(key, account)>`), and a signed
//! execute adds one more holding. The system's `RegisterModule` gives a
//! program the next account from [`MODULES_FROM`], refused while the key
//! `#closed/<module>` holds a nonzero number. It keeps no profiles.

/// The first account number a program is given.
pub const MODULES_FROM: u64 = 1000;

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, Origin, Refusal, reason, role::identity};
    use guest::{Execute, Program, Query, Reads};

    const NEXT: &[u8] = b"#next";

    fn module_key(module: &str) -> Vec<u8> {
        [b"#module/", module.as_bytes()].concat()
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
                let identity::Op::RegisterModule { module } = abi::decode(payload)?;
                let closed: Option<u64> = ctx.record([b"#closed/", module.as_bytes()].concat())?;
                if closed.is_some_and(|number| number != 0) {
                    return Err(Refusal::new("closed", module));
                }
                let key = module_key(&module);
                if ctx.get(&key).is_none() {
                    let number: u64 = ctx.record(NEXT)?.unwrap_or(super::MODULES_FROM);
                    ctx.put(NEXT, &(number + 1));
                    ctx.put(key, &number);
                }
                return Ok(());
            }
            let (key, account): (Vec<u8>, identity::AccountNumber) = abi::decode(payload)?;
            ctx.set(key, abi::encode(&account));
            Ok(())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let key = match abi::decode(request)? {
                identity::Query::Account(key) => key,
                identity::Query::OfModule(module) => module_key(&module),
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

    guest::program!(Identity);
}
