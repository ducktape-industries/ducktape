use borsh::{BorshDeserialize, BorshSerialize};

pub use abi::valset::{Genesis, Member, PROGRAM};

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Standing {
    Validator,
    Resident,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Membership {
    pub key: Vec<u8>,
    pub address: String,
    pub standing: Standing,
}

impl Membership {
    pub fn member(&self) -> Member {
        Member {
            key: self.key.clone(),
            address: self.address.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Set(Membership),
    Remove { key: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Validators,
    Members,
    Memberships,
    Membership { key: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Validators(Vec<Vec<u8>>),
    Members(Vec<Member>),
    Memberships(Vec<Membership>),
    Membership(Option<Membership>),
}

#[cfg(target_arch = "wasm32")]
pub fn standing(key: &[u8]) -> Result<Option<Standing>, abi::Refusal> {
    match guest::ask::<Query, Reply>(PROGRAM, &Query::Membership { key: key.to_vec() })? {
        Reply::Membership(membership) => Ok(membership.map(|membership| membership.standing)),
        other => Err(abi::Refusal::new(
            abi::reason::PROTOCOL,
            format!("valset answered Membership with {other:?}"),
        )),
    }
}
