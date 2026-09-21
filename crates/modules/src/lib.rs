pub mod identity;
pub mod module_registry;
pub mod valset;

use borsh::{BorshDeserialize, BorshSerialize};

pub type AccountNumber = u64;

pub const AUTHORITY: &str = "governance";

pub mod reason {
    pub use abi::reason::*;
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const CONFLICT: &str = "conflict";
}

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Page {
    pub after: Option<Vec<u8>>,
    pub limit: Option<u64>,
}

impl Page {
    pub fn all() -> Page {
        Page::default()
    }

    pub fn scan(&self, prefix: &[u8]) -> abi::Scan {
        let mut scan = abi::Scan::prefix(prefix);
        if let Some(after) = &self.after {
            scan = scan.after(after);
        }
        scan.limit = self.limit;
        scan
    }
}

#[cfg(target_arch = "wasm32")]
pub mod program {
    use abi::{Env, Origin, ProgramId, Refusal};

    use crate::reason;

    pub fn external(env: &Env) -> Result<Vec<u8>, Refusal> {
        match &env.origin {
            Origin::External(signer) => Ok(signer.clone()),
            other => Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("only a signed frame may do this, not {other:?}"),
            )),
        }
    }

    pub fn program(env: &Env) -> Result<ProgramId, Refusal> {
        match &env.origin {
            Origin::Program(program) => Ok(program.clone()),
            other => Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("only a program may do this, not {other:?}"),
            )),
        }
    }

    pub fn from(env: &Env, program: &str) -> Result<(), Refusal> {
        let sent_by_program = matches!(&env.origin, Origin::Program(sender) if sender == program);
        let sent_by_system = env.origin == Origin::System;
        let authorized = sent_by_program || sent_by_system;
        if !authorized {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("only {program} may do this, not {:?}", env.origin),
            ));
        }
        Ok(())
    }

    pub fn u64_key(prefix: &str, number: u64) -> Vec<u8> {
        let mut key = prefix.as_bytes().to_vec();
        key.extend_from_slice(&number.to_be_bytes());
        key
    }

    pub fn bytes_key(prefix: &str, bytes: &[u8]) -> Vec<u8> {
        let mut key = prefix.as_bytes().to_vec();
        key.extend_from_slice(bytes);
        key
    }

    pub fn not_found(what: impl Into<String>) -> Refusal {
        Refusal::new(reason::NOT_FOUND, what)
    }

    pub fn invalid(sentence: impl Into<String>) -> Refusal {
        Refusal::new(reason::INVALID_INPUT, sentence)
    }

    pub fn conflict(sentence: impl Into<String>) -> Refusal {
        Refusal::new(reason::CONFLICT, sentence)
    }

    pub fn unauthorized(sentence: impl Into<String>) -> Refusal {
        Refusal::new(reason::UNAUTHORIZED, sentence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_contracts_are_prefixes_of_the_program_contracts() {
        assert_eq!(
            abi::encode(&abi::valset::Query::Validators),
            abi::encode(&valset::Query::Validators)
        );
        assert_eq!(
            abi::encode(&abi::valset::Query::Members),
            abi::encode(&valset::Query::Members)
        );
        let member = abi::valset::Member {
            key: vec![1],
            address: "a".into(),
        };
        assert_eq!(
            abi::encode(&abi::valset::Reply::Validators(vec![vec![1]])),
            abi::encode(&valset::Reply::Validators(vec![vec![1]]))
        );
        assert_eq!(
            abi::encode(&abi::valset::Reply::Members(vec![member.clone()])),
            abi::encode(&valset::Reply::Members(vec![member]))
        );
        assert_eq!(
            abi::encode(&abi::module_registry::Query::At(9)),
            abi::encode(&module_registry::Query::At(9))
        );
        let entry = abi::module_registry::Entry {
            program: "p".into(),
            code: abi::BlobId::Sha256([1; 32]),
            params: vec![2],
        };
        assert_eq!(
            abi::encode(&abi::module_registry::Reply::Programs(vec![entry.clone()])),
            abi::encode(&module_registry::Reply::Programs(vec![entry]))
        );
    }

    #[test]
    fn a_page_scans_after_its_cursor() {
        let page = Page {
            after: Some(b"p/3".to_vec()),
            limit: Some(2),
        };
        let scan = page.scan(b"p/");
        assert!(!scan.admits(b"p/3"));
        assert!(scan.admits(b"p/4"));
        assert_eq!(scan.limit, Some(2));
        assert_eq!(Page::all().scan(b"p/"), abi::Scan::prefix(b"p/"));
    }
}
