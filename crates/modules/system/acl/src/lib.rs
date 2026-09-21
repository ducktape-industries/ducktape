use abi::{Refusal, Scan};
use guest::Program;
use wire::acl::{ANY, Op, Policy, Query, Reply, Standing};
use wire::{governance, identity, valset};

const POLICY: &str = "policy/";

struct Acl;

fn key(target: &str) -> Vec<u8> {
    format!("{POLICY}{target}").into_bytes()
}

impl Program for Acl {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        wire::program::from(&env, governance::PROGRAM)?;
        match abi::decode(payload)? {
            Op::SetPolicy {
                target,
                standing: Some(standing),
            } => guest::put(key(&target), &standing),
            Op::SetPolicy {
                target,
                standing: None,
            } => guest::delete(key(&target)),
        }
        Ok(())
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Policies => Reply::Policies(policies()?),
            Query::Required { target } => Reply::Required(required(&target)?),
            Query::Admits { target, signer } => Reply::Admits(admits(&target, &signer)?),
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn policies() -> Result<Vec<Policy>, Refusal> {
    Ok(guest::records::<Standing>(Scan::prefix(POLICY))?
        .into_iter()
        .map(|(key, standing)| Policy {
            target: String::from_utf8_lossy(&key[POLICY.len()..]).into_owned(),
            standing,
        })
        .collect())
}

fn required(target: &str) -> Result<Option<Standing>, Refusal> {
    match guest::record(key(target))? {
        Some(exact) => Ok(Some(exact)),
        None => guest::record(key(ANY)),
    }
}

fn admits(target: &str, signer: &[u8]) -> Result<bool, Refusal> {
    let Some(required) = required(target)? else {
        return Ok(true);
    };
    let admitted = match required {
        Standing::Open => true,
        Standing::Validator => valset::standing(signer)? == Some(valset::Standing::Validator),
        Standing::Node => valset::standing(signer)?.is_some(),
        Standing::User => identity::account_of(signer)?.is_some(),
    };
    Ok(admitted)
}

guest::program!(Acl);
