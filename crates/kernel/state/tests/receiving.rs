//! Bulk state reception resumes a cut, rejects altered roots and fills the adopted store.
use std::{collections::BTreeMap, num::NonZeroU64};

use abi::{Entry, Root};
use commonware_cryptography::sha256::Digest;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use commonware_storage::qmdb::any::unordered::Update;
use state::{Commitment, Error, Op, Storage, Store, commitment_name, digest};

fn writes(height: u64) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
    (0..32u64)
        .map(|key| {
            let name = format!("key/{key}").into_bytes();
            let value =
                (!((height + key).is_multiple_of(7))).then(|| height.to_le_bytes().to_vec());
            (name, value)
        })
        .collect()
}

#[test]
fn a_cut_resumes_at_the_next_operation_and_adopts_the_held_root() {
    deterministic::Runner::default().start(|context| async move {
        let mut source = Commitment::open(context.child("source"), "source", 0)
            .await
            .unwrap();
        for height in 0..128 {
            source.apply(height, &writes(height)).await.unwrap();
        }
        let serving = source.serving();
        let target = serving.target.clone().unwrap();
        assert!(*target.range.start() > 0);
        let mut receiving = Commitment::receive(
            context.child("first"),
            &commitment_name("joined", "p"),
            target.clone(),
        )
        .await
        .unwrap();
        assert_eq!(receiving.size(), *target.range.start());
        let first = serving
            .ops(receiving.size(), NonZeroU64::new(3).unwrap())
            .await
            .unwrap();
        receiving = receiving
            .append(&first)
            .await
            .unwrap()
            .sync()
            .await
            .unwrap();
        let next = receiving.size();
        assert_eq!(next, *target.range.start() + first.len() as u64);
        drop(receiving);
        let mut receiving = Commitment::receive(
            context.child("resumed"),
            &commitment_name("joined", "p"),
            target.clone(),
        )
        .await
        .unwrap();
        assert_eq!(receiving.size(), next);
        for height in 128..136 {
            source.apply(height, &writes(height)).await.unwrap();
        }
        while receiving.size() < *target.range.end() {
            let ops = serving
                .ops(receiving.size(), NonZeroU64::new(17).unwrap())
                .await
                .unwrap();
            assert!(!ops.is_empty());
            receiving = receiving.append(&ops).await.unwrap();
        }
        assert!(
            serving
                .ops(*target.range.end(), NonZeroU64::MIN)
                .await
                .unwrap()
                .is_empty()
        );
        let commitment = receiving
            .finish(serving.pinned().await.unwrap())
            .await
            .unwrap();
        assert_eq!(commitment.root().unwrap(), Root(target.root.0));
        assert_eq!(commitment.height().await.unwrap(), Some(127));
        let dir = tempfile::tempdir().unwrap();
        let store = Store::adopt(
            context.child("adopt"),
            "joined",
            Storage::open(dir.path()).unwrap(),
            127,
            BTreeMap::from([("p".to_owned(), commitment)]),
        )
        .await
        .unwrap();
        for (key, value) in writes(127) {
            assert!(store.storage().get("p", &key).unwrap() == value);
        }
        assert_eq!(store.root("p").unwrap(), Some(Root(target.root.0)));
    });
}

#[test]
fn an_altered_operation_is_refused_instead_of_becoming_an_adoptable_commitment() {
    deterministic::Runner::default().start(|context| async move {
        let mut source = Commitment::open(context.child("source"), "source", 0)
            .await
            .unwrap();
        source.apply(0, &writes(0)).await.unwrap();
        let serving = source.serving();
        let target = serving.target.clone().unwrap();
        let mut ops = serving
            .ops(*target.range.start(), NonZeroU64::MAX)
            .await
            .unwrap();
        let op = ops
            .iter_mut()
            .find(|op| matches!(op, Op::Update(_)))
            .unwrap();
        let key = b"tampered";
        *op = Op::Update(Update(
            digest(key),
            abi::encode(&Entry {
                key: key.to_vec(),
                value: b"wrong".to_vec(),
            }),
        ));
        let receiving = Commitment::receive(context.child("receiving"), "receiving", target)
            .await
            .unwrap();
        let result = receiving
            .append(&ops)
            .await
            .unwrap()
            .finish(serving.pinned().await.unwrap())
            .await;
        assert!(matches!(result, Err(Error::RootMismatch { .. })));
    });
}

#[test]
fn a_dropped_tail_is_refused_before_building_the_database() {
    deterministic::Runner::default().start(|context| async move {
        let mut source = Commitment::open(context.child("source"), "source", 0)
            .await
            .unwrap();
        source.apply(0, &writes(0)).await.unwrap();
        let serving = source.serving();
        let target = serving.target.clone().unwrap();
        let mut ops = serving
            .ops(*target.range.start(), NonZeroU64::MAX)
            .await
            .unwrap();
        ops.pop().unwrap();
        let receiving = Commitment::receive(context.child("receiving"), "receiving", target)
            .await
            .unwrap();
        let result = receiving
            .append(&ops)
            .await
            .unwrap()
            .finish(serving.pinned().await.unwrap())
            .await;
        assert!(matches!(result, Err(Error::IncompleteSync { .. })));
    });
}

#[test]
fn an_untrusted_target_root_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let mut source = Commitment::open(context.child("source"), "source", 0)
            .await
            .unwrap();
        source.apply(0, &writes(0)).await.unwrap();
        let serving = source.serving();
        let mut target = serving.target.clone().unwrap();
        target.root = Digest([0; 32]);
        let ops = serving
            .ops(*target.range.start(), NonZeroU64::MAX)
            .await
            .unwrap();
        let receiving = Commitment::receive(context.child("receiving"), "receiving", target)
            .await
            .unwrap();
        let result = receiving
            .append(&ops)
            .await
            .unwrap()
            .finish(serving.pinned().await.unwrap())
            .await;
        assert!(matches!(result, Err(Error::RootMismatch { .. })));
    });
}
