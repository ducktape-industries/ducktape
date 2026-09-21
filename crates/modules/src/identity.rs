use abi::{BlobId, ProgramId, Scheme};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::{AccountNumber, Page};

pub const PROGRAM: &str = "identity";
pub const CONSENT_NAMESPACE: &[u8] = b"ducktape:identity:consent";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Key {
    pub scheme: Scheme,
    pub key: Vec<u8>,
    pub label: Option<String>,
    pub added_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Standing {
    Active,
    Suspended,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Control {
    Keys(Vec<Key>),
    Program {
        executor: ProgramId,
        controller: AccountNumber,
        standing: Standing,
    },
    Revoked {
        controller: AccountNumber,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Account {
    pub number: AccountNumber,
    pub name: String,
    pub control: Control,
    pub avatar: Option<BlobId>,
    pub bio: Option<String>,
    pub updated_at: u64,
}

impl Account {
    pub fn keys(&self) -> &[Key] {
        match &self.control {
            Control::Keys(keys) => keys,
            Control::Program { .. } | Control::Revoked { .. } => &[],
        }
    }

    pub fn holds(&self, key: &[u8]) -> bool {
        self.keys().iter().any(|held| held.key == key)
    }

    pub fn live(&self) -> bool {
        match &self.control {
            Control::Keys(_) => true,
            Control::Program { standing, .. } => *standing == Standing::Active,
            Control::Revoked { .. } => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Consent {
    pub key: Vec<u8>,
    pub account: AccountNumber,
    pub expires_at: u64,
    pub proof: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Admission {
    pub network: Vec<u8>,
    pub scheme: Scheme,
    pub key: Vec<u8>,
    pub generation: u64,
    pub account: AccountNumber,
    pub expires_at: u64,
}

impl Admission {
    pub fn preimage(&self) -> Vec<u8> {
        abi::encode(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Create {
        name: String,
        scheme: Scheme,
    },
    AddKey {
        scheme: Scheme,
        label: Option<String>,
        consent: Consent,
    },
    RemoveKey {
        key: Vec<u8>,
    },
    SetName {
        account: AccountNumber,
        name: String,
    },
    SetProfile {
        account: AccountNumber,
        avatar: Option<BlobId>,
        bio: Option<String>,
    },
    CreateProgram {
        name: String,
        controller: AccountNumber,
    },
    SetStanding {
        account: AccountNumber,
        standing: Standing,
    },
    TransferControl {
        account: AccountNumber,
        to: AccountNumber,
    },
    Revoke {
        account: AccountNumber,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reference {
    Account(AccountNumber),
    Key(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Get { number: AccountNumber },
    OfKey { key: Vec<u8> },
    Generation { key: Vec<u8> },
    Resolve { references: Vec<Reference> },
    List { page: Page },
    Controlled { by: AccountNumber, page: Page },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Account(Option<Account>),
    Number(Option<AccountNumber>),
    Generation(u64),
    Resolved(Vec<Option<AccountNumber>>),
    Accounts(Vec<Account>),
}

pub fn principal(number: AccountNumber) -> Vec<u8> {
    number.to_le_bytes().to_vec()
}

pub fn account_of_principal(bytes: &[u8]) -> Option<AccountNumber> {
    <[u8; 8]>::try_from(bytes).ok().map(u64::from_le_bytes)
}

#[cfg(target_arch = "wasm32")]
pub fn account_of(key: &[u8]) -> Result<Option<AccountNumber>, abi::Refusal> {
    match guest::ask::<Query, Reply>(PROGRAM, &Query::OfKey { key: key.to_vec() })? {
        Reply::Number(number) => Ok(number),
        other => Err(abi::Refusal::new(
            abi::reason::PROTOCOL,
            format!("identity answered OfKey with {other:?}"),
        )),
    }
}

#[cfg(target_arch = "wasm32")]
pub fn account(number: AccountNumber) -> Result<Option<Account>, abi::Refusal> {
    match guest::ask::<Query, Reply>(PROGRAM, &Query::Get { number })? {
        Reply::Account(account) => Ok(account),
        other => Err(abi::Refusal::new(
            abi::reason::PROTOCOL,
            format!("identity answered Get with {other:?}"),
        )),
    }
}
