pub const KEY: &[u8] = b"members";

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, Refusal, role::validators};
    use guest::{Execute, Program, Query, Reads};

    use crate::KEY;

    struct Valset;

    impl Program for Valset {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: validators::Genesis = abi::decode(params)?;
            ctx.set(KEY, abi::encode(&genesis.validators));
            Ok(())
        }

        fn execute(ctx: &mut Execute, _env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let members: Vec<validators::Member> = abi::decode(payload)?;
            ctx.set(KEY, abi::encode(&members));
            Ok(())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let members: Vec<validators::Member> = match ctx.get(KEY) {
                Some(bytes) => abi::decode(&bytes)?,
                None => Vec::new(),
            };
            let reply = match abi::decode(request)? {
                validators::Query::Validators => validators::Reply::Validators(
                    members.into_iter().map(|member| member.key).collect(),
                ),
                validators::Query::Members => validators::Reply::Members(members),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    guest::program!(Valset);
}
