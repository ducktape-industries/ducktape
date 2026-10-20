use std::collections::BTreeMap;
use std::num::NonZeroU32;

use abi::{BlobId, ProgramId, Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};

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
pub const LINES: &[u8] = b"line/";
pub const ON: &[u8] = b"on/";

pub fn blob(id: &BlobId) -> Vec<u8> {
    abi::encode(id)
}

pub fn lines(holder: &str) -> Vec<u8> {
    [LINES, abi::encode(&holder).as_slice()].concat()
}

pub fn line(holder: &str, line: &[u8]) -> Vec<u8> {
    [lines(holder).as_slice(), line].concat()
}

pub fn on(id: &BlobId, holder: &str, line: &[u8]) -> Vec<u8> {
    [ON, abi::encode(&(id, holder, line)).as_slice()].concat()
}

pub fn on_lines(id: &BlobId, holder: &str) -> Vec<u8> {
    [ON, abi::encode(&(id, holder)).as_slice()].concat()
}

pub fn line_on(key: &[u8], prefix: &[u8]) -> Result<Vec<u8>> {
    let rest = key.get(prefix.len()..).unwrap_or_default();
    abi::decode(rest).map_err(corrupt)
}

pub fn is_index(key: &[u8]) -> bool {
    key.starts_with(LINES) || key.starts_with(ON)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Held {
    Alone,
    Lines { alone: bool, lines: NonZeroU32 },
}

impl Held {
    fn of(alone: bool, lines: u32) -> Option<Held> {
        match (alone, NonZeroU32::new(lines)) {
            (false, None) => None,
            (true, None) => Some(Held::Alone),
            (alone, Some(lines)) => Some(Held::Lines { alone, lines }),
        }
    }

    fn parts(self) -> (bool, u32) {
        match self {
            Held::Alone => (true, 0),
            Held::Lines { alone, lines } => (alone, lines.get()),
        }
    }
}

pub enum Change {
    Alone,
    List,
    Unlist,
    Release,
}

pub enum Roster {
    Unchanged,
    Written(Vec<u8>),
    Dropped,
}

type Holders = BTreeMap<ProgramId, Held>;

fn holders(value: &[u8]) -> Result<Option<Holders>> {
    let unknown = value.is_empty();
    if unknown {
        return Ok(None);
    }
    abi::decode(value).map(Some).map_err(corrupt)
}

pub fn changed(row: Option<&[u8]>, program: &str, change: Change) -> Result<Roster> {
    let mut held = match row {
        None => Holders::new(),
        Some(value) => match holders(value)? {
            Some(held) => held,
            None => return Ok(Roster::Unchanged),
        },
    };
    let before = held.get(program).copied();
    let (alone, lines) = before.map_or((false, 0), Held::parts);
    let (alone, lines) = match change {
        Change::Alone => (true, lines),
        Change::List => (alone, one_more(program, lines)?),
        Change::Unlist => (alone, lines.saturating_sub(1)),
        Change::Release => (false, 0),
    };
    let after = Held::of(alone, lines);
    if after == before {
        return Ok(Roster::Unchanged);
    }
    match after {
        Some(kept) => held.insert(program.to_owned(), kept),
        None => held.remove(program),
    };
    if held.is_empty() {
        return Ok(Roster::Dropped);
    }
    Ok(Roster::Written(abi::encode(&held)))
}

fn one_more(program: &str, lines: u32) -> Result<u32> {
    lines.checked_add(1).ok_or_else(|| {
        corrupt(Refusal::new(
            reason::CORRUPT,
            format!("{program} holds one blob on more lines than a u32 counts"),
        ))
    })
}

pub fn on_line(value: &[u8]) -> Result<BlobId> {
    abi::decode(value).map_err(corrupt)
}

pub fn latest(value: Option<&[u8]>) -> Result<Option<BlobId>> {
    value.map(on_line).transpose()
}
