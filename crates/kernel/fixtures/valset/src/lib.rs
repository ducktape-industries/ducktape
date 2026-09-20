pub const KEY: &[u8] = b"validators";

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
            let validators: Vec<Vec<u8>> = abi::decode(payload)?;
            guest::set(KEY, abi::encode(&validators));
            Ok(())
        }

        fn query(request: &[u8]) -> Result<Vec<u8>, Refusal> {
            let validators::Query::Validators = abi::decode(request)?;
            let validators: Vec<Vec<u8>> = match guest::get(KEY) {
                Some(bytes) => abi::decode(&bytes)?,
                None => Vec::new(),
            };
            Ok(abi::encode(&validators::Reply::Validators(validators)))
        }
    }

    guest::program!(Valset);
}
