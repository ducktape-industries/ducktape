//! Authenticated filesystem principals, independent of the consensus SDK.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::codec::{Reader, push_string};

/// Stable ownership and authorship. A program is an account, without a key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Actor {
    Account(u64),
    Key(Vec<u8>),
    Module(String),
    System,
}

impl Actor {
    pub(crate) fn encoded_len(&self) -> usize {
        match self {
            Self::Account(_) => 9,
            Self::Key(key) => 9 + key.len(),
            Self::Module(module) => 9 + module.len(),
            Self::System => 1,
        }
    }

    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Account(number) => {
                out.push(0);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Self::Key(key) => {
                out.push(1);
                out.extend_from_slice(&(key.len() as u64).to_le_bytes());
                out.extend_from_slice(key);
            }
            Self::Module(module) => {
                out.push(2);
                push_string(out, module);
            }
            Self::System => out.push(3),
        }
    }

    pub(crate) fn decode(reader: &mut Reader<'_>) -> Result<Self, String> {
        match reader.u8()? {
            0 => {
                let number = reader.u64()?;
                if number == 0 {
                    return Err("account zero is not an actor".into());
                }
                Ok(Self::Account(number))
            }
            1 => {
                let key = reader.bytes()?;
                if key.is_empty() {
                    return Err("actor key is empty".into());
                }
                Ok(Self::Key(key))
            }
            2 => {
                let module = reader.string()?;
                if module.is_empty() {
                    return Err("actor module is empty".into());
                }
                Ok(Self::Module(module))
            }
            3 => Ok(Self::System),
            _ => Err("invalid actor tag".into()),
        }
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Account(number) => write!(f, "acct:{number}"),
            Self::Key(key) => write!(f, "ext:{}", crate::to_hex(key)),
            Self::Module(module) => write!(f, "module:{module}"),
            Self::System => f.write_str("system"),
        }
    }
}

/// Evidence supplied by the authenticated adapter, never by a file message.
/// Account admission keeps a key's own earlier staging and pin shares under
/// the exact key; another key on that account draws on the account's shares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authority {
    External { key: Vec<u8>, account: Option<u64> },
    Program(u64),
    Module(String),
    System,
}

impl Authority {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let actor = self.actor();
        match actor {
            Actor::Account(0) => Err("account zero is not an actor".into()),
            Actor::Account(_) | Actor::System => Ok(()),
            Actor::Key(key) => {
                if key.is_empty() {
                    return Err("actor key is empty".into());
                }
                Ok(())
            }
            Actor::Module(module) => {
                if module.is_empty() {
                    return Err("actor module is empty".into());
                }
                Ok(())
            }
        }
    }

    pub fn actor(&self) -> Actor {
        match self {
            Self::External { key, account: None } => Actor::Key(key.clone()),
            Self::External {
                account: Some(number),
                ..
            }
            | Self::Program(number) => Actor::Account(*number),
            Self::Module(module) => Actor::Module(module.clone()),
            Self::System => Actor::System,
        }
    }

    pub fn controls(&self, owner: &Actor) -> bool {
        match (owner, self) {
            (Actor::Key(owner_key), Self::External { key, .. }) => owner_key == key,
            (Actor::Key(_), Self::Program(_) | Self::Module(_) | Self::System) => false,
            (Actor::Account(_) | Actor::Module(_) | Actor::System, _) => *owner == self.actor(),
        }
    }
}
