use std::collections::BTreeMap;

use abi::{Env, ProgramId, Refusal, Scan};
use guest::Program;
use modules::capability::{
    Announcement, Claim, Op, Provider, Query, Reply, class_is_well_formed, tag_is_well_formed,
};
use modules::program::{bytes_key, conflict, invalid, unauthorized};
use modules::valset;

const NODE: &str = "n/";
const CLASS: &str = "c/";

struct Capability;

fn node_key(node: &[u8]) -> Vec<u8> {
    bytes_key(NODE, node)
}

fn class_key(class: &str) -> Vec<u8> {
    format!("{CLASS}{class}").into_bytes()
}

impl Program for Capability {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        match abi::decode(payload)? {
            Op::Announce(announcement) => announce(&env, announcement),
            Op::ClaimClass { class } => claim(&env, class),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Providers { tag, demands } => Reply::Providers(providers(&tag, &demands)?),
            Query::Node { node } => Reply::Node(guest::record(node_key(&node))?),
            Query::All { page } => Reply::All(
                guest::records::<Announcement>(page.scan(NODE.as_bytes()))?
                    .into_iter()
                    .map(|(key, announcement)| Provider {
                        node: key[NODE.len()..].to_vec(),
                        announcement,
                    })
                    .collect(),
            ),
            Query::Class { class } => Reply::Class(guest::record(class_key(&class))?),
            Query::Classes => Reply::Classes(
                guest::records::<ProgramId>(Scan::prefix(CLASS))?
                    .into_iter()
                    .map(|(key, program)| Claim {
                        class: String::from_utf8_lossy(&key[CLASS.len()..]).into_owned(),
                        program,
                    })
                    .collect(),
            ),
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn announce(env: &Env, announcement: Announcement) -> Result<(), Refusal> {
    modules::acl::admit(env)?;
    let node = modules::program::external(env)?;
    let member = valset::standing(&node)?.is_some();
    if !member {
        return Err(unauthorized("only a member announces what it can run"));
    }
    for tag in &announcement.tags {
        if !tag_is_well_formed(tag) {
            return Err(invalid(format!("{tag:?} is not a capability tag")));
        }
    }
    for (dimension, amount) in &announcement.resources {
        if !tag_is_well_formed(dimension) {
            return Err(invalid(format!(
                "{dimension:?} is not a resource dimension"
            )));
        }
        if *amount == 0 {
            return Err(invalid(format!("{dimension:?} names no capacity; omit it")));
        }
    }
    let withdraws = announcement.tags.is_empty();
    if withdraws {
        guest::delete(node_key(&node));
        return Ok(());
    }
    let mut announcement = announcement;
    announcement.tags.sort();
    announcement.tags.dedup();
    guest::put(node_key(&node), &announcement);
    Ok(())
}

fn claim(env: &Env, class: String) -> Result<(), Refusal> {
    let program = modules::program::program(env)?;
    if !class_is_well_formed(&class) {
        return Err(invalid(format!("{class:?} is not a capability class")));
    }
    match guest::record::<ProgramId>(class_key(&class))? {
        Some(owner) if owner == program => Ok(()),
        Some(owner) => Err(conflict(format!("{class} is claimed by {owner}"))),
        None => {
            guest::put(class_key(&class), &program);
            Ok(())
        }
    }
}

fn providers(tag: &str, demands: &BTreeMap<String, u64>) -> Result<Vec<Vec<u8>>, Refusal> {
    Ok(guest::records::<Announcement>(Scan::prefix(NODE))?
        .into_iter()
        .filter(|(_, announcement)| {
            let serves = announcement.tags.iter().any(|served| served == tag);
            serves && announcement.covers(demands)
        })
        .map(|(key, _)| key[NODE.len()..].to_vec())
        .collect())
}

guest::program!(Capability);
