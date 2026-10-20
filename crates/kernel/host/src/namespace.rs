use std::collections::BTreeMap;

use abi::{BlobId, ProgramId, Retention};

use crate::{Result, Submission, corrupt};

pub const BLOBS: &str = "$blobs";
pub const PROGRAMS: &str = "$programs";
pub const NETWORK: &str = "$network";
pub const RECENT: &str = "$recent";
pub const SPENT: &str = "$spent";
pub const PARKED: &str = "$parked";
pub const RESERVED: [&str; 6] = [BLOBS, PROGRAMS, NETWORK, RECENT, SPENT, PARKED];

pub const ID: &[u8] = b"id";
pub const LIMITS: &[u8] = b"limits";
pub const EPOCH_LENGTH: &[u8] = b"epoch_length";
pub const MEMBER_CAP: &[u8] = b"member_cap";
pub const ROLES: &[u8] = b"roles";
pub const NEXT_ITEM: &[u8] = b"next_item";

pub fn blob(id: &BlobId) -> Vec<u8> {
    abi::encode(id)
}

pub fn height(height: u64) -> Vec<u8> {
    height.to_be_bytes().to_vec()
}

pub fn spent(submission: &Submission) -> Vec<u8> {
    [height(submission.anchor.height), submission.id().to_vec()].concat()
}

pub fn epoch(number: u64) -> Vec<u8> {
    format!("epoch/{number}").into_bytes()
}

pub fn epoch_validators(number: u64) -> Vec<u8> {
    format!("validators/{number}").into_bytes()
}

pub enum Released {
    Kept,
    Held(Vec<u8>),
    Dropped,
}

fn holders(value: &[u8]) -> Result<Option<BTreeMap<ProgramId, Retention>>> {
    let unknown = value.is_empty();
    if unknown {
        return Ok(None);
    }
    abi::decode(value).map(Some).map_err(corrupt)
}

pub fn put_by(row: Option<&[u8]>, program: &str) -> Result<Option<Vec<u8>>> {
    let Some(value) = row else {
        let held = BTreeMap::from([(program.to_owned(), Retention::Referenced)]);
        return Ok(Some(abi::encode(&held)));
    };
    let Some(mut held) = holders(value)? else {
        return Ok(None);
    };
    let holds_already = held.contains_key(program);
    if holds_already {
        return Ok(None);
    }
    held.insert(program.to_owned(), Retention::Referenced);
    Ok(Some(abi::encode(&held)))
}

pub fn released_by(row: Option<&[u8]>, program: &str) -> Result<Released> {
    let Some(value) = row else {
        return Ok(Released::Kept);
    };
    let Some(mut held) = holders(value)? else {
        return Ok(Released::Kept);
    };
    let removed = held.remove(program).is_some();
    if !removed {
        return Ok(Released::Kept);
    }
    let none_left = held.is_empty();
    if none_left {
        return Ok(Released::Dropped);
    }
    Ok(Released::Held(abi::encode(&held)))
}
