//! A historical op proof for one key at a recorded size: it verifies against the root the
//! commitment had at that size after more blocks apply, and after a prune it is found again at a
//! later size, where the key's write is still active. The state crate holds such a proof under
//! exactly the root, key and value it verifies for. The longest such proof is the one a crosspond
//! relay frame makes room for.
use std::collections::BTreeMap;

use abi::{Entry, Root};
use commonware_codec::{Decode as _, Encode as _};
use commonware_cryptography::Sha256;
use commonware_cryptography::sha256::Digest;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use commonware_storage::merkle::{Location, MAX_PROOF_DIGESTS_PER_ELEMENT, Proof};
use commonware_storage::qmdb::any::unordered::Update;
use state::{Commitment, Family, Op, OpProof, Slot, digest};

const KEY: &[u8] = b"out/packet";
const VALUE: &[u8] = b"a packet";
const KEYS: u32 = 64;
const BLOCKS: u64 = 120;

fn churn(height: u64) -> BTreeMap<Vec<u8>, Slot> {
    (0..KEYS)
        .map(|key| {
            let name = format!("key/{key}").into_bytes();
            let deleted = (u64::from(key) + height).is_multiple_of(5);
            let value = format!("{height}/{key}").into_bytes();
            (name, (!deleted).then_some(value))
        })
        .collect()
}

fn held(root: Root, proven: &OpProof, key: &[u8], value: &[u8]) -> bool {
    let leaf = abi::encode(&Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    });
    let op: Op = Op::Update(Update(digest(key), leaf));
    let Ok(proof) = Proof::<Family, Digest>::decode_cfg(
        proven.proof.as_slice(),
        &MAX_PROOF_DIGESTS_PER_ELEMENT,
    ) else {
        return false;
    };
    let hasher = commonware_storage::qmdb::hasher::<Sha256>();
    proof.verify_range_inclusion(
        &hasher,
        &[op.encode()],
        Location::new(proven.location),
        &Digest(root.0),
    )
}

#[test]
fn a_proof_at_a_recorded_size_verifies_after_more_blocks_and_a_prune_finds_it_later() {
    deterministic::Runner::default().start(|context| async move {
        let mut commitment = Commitment::open(context.child("crosspond"), "proof", 8)
            .await
            .unwrap();
        let written = BTreeMap::from([(KEY.to_vec(), Some(VALUE.to_vec()))]);
        commitment.apply(1, &written).await.unwrap();
        let recorded = commitment.serving().size();
        let then = commitment.root().unwrap();
        let first = commitment
            .serving()
            .op_proof(recorded, KEY, VALUE)
            .await
            .unwrap()
            .expect("the write is in the log below the recorded size");
        assert!(held(then, &first, KEY, VALUE));

        for height in 2..=BLOCKS / 4 {
            commitment.apply(height, &churn(height)).await.unwrap();
        }
        let serving = commitment.serving();
        let again = serving.op_proof(recorded, KEY, VALUE).await.unwrap();
        assert_eq!(again.as_ref(), Some(&first));
        assert!(held(then, &first, KEY, VALUE));
        assert!(!held(commitment.root().unwrap(), &first, KEY, VALUE));
        assert!(!held(then, &first, KEY, b"another packet"));
        let unwritten = serving.op_proof(recorded, KEY, b"another packet").await;
        assert_eq!(unwritten.unwrap(), None);

        for height in BLOCKS / 4 + 1..=BLOCKS {
            commitment.apply(height, &churn(height)).await.unwrap();
        }
        let serving = commitment.serving();
        let start = *commitment.db().unwrap().bounds().start;
        assert!(
            start > first.location,
            "the commitment pruned past the write"
        );
        assert_eq!(serving.op_proof(recorded, KEY, VALUE).await.unwrap(), None);
        let later = serving.size();
        let now = commitment.root().unwrap();
        let moved = serving
            .op_proof(later, KEY, VALUE)
            .await
            .unwrap()
            .expect("the key is still active, so its write is above the floor");
        assert!(moved.location >= start);
        assert!(held(now, &moved, KEY, VALUE));
        assert_eq!(serving.op_proof(later + 1, KEY, VALUE).await.unwrap(), None);
    });
}

#[test]
fn the_state_crate_holds_a_proof_exactly_where_the_proof_verifies() {
    deterministic::Runner::default().start(|context| async move {
        let mut commitment = Commitment::open(context.child("crosspond"), "holds", 8)
            .await
            .unwrap();
        commitment.apply(1, &churn(1)).await.unwrap();
        let written = BTreeMap::from([(KEY.to_vec(), Some(VALUE.to_vec()))]);
        commitment.apply(2, &written).await.unwrap();
        let recorded = commitment.serving().size();
        let then = commitment.root().unwrap();
        for height in 3..=6 {
            commitment.apply(height, &churn(height)).await.unwrap();
        }
        let proven = commitment
            .serving()
            .op_proof(recorded, KEY, VALUE)
            .await
            .unwrap()
            .expect("the write is below the recorded size");
        assert!(held(then, &proven, KEY, VALUE));
        assert!(state::holds(&then, &proven, KEY, VALUE));
        assert!(!state::holds(&then, &proven, KEY, b"another packet"));
        assert!(!state::holds(&then, &proven, b"out/another", VALUE));
        assert!(!state::holds(
            &commitment.root().unwrap(),
            &proven,
            KEY,
            VALUE
        ));
        let moved = OpProof {
            location: proven.location + 1,
            proof: proven.proof.clone(),
        };
        assert!(!state::holds(&then, &moved, KEY, VALUE));
        let cut = OpProof {
            location: proven.location,
            proof: proven.proof[1..].to_vec(),
        };
        assert!(!state::holds(&then, &cut, KEY, VALUE));
    });
}

#[test]
fn the_longest_op_proof_a_relay_frame_makes_room_for_is_the_longest_one_element_proof() {
    use abi::role::crosspond::{LONGEST_OP_PROOF, OP_PROOF_DIGESTS, OP_PROOF_LEAVES};
    use commonware_storage::merkle::Family as _;
    assert_eq!(OP_PROOF_DIGESTS, MAX_PROOF_DIGESTS_PER_ELEMENT);
    assert_eq!(OP_PROOF_LEAVES, *Family::MAX_LEAVES);
    let longest = Proof::<Family, Digest> {
        leaves: Family::MAX_LEAVES,
        inactive_peaks: usize::MAX,
        digests: vec![Digest([0; 32]); MAX_PROOF_DIGESTS_PER_ELEMENT],
    };
    assert_eq!(longest.encode().len(), LONGEST_OP_PROOF);
}
