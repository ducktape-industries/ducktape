use borsh::{BorshDeserialize, BorshSerialize};

pub const PROGRAM: &str = "acl";
pub const ANY: &str = "*";

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Standing {
    Validator,
    Node,
    User,
    Open,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Policy {
    pub target: String,
    pub standing: Standing,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    SetPolicy {
        target: String,
        standing: Option<Standing>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Policies,
    Required { target: String },
    Admits { target: String, signer: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Policies(Vec<Policy>),
    Required(Option<Standing>),
    Admits(bool),
}

#[cfg(target_arch = "wasm32")]
pub fn admit(env: &abi::Env) -> Result<(), abi::Refusal> {
    use abi::{Origin, Refusal};

    use crate::reason;

    let Origin::External(signer) = &env.origin else {
        return Ok(());
    };
    let asked = guest::ask::<Query, Reply>(
        PROGRAM,
        &Query::Admits {
            target: env.me.clone(),
            signer: signer.clone(),
        },
    );
    let admitted = match asked {
        Ok(Reply::Admits(admitted)) => admitted,
        Ok(other) => {
            return Err(Refusal::new(
                reason::PROTOCOL,
                format!("acl answered Admits with {other:?}"),
            ));
        }
        Err(refusal) if no_acl_program(&refusal) => true,
        Err(refusal) => return Err(refusal),
    };
    if !admitted {
        return Err(Refusal::new(
            reason::UNAUTHORIZED,
            format!("{} does not admit this signer", env.me),
        ));
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn no_acl_program(refusal: &abi::Refusal) -> bool {
    refusal.reason == crate::reason::UNKNOWN_PROGRAM && refusal.sentence == PROGRAM
}
