use std::path::Path;

use super::*;
use crate::history::Durability;

fn closed(epoch: u64) -> Closed {
    Closed {
        epoch,
        height: epoch * 8 - 1,
        roots: vec![("crosspond".to_owned(), Root([epoch as u8; 32]))],
        sizes: BTreeMap::from([("crosspond".to_owned(), epoch * 3)]),
        validators: vec![vec![epoch as u8; 32]],
    }
}

fn opened(dir: &Path) -> Closes {
    Closes::load(History::open(dir, Durability::EachBlock).unwrap()).unwrap()
}

fn closes(dir: &Path, epochs: &[u64]) -> Closes {
    let closes = opened(dir);
    for epoch in epochs {
        closes.keep(closed(*epoch)).unwrap();
    }
    closes
}

fn epochs(closes: &Closes) -> Vec<u64> {
    let mut epochs: Vec<u64> = closes
        .newest_first()
        .iter()
        .map(|closed| closed.epoch)
        .collect();
    epochs.reverse();
    epochs
}

const ONE: NetworkId = [1; 32];
const TWO: NetworkId = [2; 32];

#[test]
fn a_record_goes_once_every_linked_network_holds_a_header_of_its_epoch_or_later() {
    let dir = tempfile::tempdir().unwrap();
    let closes = closes(dir.path(), &[1, 2, 3, 4]);
    closes.held(ONE, 3, [ONE, TWO]).unwrap();
    assert_eq!(epochs(&closes), [1, 2, 3, 4]);
    closes.held(TWO, 2, [ONE, TWO]).unwrap();
    assert_eq!(epochs(&closes), [2, 3, 4]);
    closes.held(TWO, 4, [ONE, TWO]).unwrap();
    assert_eq!(epochs(&closes), [3, 4]);
    assert_eq!(closes.of(3).map(|closed| closed.height), Some(23));
    assert_eq!(closes.of(2), None);
}

#[test]
fn no_record_goes_while_no_network_is_linked() {
    let dir = tempfile::tempdir().unwrap();
    let closes = closes(dir.path(), &[1, 2]);
    closes.held(ONE, 2, []).unwrap();
    assert_eq!(epochs(&closes), [1, 2]);
}

#[test]
fn every_kept_record_is_read_back_whole_when_the_node_opens_its_history_again() {
    let dir = tempfile::tempdir().unwrap();
    drop(closes(dir.path(), &[3, 1, 2]));
    let reopened = opened(dir.path());
    assert_eq!(epochs(&reopened), [1, 2, 3]);
    for epoch in [1, 2, 3] {
        assert_eq!(reopened.of(epoch).as_deref(), Some(&closed(epoch)));
    }
}

#[test]
fn a_record_dropped_once_every_link_holds_its_epoch_stays_dropped_when_the_node_opens_again() {
    let dir = tempfile::tempdir().unwrap();
    let closes = closes(dir.path(), &[1, 2, 3, 4]);
    closes.held(ONE, 3, [ONE]).unwrap();
    drop(closes);
    let reopened = opened(dir.path());
    assert_eq!(epochs(&reopened), [3, 4]);
    reopened.keep(closed(5)).unwrap();
    drop(reopened);
    assert_eq!(epochs(&opened(dir.path())), [3, 4, 5]);
}

#[test]
fn a_record_kept_again_for_its_epoch_replaces_the_one_before() {
    let dir = tempfile::tempdir().unwrap();
    let closes = closes(dir.path(), &[1]);
    let again = Closed {
        height: 99,
        ..closed(1)
    };
    closes.keep(again.clone()).unwrap();
    assert_eq!(closes.of(1).as_deref(), Some(&again));
    drop(closes);
    assert_eq!(opened(dir.path()).of(1).as_deref(), Some(&again));
}
