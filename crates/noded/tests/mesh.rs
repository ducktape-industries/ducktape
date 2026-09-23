use abi::valset::{Member, Seating};
use commonware_cryptography::{Signer as _, ed25519};
use noded::{PEERS_PER_SET, tracked};

fn key(seed: u64) -> Vec<u8> {
    ed25519::PrivateKey::from_seed(seed)
        .public_key()
        .as_ref()
        .to_vec()
}

fn member(seed: u64) -> Member {
    Member {
        key: key(seed),
        address: format!("198.51.100.{}:{}", seed % 250, 9000 + seed),
    }
}

#[test]
fn a_seating_that_fits_is_tracked_whole() {
    let seating = Seating {
        validators: vec![key(1), key(2)],
        members: (1..=5).map(member).collect(),
    };
    let tracked = tracked(&seating, &key(9));
    assert_eq!(tracked.peers.len(), 5);
    assert_eq!(tracked.dropped, 0);
    assert_eq!(tracked.unreachable, 0);
}

#[test]
fn an_oversized_seating_keeps_the_validators_and_the_node_itself() {
    let residents: Vec<Member> = (100..100 + PEERS_PER_SET as u64 + 50).map(member).collect();
    let local = residents[residents.len() - 1].key.clone();
    let seating = Seating {
        validators: vec![key(1), key(2)],
        members: residents
            .into_iter()
            .chain([member(1), member(2)])
            .collect(),
    };
    let tracked = tracked(&seating, &local);
    let keys: Vec<Vec<u8>> = tracked
        .peers
        .iter()
        .map(|(key, _)| key.as_ref().to_vec())
        .collect();
    assert!(keys.contains(&key(1)));
    assert!(keys.contains(&key(2)));
    assert!(keys.contains(&local));
    assert_eq!(tracked.peers.len(), PEERS_PER_SET);
    assert_eq!(tracked.dropped, seating.members.len() - PEERS_PER_SET);
}

#[test]
fn a_member_with_an_unparsable_address_is_counted_not_tracked() {
    let mut broken = member(3);
    broken.address = "nowhere".to_owned();
    let seating = Seating {
        validators: vec![key(1)],
        members: vec![member(1), broken],
    };
    let tracked = tracked(&seating, &key(9));
    assert_eq!(tracked.peers.len(), 1);
    assert_eq!(tracked.unreachable, 1);
}
