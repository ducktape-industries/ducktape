use host::Tip;
use node::{Block, Settled};

use super::{Opening, Unopened, required_after};

const FIRST: u64 = 64;

fn held() -> Settled {
    Settled {
        height: FIRST - 1,
        root: [7; 32],
    }
}

fn opening(required: bool) -> Opening {
    Opening {
        first: FIRST,
        settled: Some(held()),
        required,
    }
}

fn block(height: u64, frames: Vec<Vec<u8>>) -> Block {
    let parent = Tip {
        height: height - 1,
        id: [0; 32],
    };
    Block::next(parent, 0, frames)
}

fn sealed(height: u64, records: &[Settled]) -> Block {
    let mut block = block(height, vec![vec![1]]);
    for record in records {
        block.settle(*record);
    }
    block
}

#[test]
fn an_opening_whose_last_frame_is_the_held_record_is_admitted() {
    assert_eq!(opening(true).admits(&sealed(FIRST, &[held()])), Ok(()));
    assert_eq!(opening(false).admits(&sealed(FIRST, &[held()])), Ok(()));
}

#[test]
fn a_block_past_the_opening_is_admitted_with_no_record_and_refused_with_one() {
    assert_eq!(opening(true).admits(&block(FIRST + 1, Vec::new())), Ok(()));
    assert_eq!(
        opening(true).admits(&sealed(FIRST + 1, &[held()])),
        Err(Unopened::Stray)
    );
}

#[test]
fn an_opening_with_no_record_is_admitted_only_until_records_are_required() {
    assert_eq!(opening(false).admits(&block(FIRST, Vec::new())), Ok(()));
    assert_eq!(
        opening(true).admits(&block(FIRST, Vec::new())),
        Err(Unopened::Missing)
    );
}

#[test]
fn an_opening_carrying_two_records_is_refused() {
    assert_eq!(
        opening(false).admits(&sealed(FIRST, &[held(), held()])),
        Err(Unopened::Doubled)
    );
}

#[test]
fn an_opening_whose_record_is_not_its_last_frame_is_refused() {
    let block = block(FIRST, vec![held().frame(), vec![1]]);
    assert_eq!(opening(false).admits(&block), Err(Unopened::Misplaced));
}

#[test]
fn an_opening_is_refused_by_a_node_that_holds_no_root_for_it() {
    let unheld = Opening {
        settled: None,
        ..opening(false)
    };
    assert_eq!(
        unheld.admits(&sealed(FIRST, &[held()])),
        Err(Unopened::Unsettled)
    );
}

#[test]
fn an_opening_naming_another_root_or_height_is_refused() {
    let root = Settled {
        root: [8; 32],
        ..held()
    };
    let height = Settled {
        height: FIRST - 2,
        ..held()
    };
    assert_eq!(
        opening(false).admits(&sealed(FIRST, &[root])),
        Err(Unopened::Wrong)
    );
    assert_eq!(
        opening(false).admits(&sealed(FIRST, &[height])),
        Err(Unopened::Wrong)
    );
}

#[test]
fn records_are_required_once_the_last_epochs_opening_carried_one() {
    assert!(!required_after(None));
    assert!(!required_after(Some(&block(FIRST, Vec::new()))));
    assert!(required_after(Some(&sealed(FIRST, &[held()]))));
}
