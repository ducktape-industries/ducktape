use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccountNumber;

pub const PROGRAM: &str = "gateway";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Audience {
    Owner,
    Network,
    Accounts(Vec<AccountNumber>),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Target {
    Content { manifest: [u8; 32] },
    Loopback,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Policy {
    pub audience: Audience,
    pub methods: Vec<Method>,
    pub max_request_bytes: Option<u64>,
    pub max_response_bytes: Option<u64>,
    pub allow_authorization: bool,
    pub allow_upgrade: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Definition {
    pub publisher: Vec<u8>,
    pub target: Target,
    pub policy: Policy,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Route {
    pub account: AccountNumber,
    pub name: Option<String>,
    pub definition: Definition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum CredentialKind {
    Claude,
    Codex,
    AppleCodesign,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Credential {
    pub account: AccountNumber,
    pub name: String,
    pub kind: CredentialKind,
    pub publisher: Vec<u8>,
    pub seal_key: [u8; 32],
    pub grants: Vec<AccountNumber>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Handle {
    pub handle: String,
    pub account: AccountNumber,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    SetHandle {
        handle: Option<String>,
    },
    SetRoute {
        name: Option<String>,
        definition: Option<Definition>,
    },
    SetCredential {
        name: String,
        kind: CredentialKind,
        publisher: Vec<u8>,
        seal_key: [u8; 32],
    },
    RemoveCredential {
        name: String,
    },
    GrantCredential {
        name: String,
        to: AccountNumber,
    },
    RevokeCredential {
        name: String,
        from: AccountNumber,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Resolve {
        handle: String,
    },
    Handle {
        account: AccountNumber,
    },
    Handles,
    Route {
        account: AccountNumber,
        name: Option<String>,
    },
    Routes {
        account: AccountNumber,
    },
    Credential {
        account: AccountNumber,
        name: String,
    },
    Credentials {
        account: AccountNumber,
    },
    Granted {
        to: AccountNumber,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Resolved(Option<AccountNumber>),
    Handle(Option<String>),
    Handles(Vec<Handle>),
    Route(Option<Route>),
    Routes(Vec<Route>),
    Credential(Option<Credential>),
    Credentials(Vec<Credential>),
}

pub fn handle_is_well_formed(handle: &str) -> bool {
    let charset = handle
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let edges = !handle.starts_with('-') && !handle.ends_with('-');
    !handle.is_empty() && charset && edges
}

pub fn label_is_well_formed(label: &str) -> bool {
    handle_is_well_formed(label)
}
