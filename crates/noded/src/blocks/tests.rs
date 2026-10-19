//! Tests for the window of recent block summaries the block read routes answer from.
use std::sync::Arc;

use host::{ANCHOR_WINDOW, Tip};

use super::{Kept, Recent};
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
