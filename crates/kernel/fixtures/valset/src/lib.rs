pub const KEY: &[u8] = b"members";

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Refusal, validators};
    use guest::Program;

    use crate::KEY;

    struct Valset;

    impl Program for Valset {
        fn init(params: &[u8]) -> Result<(), Refusal> {
            let genesis: validators::Genesis = abi::decode(params)?;
            guest::set(KEY, abi::encode(&genesis.validators));
            Ok(())
        }

        fn execute(payload: &[u8]) -> Result<(), Refusal> {
            let members: Vec<validators::Member> = abi::decode(payload)?;
            guest::set(KEY, abi::encode(&members));
            Ok(())
        }

        fn query(request: &[u8]) -> Result<Vec<u8>, Refusal> {
            let members: Vec<validators::Member> = match guest::get(KEY) {
                Some(bytes) => abi::decode(&bytes)?,
                None => Vec::new(),
            };
            let reply = match abi::decode(request)? {
                validators::Query::Validators => validators::Reply::Validators(
                    members.into_iter().map(|member| member.key).collect(),
                ),
                validators::Query::Members => validators::Reply::Members(members),
            };
            Ok(abi::encode(&reply))
        }
    }

    guest::program!(Valset);
}
