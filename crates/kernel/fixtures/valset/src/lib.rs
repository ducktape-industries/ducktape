pub const VALIDATORS: &[u8] = b"validators";
pub const RESIDENTS: &[u8] = b"residents";

/// A validators program: `execute` takes `(validators, residents)`, both
/// `Vec<Member>`. `Validators` answers the validators' keys, `Members` the
/// validators then the residents.
#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Env, Refusal, role::validators};
    use guest::{Execute, Program, Query, Reads};

    use crate::{RESIDENTS, VALIDATORS};

    struct Valset;

    fn read(ctx: &Query, key: &[u8]) -> Result<Vec<validators::Member>, Refusal> {
        match ctx.get(key) {
            Some(bytes) => abi::decode(&bytes),
            None => Ok(Vec::new()),
        }
    }

    impl Program for Valset {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: validators::Genesis = abi::decode(params)?;
            ctx.set(VALIDATORS, abi::encode(&genesis.validators));
            Ok(())
        }

        fn execute(ctx: &mut Execute, _env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let (validators, residents): (Vec<validators::Member>, Vec<validators::Member>) =
                abi::decode(payload)?;
            ctx.set(VALIDATORS, abi::encode(&validators));
            ctx.set(RESIDENTS, abi::encode(&residents));
            Ok(())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let validators = read(ctx, VALIDATORS)?;
            let reply = match abi::decode(request)? {
                validators::Query::Validators => validators::Reply::Validators(
                    validators.into_iter().map(|member| member.key).collect(),
                ),
                validators::Query::Members => {
                    let mut members = validators;
                    members.extend(read(ctx, RESIDENTS)?);
                    validators::Reply::Members(members)
                }
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    guest::program!(Valset);
}
