//! The identity role as the kernel asks it, over a table of keys: founded
//! with the keys each account holds (`Vec<(key, account)>`), and an execute
//! adds one more holding.

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, Refusal, role::identity};
    use guest::{Execute, Program, Query, Reads};

    struct Identity;

    impl Program for Identity {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let held: Vec<(Vec<u8>, identity::AccountNumber)> = abi::decode(params)?;
            for (key, account) in held {
                ctx.set(key, abi::encode(&account));
            }
            Ok(())
        }

        fn execute(ctx: &mut Execute, _env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let (key, account): (Vec<u8>, identity::AccountNumber) = abi::decode(payload)?;
            ctx.set(key, abi::encode(&account));
            Ok(())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let identity::Query::Account(key) = abi::decode(request)?;
            let account = ctx.get(&key).map(|bytes| abi::decode(&bytes)).transpose()?;
            ctx.respond(abi::encode(&identity::Reply::Account(account)));
            Ok(())
        }
    }

    guest::program!(Identity);
}
