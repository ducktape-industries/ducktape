//! Tests for the window of recent block summaries the block read routes answer from.
use std::sync::Arc;

use host::{ANCHOR_WINDOW, Tip};

use super::{Kept, Ranged, Recent, facts_of, listed_limit, row_of};
use crate::history::{Durability, History, Row, WINDOW};
use crate::wire::{Finalized, MOST_LISTED, Tx, TxPage, TxPlace, TxRange};

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

fn sent(hash: u8, target: &str) -> Tx {
    Tx {
        target: target.into(),
        ..tx(hash, 0)
    }
}

fn places(page: &TxPage) -> Vec<(u64, u32)> {
    page.txs.iter().map(|tx| (tx.height, tx.position)).collect()
}

fn place(height: u64, position: u32) -> TxPlace {
    TxPlace { height, position }
}

fn range(before: Option<TxPlace>, target: Option<&str>, limit: u32) -> TxRange {
    TxRange {
        before,
        oldest: 0,
        target: target.map(Into::into),
        limit,
    }
}

fn chain() -> Vec<Arc<Kept>> {
    vec![
        kept(3, vec![sent(1, "chat")]),
        kept(4, vec![sent(2, "chat"), sent(3, "files"), sent(4, "chat")]),
        kept(5, vec![sent(5, "chat"), sent(6, "chat")]),
    ]
}

fn paged(blocks: &[Arc<Kept>], range: &TxRange) -> (TxPage, Vec<u64>) {
    let heights: Vec<u64> = blocks.iter().map(|kept| kept.block.height).collect();
    let lowest = heights.iter().copied().min().expect("a kept block");
    let newest = heights.iter().copied().max().expect("a kept block");
    let mut ranged = Ranged::new(range, lowest, newest);
    let mut read = Vec::new();
    while let Some(height) = ranged.wants() {
        read.push(height);
        let kept = blocks
            .iter()
            .find(|kept| kept.block.height == height)
            .expect("a kept height");
        ranged.take(&kept.block);
    }
    (ranged.page(), read)
}

#[test]
fn a_range_pages_newest_first_by_its_cursor_to_its_end() {
    let chain = chain();
    let mut before = None;
    let mut seen = Vec::new();
    loop {
        let (page, read) = paged(&chain, &range(before, None, 2));
        assert!(
            read.len() <= 2,
            "a page reads at most limit blocks: {read:?}"
        );
        seen.extend(places(&page));
        let Some(next) = page.next else {
            break;
        };
        before = Some(next);
    }
    assert_eq!(seen, [(5, 1), (5, 0), (4, 2), (4, 1), (4, 0), (3, 0)]);
}

#[test]
fn a_one_row_ask_reads_at_most_the_one_block_past_its_cursor() {
    let chain = chain();
    let (page, read) = paged(&chain, &range(Some(place(4, 2)), None, 1));
    assert_eq!(read, [4]);
    assert_eq!(places(&page), [(4, 1)]);
    assert_eq!(page.next, Some(place(4, 1)));
    let (page, read) = paged(&chain, &range(Some(place(4, 0)), None, 1));
    assert_eq!(read, [3]);
    assert_eq!(places(&page), [(3, 0)]);
    assert_eq!(page.next, None, "the chain ends below block 3");
    let (page, read) = paged(&chain, &range(None, Some("files"), 1));
    assert_eq!(
        read,
        [5],
        "a filtered ask reads no further than its limit of blocks"
    );
    assert!(page.txs.is_empty());
    assert_eq!(page.next, Some(place(5, 0)));
    let (page, read) = paged(&chain, &range(Some(place(5, 0)), Some("files"), 1));
    assert_eq!(read, [4]);
    assert_eq!(places(&page), [(4, 1)]);
}

#[test]
fn a_range_reads_no_block_below_its_oldest() {
    let chain = chain();
    let newest_only = TxRange {
        before: None,
        oldest: 5,
        target: None,
        limit: u32::MAX,
    };
    let (page, read) = paged(&chain, &newest_only);
    assert_eq!(read, [5]);
    assert_eq!(places(&page), [(5, 1), (5, 0)]);
    assert_eq!(page.next, None);
}

#[test]
fn every_list_answers_at_most_the_blocks_the_history_keeps() {
    assert_eq!(listed_limit(u32::MAX), MOST_LISTED as usize);
    assert_eq!(listed_limit(3), 3);
    let crowded: Vec<Tx> = (0..=MOST_LISTED).map(|hash| tx(hash as u8, 0)).collect();
    let (page, _) = paged(&[kept(9, crowded)], &range(None, None, u32::MAX));
    assert_eq!(page.txs.len(), MOST_LISTED as usize);
    assert_eq!(page.next, Some(place(9, 1)));
}

#[test]
fn facts_count_a_block_s_transactions_without_carrying_them() {
    let kept = kept(7, vec![tx(1, 0), tx(2, 0)]);
    let facts = facts_of(&kept.block);
    assert_eq!((facts.height, facts.id, facts.txs), (7, kept.block.id, 2));
}
