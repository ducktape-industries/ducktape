#[cfg(target_arch = "wasm32")]
mod program {
    use abi::valset::Member;
    use abi::{Env, Origin, Refusal, admission, reason, valset};
    use guest::{Execute, Program, Query};

    struct Admission;

    impl Program for Admission {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let Origin::External(key) = &env.origin else {
                return Err(Refusal::new(
                    reason::UNAUTHORIZED,
                    format!("only a signed frame may enroll, not {:?}", env.origin),
                ));
            };
            let admission::Op::Enroll { address } = abi::decode(payload)?;
            let member = Member {
                key: key.clone(),
                address,
            };
            ctx.emit(valset::PROGRAM, abi::encode(&member));
            Ok(())
        }

        fn query(_ctx: &mut Query, _env: &Env, _request: &[u8]) -> Result<(), Refusal> {
            Err(Refusal::new(
                reason::UNSUPPORTED,
                "admission answers no query",
            ))
        }
    }

    guest::program!(Admission);
}
