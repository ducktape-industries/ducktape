use abi::{ProgramId, roster};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
pub enum Change {
    Set(roster::Entry),
    Remove(ProgramId),
}

pub fn key(program: &str) -> Vec<u8> {
    format!("p/{program}").into_bytes()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Refusal, Scan, roster};
    use guest::Program;

    use crate::{Change, key};

    struct Modules;

    impl Program for Modules {
        fn init(params: &[u8]) -> Result<(), Refusal> {
            let genesis: roster::Genesis = abi::decode(params)?;
            for entry in genesis.programs {
                guest::set(key(&entry.program), abi::encode(&entry));
            }
            Ok(())
        }

        fn execute(payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Change::Set(entry) => guest::set(key(&entry.program), abi::encode(&entry)),
                Change::Remove(program) => guest::delete(key(&program)),
            }
            Ok(())
        }

        fn query(request: &[u8]) -> Result<Vec<u8>, Refusal> {
            let roster::Query::At(_) = abi::decode(request)?;
            let programs = guest::scan(Scan::prefix(b"p/"))
                .into_iter()
                .map(|entry| abi::decode(&entry.value))
                .collect::<Result<Vec<roster::Entry>, Refusal>>()?;
            Ok(abi::encode(&roster::Reply::Programs(programs)))
        }
    }

    guest::program!(Modules);
}
