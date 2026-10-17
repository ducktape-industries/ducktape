// The blobs program's contract: its ops, queries and replies, the bundle it records, and the
// keys it records them under. The blobs program, the host's Held check and the node's watcher read it.

use abi::BlobId;
use abi::role::identity::AccountNumber;
use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};

use crate::{Collision, id_of, parse};

/// The halo lane a node gathers and serves blobs on, as the halo role's table names it.
pub const LANE: &str = "ducktape/blobs";
pub const BUNDLES: &[u8] = b"b/";
pub const INDEX: &[u8] = b"i/";
pub const NEXT: &[u8] = b"next";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub struct Entry {
    pub id: BlobId,
    pub len: u64,
    pub kind: String,
}

impl Entry {
    pub fn of(framed: &[u8]) -> Option<Entry> {
        let (header, _) = parse(framed)?;
        Some(Entry {
            id: id_of(abi::HashKind::Sha256, framed).ok()?,
            len: framed.len() as u64,
            kind: header.kind,
        })
    }

    pub fn names(&self, framed: &[u8]) -> Result<bool, Collision> {
        let Some((header, _)) = parse(framed) else {
            return Ok(false);
        };
        let sized = framed.len() as u64 == self.len;
        let kinded = header.kind == self.kind;
        let hashed = id_of(self.id.kind(), framed)? == self.id;
        Ok(sized && kinded && hashed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum Op {
    Put { blobs: Vec<Entry> },
    Receipt { bundle: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum Query {
    Held { blob: BlobId },
    Bundle { n: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum Custody {
    Receipted(Vec<Vec<u8>>),
    Held { since: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub struct Bundle {
    pub putter: AccountNumber,
    pub height: u64,
    pub blobs: Vec<Entry>,
    pub custody: Custody,
}

impl Bundle {
    pub fn held(&self) -> bool {
        matches!(self.custody, Custody::Held { .. })
    }

    pub fn held_since(&self) -> Option<u64> {
        match self.custody {
            Custody::Held { since } => Some(since),
            Custody::Receipted(_) => None,
        }
    }

    pub fn receipted_by(&self, key: &[u8]) -> bool {
        match &self.custody {
            Custody::Receipted(keys) => keys.iter().any(|held| held == key),
            Custody::Held { .. } => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub struct Holding {
    pub bundle: u64,
    pub len: u64,
    pub kind: String,
    pub since: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum Reply {
    Held(Option<Holding>),
    Bundle(Option<Bundle>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    pub bundle: u64,
    pub refusal: abi::Refusal,
}

pub fn held_since(stored: Option<&[u8]>) -> Option<u64> {
    let bundle: Bundle = abi::decode(stored?).ok()?;
    bundle.held_since()
}

pub fn recorded(entries: &[abi::Entry]) -> Result<Vec<(u64, Bundle)>, Unreadable> {
    let mut bundles = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(n) = bundle_of(&entry.key) else {
            continue;
        };
        let bundle =
            abi::decode(&entry.value).map_err(|refusal| Unreadable { bundle: n, refusal })?;
        bundles.push((n, bundle));
    }
    Ok(bundles)
}

pub fn bundle_key(n: u64) -> Vec<u8> {
    [BUNDLES, &n.to_be_bytes()].concat()
}

pub fn bundle_of(key: &[u8]) -> Option<u64> {
    let number = key.strip_prefix(BUNDLES)?;
    Some(u64::from_be_bytes(number.try_into().ok()?))
}

pub fn index_prefix(id: &BlobId) -> Vec<u8> {
    [INDEX, &abi::encode(id)].concat()
}

pub fn index_key(id: &BlobId, n: u64) -> Vec<u8> {
    [index_prefix(id), n.to_be_bytes().to_vec()].concat()
}

pub fn indexed_bundle(key: &[u8], id: &BlobId) -> Option<u64> {
    let number = key.strip_prefix(index_prefix(id).as_slice())?;
    Some(u64::from_be_bytes(number.try_into().ok()?))
}

pub fn quorum(validators: usize) -> usize {
    validators - validators.saturating_sub(1) / 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;

    #[test]
    fn an_entry_names_only_the_bytes_it_was_made_of() {
        let framed = frame("pack", b"PACK").unwrap();
        let entry = Entry::of(&framed).unwrap();
        assert_eq!(entry.kind, "pack");
        assert_eq!(entry.len, framed.len() as u64);
        assert_eq!(entry.names(&framed), Ok(true));
        let other = frame("blob", b"PACK").unwrap();
        assert_eq!(entry.names(&other), Ok(false));
        let mislabelled = Entry {
            kind: "blob".into(),
            ..entry.clone()
        };
        assert_eq!(mislabelled.names(&framed), Ok(false));
        assert!(Entry::of(b"not framed").is_none());
    }

    #[test]
    fn a_bundle_key_reads_back_its_number_and_an_index_key_its_bundle() {
        assert_eq!(bundle_of(&bundle_key(7)), Some(7));
        assert_eq!(bundle_of(b"b/short"), None);
        assert_eq!(bundle_of(NEXT), None);
        let id = BlobId::Sha256([3; 32]);
        let other = BlobId::Sha256([4; 32]);
        assert_eq!(indexed_bundle(&index_key(&id, 9), &id), Some(9));
        assert_eq!(indexed_bundle(&index_key(&other, 9), &id), None);
    }

    fn bundle(custody: Custody) -> Bundle {
        Bundle {
            putter: 1,
            height: 4,
            blobs: Vec::new(),
            custody,
        }
    }

    #[test]
    fn a_stored_bundle_is_held_since_its_height_only_once_held() {
        let held = abi::encode(&bundle(Custody::Held { since: 11 }));
        let receipted = abi::encode(&bundle(Custody::Receipted(vec![vec![1]])));
        assert_eq!(held_since(Some(&held)), Some(11));
        assert_eq!(held_since(Some(&receipted)), None);
        assert_eq!(held_since(None), None);
        assert_eq!(held_since(Some(b"not a bundle")), None);
    }

    #[test]
    fn recorded_reads_every_bundle_and_names_the_one_that_does_not_decode() {
        let held = bundle(Custody::Held { since: 11 });
        let receipted = bundle(Custody::Receipted(Vec::new()));
        let entry = |key: Vec<u8>, value: Vec<u8>| abi::Entry { key, value };
        let entries = [
            entry(bundle_key(2), abi::encode(&held)),
            entry(bundle_key(5), abi::encode(&receipted)),
            entry(NEXT.to_vec(), abi::encode(&6u64)),
        ];
        assert_eq!(
            recorded(&entries),
            Ok(vec![(2, held.clone()), (5, receipted)])
        );
        let broken = [
            entry(bundle_key(2), abi::encode(&held)),
            entry(bundle_key(3), b"not a bundle".to_vec()),
        ];
        assert_eq!(
            recorded(&broken).map_err(|unreadable| unreadable.bundle),
            Err(3)
        );
    }

    #[test]
    fn a_quorum_is_all_but_the_faults_a_set_tolerates() {
        assert_eq!(quorum(1), 1);
        assert_eq!(quorum(3), 3);
        assert_eq!(quorum(4), 3);
        assert_eq!(quorum(7), 5);
        assert_eq!(quorum(0), 0);
    }
}
