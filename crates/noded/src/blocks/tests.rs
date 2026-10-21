//! Tests for the window of recent block summaries the block read routes answer from.
use std::sync::Arc;

use host::{ANCHOR_WINDOW, Tip};

use super::{Kept, Recent, row_of};
use crate::history::{Durability, History, Row, WINDOW};
use crate::wire::{Finalized, Tx};

fn tx(hash: u8, anchor: u64) -> Tx {
    Tx {
        hash: [hash; 32],
        signer: Vec::new(),
        anchor: Tip {
            height: anchor,
            id: [0; 32],
        },
        target: "probe".into(),
        size: 0,
        payload: None,
        receipt: None,
    }
}

fn kept(height: u64, txs: Vec<Tx>) -> Arc<Kept> {
    let mut id = [0; 32];
    id[..8].copy_from_slice(&height.to_le_bytes());
    Arc::new(Kept {
        block: Finalized {
            height,
            id,
            parent: [0; 32],
            time: 0,
            epoch: 0,
            proposer: None,
            txs,
        },
        frames: Vec::new(),
    })
}

fn height_of(kept: Option<Arc<Kept>>) -> Option<u64> {
    kept.map(|kept| kept.block.height)
}

#[test]
fn the_window_holds_the_newest_anchor_window_of_heights() {
    let recent = Recent::new();
    let newest = ANCHOR_WINDOW + 2;
    for height in 1..=newest {
        recent.keep(kept(height, Vec::new()));
    }
    let oldest = newest + 1 - ANCHOR_WINDOW;
    assert_eq!(height_of(recent.at(newest)), Some(newest));
    assert_eq!(height_of(recent.at(oldest)), Some(oldest));
    assert_eq!(height_of(recent.at(oldest - 1)), None);
    assert_eq!(height_of(recent.at(newest + 1)), None);
    let mut id = [0; 32];
    id[..8].copy_from_slice(&oldest.to_le_bytes());
    assert_eq!(height_of(recent.by_id(id)), Some(oldest));
}

#[test]
fn a_height_that_does_not_follow_starts_the_window_again() {
    let recent = Recent::new();
    recent.keep(kept(5, Vec::new()));
    recent.keep(kept(6, Vec::new()));
    recent.keep(kept(9, Vec::new()));
    assert_eq!(height_of(recent.at(9)), Some(9));
    assert_eq!(height_of(recent.at(6)), None);
}

#[test]
fn a_transaction_is_placed_from_the_window_only_when_no_landing_can_precede_it() {
    let recent = Recent::new();
    recent.keep(kept(10, vec![tx(1, 4)]));
    recent.keep(kept(11, vec![tx(2, 10), tx(3, 10)]));
    recent.keep(kept(12, vec![tx(3, 10)]));
    assert_eq!(recent.place([3; 32]), Some((11, 1)));
    assert_eq!(recent.place([2; 32]), Some((11, 0)));
    assert_eq!(recent.place([1; 32]), None);
    assert_eq!(recent.place([9; 32]), None);
}

fn landed(height: u64) -> Tx {
    let mut landed = tx(7, height);
    landed.hash[..8].copy_from_slice(&height.to_le_bytes());
    landed
}

fn summarized(height: u64) -> Row {
    let kept = kept(height, vec![landed(height)]);
    let payloads = vec![height.to_le_bytes().to_vec()];
    row_of(&kept, abi::encode(&*kept), payloads)
}

fn id_of(height: u64) -> [u8; 32] {
    kept(height, Vec::new()).block.id
}

#[test]
fn history_keeps_its_window_and_lets_go_of_each_block_that_falls_below_it() {
    let dir = tempfile::tempdir().unwrap();
    let history = History::open(dir.path(), Durability::AtClose).unwrap();
    let newest = WINDOW + 3;
    for height in 0..=newest {
        history.keep(summarized(height)).unwrap();
    }
    let bottom = newest - WINDOW;
    assert_eq!(history.span(), Some((bottom, newest)));
    assert!(history.summary(bottom).unwrap().is_some());
    assert!(history.summary_by_id(id_of(bottom)).unwrap().is_some());
    assert_eq!(
        history.place(landed(bottom).hash).unwrap(),
        Some((bottom, 0))
    );
    assert!(history.payload(bottom, 0).unwrap().is_some());
    for gone in 0..bottom {
        assert_eq!(history.summary(gone).unwrap(), None, "{gone}");
        assert_eq!(history.summary_by_id(id_of(gone)).unwrap(), None, "{gone}");
        assert_eq!(history.place(landed(gone).hash).unwrap(), None, "{gone}");
        assert_eq!(history.payload(gone, 0).unwrap(), None, "{gone}");
    }
}

#[test]
fn history_opened_over_older_blocks_lets_go_of_everything_below_the_floor_once() {
    let dir = tempfile::tempdir().unwrap();
    let history = History::open(dir.path(), Durability::AtClose).unwrap();
    for height in 0..10 {
        history.keep(summarized(height)).unwrap();
    }
    assert_eq!(history.forget_below(6).unwrap(), 6);
    assert_eq!(history.span(), Some((6, 9)));
    assert_eq!(history.summary(5).unwrap(), None);
    assert_eq!(history.place(landed(5).hash).unwrap(), None);
    assert_eq!(history.payload(5, 0).unwrap(), None);
    assert!(history.summary(6).unwrap().is_some());
    assert_eq!(history.place(landed(6).hash).unwrap(), Some((6, 0)));
    assert_eq!(
        history.forget_below(6).unwrap(),
        0,
        "a floor already reached lets go of nothing"
    );
}
