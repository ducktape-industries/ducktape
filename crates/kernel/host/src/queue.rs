use abi::{ItemRef, Message, Outcome, ProgramId, Scan};
use borsh::{BorshDeserialize, BorshSerialize};
use state::{Overlay, Storage, View};

use crate::{Error, Result, namespace};

const ITEMS: &[u8] = b"i/";
const NEXT: &[u8] = b"n";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Item {
    Message {
        source: ProgramId,
        message: Message,
    },
    Completion {
        item: ItemRef,
        by: ProgramId,
        outcome: Outcome,
    },
}

pub struct Queued {
    pub seq: u64,
    pub item: Item,
}

pub fn key(seq: u64) -> Vec<u8> {
    [ITEMS, seq.to_be_bytes().as_slice()].concat()
}

fn seq_of(key: &[u8]) -> Result<u64> {
    let digits = key
        .strip_prefix(ITEMS)
        .and_then(|digits| <[u8; 8]>::try_from(digits).ok())
        .ok_or_else(|| {
            Error::Corrupt(format!(
                "queue key {} is not a sequence number",
                abi::hex(key)
            ))
        })?;
    Ok(u64::from_be_bytes(digits))
}

pub fn pending(view: &View<'_>) -> Result<Vec<Queued>> {
    view.scan(namespace::QUEUE, &Scan::prefix(ITEMS))?
        .into_iter()
        .map(|entry| {
            Ok(Queued {
                seq: seq_of(&entry.key)?,
                item: abi::decode(&entry.value)
                    .map_err(|refusal| Error::Corrupt(refusal.sentence))?,
            })
        })
        .collect()
}

pub fn push(storage: &Storage, overlay: &mut Overlay, item: Item) -> Result<u64> {
    let seq = match View::new(storage, vec![&*overlay]).get(namespace::QUEUE, NEXT)? {
        Some(bytes) => abi::decode(&bytes).map_err(|refusal| Error::Corrupt(refusal.sentence))?,
        None => 0,
    };
    overlay.set(namespace::QUEUE, key(seq), abi::encode(&item));
    overlay.set(namespace::QUEUE, NEXT.to_vec(), abi::encode(&(seq + 1)));
    Ok(seq)
}

pub fn take(overlay: &mut Overlay, seq: u64) {
    overlay.delete(namespace::QUEUE, key(seq));
}
