pub const KEY: &[u8] = b"members";

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Refusal, valset};
    use guest::Program;

    use crate::KEY;

    struct Valset;

    impl Program for Valset {
        fn init(params: &[u8]) -> Result<(), Refusal> {
            let genesis: valset::Genesis = abi::decode(params)?;
            guest::set(KEY, abi::encode(&genesis.validators));
            Ok(())
        }

        fn execute(payload: &[u8]) -> Result<(), Refusal> {
            let members: Vec<valset::Member> = abi::decode(payload)?;
            guest::set(KEY, abi::encode(&members));
            Ok(())
        }

        fn query(request: &[u8]) -> Result<(), Refusal> {
            let members: Vec<valset::Member> = match guest::get(KEY) {
                Some(bytes) => abi::decode(&bytes)?,
                None => Vec::new(),
            };
            let reply = match abi::decode(request)? {
                valset::Query::Validators => valset::Reply::Validators(
                    members.into_iter().map(|member| member.key).collect(),
                ),
                valset::Query::Members => valset::Reply::Members(members),
            };
            guest::respond(abi::encode(&reply));
            Ok(())
        }
    }

    guest::program!(Valset);
}
