use std::sync::atomic::Ordering;
use std::sync::mpsc;

use programs_sdk::{Env, ExecCtx, MockHost};
use store::Map;

use super::*;

const PROGRAM_FRAMES: Map<(u64, u32), Frame> = Map::new("f/").shared();
const PROGRAM_SIGNED: Map<(Vec<u8>, u64, u32), Frame> = Map::new("k/").shared();
const PROGRAM_ACTED: Map<(AccountNumber, u64, u32), Frame> = Map::new("a/").shared();
const PROGRAM_USED: Map<(AccountNumber, String), u64> = Map::new("u/").shared();

fn keep(ctx: &ExecCtx, record: Record) {
    let frame = Frame {
        height: record.height,
        position: record.position,
        time: ctx.env().time,
        signer: record.signer,
        target: record.target,
        applied: record.applied,
    };
    let (height, position) = (u64::MAX - frame.height, u32::MAX - frame.position);
    PROGRAM_FRAMES.put(ctx, &(height, position), &frame);
    PROGRAM_SIGNED.put(ctx, &(frame.signer.clone(), height, position), &frame);
    let Some(account) = record.account else {
        return;
    };
    PROGRAM_ACTED.put(ctx, &(account, height, position), &frame);
    let used = (account, frame.target);
    let frames = PROGRAM_USED.get(ctx, &used).unwrap().unwrap_or(0);
    PROGRAM_USED.put(ctx, &used, &(frames + 1));
}

fn record(height: u64, position: u32, signer: u8, account: Option<u64>, target: &str) -> Record {
    Record {
        height,
        position,
        signer: vec![signer, 0, signer],
        account,
        target: target.to_owned(),
        applied: position.is_multiple_of(2),
    }
}

fn block(height: u64, records: Vec<Record>) -> Block {
    Block {
        height,
        time: height * 1000,
        records,
        refused: Vec::new(),
    }
}

fn chain() -> Vec<Block> {
    vec![
        block(
            1,
            vec![
                record(1, 0, 1, Some(1), "chat"),
                record(1, 1, 2, None, "chat"),
                record(1, 2, 1, Some(1), "chat"),
            ],
        ),
        block(
            2,
            vec![
                record(2, 0, 0, Some(2), "files"),
                record(2, 1, 1, Some(1), "files"),
            ],
        ),
        block(3, Vec::new()),
        block(4, vec![record(4, 5, 2, Some(1), "chat")]),
    ]
}

fn kept_by_the_program(host: &MockHost, blocks: &[Block]) {
    for block in blocks {
        let ctx = host.exec(Env {
            height: block.height,
            time: block.time,
            ..MockHost::env(PROGRAM)
        });
        for record in &block.records {
            keep(&ctx, record.clone());
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

const LAG: Lag = Lag { from: 0, epoch: 2 };

fn opened<S>(dir: &Path, seed: S) -> Result<Index>
where
    S: FnOnce() -> Result<Seed> + Send + 'static,
{
    Index::open(dir, LAG, seed)
}

fn empty() -> Result<Seed> {
    Ok(Seed {
        height: 0,
        rows: Vec::new(),
    })
}

fn indexed_to(index: &Index, height: u64) -> Indexed {
    let mut indexed = index.indexed();
    let reached =
        runtime().block_on(indexed.wait_for(|span| span.is_some_and(|span| span.height == height)));
    reached.unwrap().unwrap()
}

fn fed(dir: &Path, blocks: Vec<Block>) -> Index {
    let index = opened(dir, empty).unwrap();
    let top = blocks.last().map_or(0, |block| block.height);
    for block in blocks {
        index.offer(block);
    }
    indexed_to(&index, top);
    index
}

fn asked(index: &Index, query: Query) -> std::result::Result<Reply, Refusal> {
    index.reads().read(query).unwrap()
}

fn queries(page: PageRequest) -> Vec<Query> {
    vec![
        Query::Frames { page: page.clone() },
        Query::Signed {
            key: vec![1, 0, 1],
            page: page.clone(),
        },
        Query::Signed {
            key: vec![0, 0, 0],
            page: page.clone(),
        },
        Query::Acted {
            account: 1,
            page: page.clone(),
        },
        Query::Acted {
            account: 3,
            page: page.clone(),
        },
        Query::Used { page },
    ]
}

fn resumed_with(query: &Query, after: Option<Vec<u8>>) -> Query {
    let again = |page: &PageRequest| PageRequest {
        after: after.clone(),
        limit: page.limit,
    };
    match query {
        Query::Frames { page } => Query::Frames { page: again(page) },
        Query::Signed { key, page } => Query::Signed {
            key: key.clone(),
            page: again(page),
        },
        Query::Acted { account, page } => Query::Acted {
            account: *account,
            page: again(page),
        },
        Query::Used { page } => Query::Used { page: again(page) },
        Query::Indexed => Query::Indexed,
        Query::Refusal { .. } => query.clone(),
    }
}

fn the_programs(host: &MockHost, height: u64, query: &Query) -> (Vec<u8>, Option<Vec<u8>>) {
    let ctx = host.query(Env {
        height,
        ..MockHost::env(PROGRAM)
    });
    let sdk = |page: &PageRequest| store::PageRequest {
        after: page.after.clone(),
        limit: page.limit,
    };
    let frames = |page: store::PageResponse<Frame>| {
        let next = page.next.clone();
        ([vec![0], abi::encode(&page)].concat(), next)
    };
    match query {
        Query::Frames { page } => frames(
            PROGRAM_FRAMES
                .range(&ctx, &sdk(page))
                .unwrap()
                .map(|(_, frame)| frame),
        ),
        Query::Signed { key, page } => frames(
            PROGRAM_SIGNED
                .range_of(&ctx, key, &sdk(page))
                .unwrap()
                .map(|(_, frame)| frame),
        ),
        Query::Acted { account, page } => frames(
            PROGRAM_ACTED
                .range_of(&ctx, account, &sdk(page))
                .unwrap()
                .map(|(_, frame)| frame),
        ),
        Query::Used { page } => {
            let used = PROGRAM_USED.range(&ctx, &sdk(page)).unwrap().map(
                |((account, program), frames)| Used {
                    account,
                    program,
                    frames,
                },
            );
            let next = used.next.clone();
            ([vec![1], abi::encode(&used)].concat(), next)
        }
        Query::Indexed | Query::Refusal { .. } => panic!("the program answers neither"),
    }
}

fn next_of(reply: &Reply) -> Option<Vec<u8>> {
    match reply {
        Reply::Frames(page) => page.next.clone(),
        Reply::Used(page) => page.next.clone(),
        Reply::Indexed(_) | Reply::Refusal(_) => None,
    }
}

fn every_page(index: &Index, query: &Query) -> Vec<Vec<u8>> {
    let mut pages = Vec::new();
    let mut next = None;
    loop {
        let reply = asked(index, resumed_with(query, next)).unwrap();
        pages.push(abi::encode(&reply));
        next = next_of(&reply);
        if next.is_none() {
            return pages;
        }
    }
}

#[test]
fn every_page_is_the_programs_page_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let index = fed(dir.path(), chain());
    let host = MockHost::default();
    kept_by_the_program(&host, &chain());
    for limit in [None, Some(1), Some(2), Some(3)] {
        for query in queries(PageRequest { after: None, limit }) {
            let mut ours = None;
            let mut theirs = None;
            loop {
                let reply = asked(&index, resumed_with(&query, ours.clone())).unwrap();
                let (bytes, next) = the_programs(&host, 4, &resumed_with(&query, theirs));
                assert_eq!(abi::encode(&reply), bytes, "{query:?}");
                ours = next_of(&reply);
                theirs = next;
                if ours.is_none() {
                    break;
                }
            }
        }
    }
    let Reply::Used(used) = asked(
        &index,
        Query::Used {
            page: PageRequest::default(),
        },
    )
    .unwrap() else {
        panic!("Used answers counts");
    };
    let counts: Vec<(u64, &str, u64)> = used
        .items
        .iter()
        .map(|used| (used.account, used.program.as_str(), used.frames))
        .collect();
    assert_eq!(counts, [(1, "chat", 3), (1, "files", 1), (2, "files", 1)]);
    assert_eq!(
        asked(&index, Query::Indexed),
        Ok(Reply::Indexed(Indexed { from: 1, height: 4 }))
    );
}

#[test]
fn a_cursor_stays_in_its_listing() {
    let dir = tempfile::tempdir().unwrap();
    let index = fed(dir.path(), chain());
    let first = asked(
        &index,
        Query::Frames {
            page: PageRequest {
                after: None,
                limit: Some(1),
            },
        },
    )
    .unwrap();
    let elsewhere = Query::Used {
        page: PageRequest {
            after: next_of(&first),
            limit: None,
        },
    };
    assert_eq!(asked(&index, elsewhere).unwrap_err().reason, reason::STALE);
    let garbage = Query::Frames {
        page: PageRequest {
            after: Some(vec![1]),
            limit: None,
        },
    };
    assert_eq!(
        asked(&index, garbage).unwrap_err().reason,
        reason::INVALID_INPUT
    );
}

#[test]
fn a_seeded_index_answers_as_the_one_it_was_seeded_from_and_goes_on_alike() {
    let root = tempfile::tempdir().unwrap();
    let fed_one = fed(&root.path().join("fed"), chain());
    let rows: Vec<Entry> = fed_one
        .reads()
        .db
        .iter(None, None, false)
        .unwrap()
        .map(|entry| {
            let (key, value) = entry.unwrap();
            Entry { key, value }
        })
        .collect();
    let seeded_one = opened(&root.path().join("seeded"), move || {
        Ok(Seed { height: 4, rows })
    })
    .unwrap();
    indexed_to(&seeded_one, 4);
    let fifth = || {
        block(
            5,
            vec![
                record(5, 0, 1, Some(1), "chat"),
                record(5, 1, 3, Some(3), "board"),
            ],
        )
    };
    for index in [&fed_one, &seeded_one] {
        index.offer(fifth());
        indexed_to(index, 5);
    }
    for limit in [None, Some(2)] {
        for query in queries(PageRequest { after: None, limit }) {
            assert_eq!(
                every_page(&fed_one, &query),
                every_page(&seeded_one, &query),
                "{query:?}"
            );
        }
    }
    assert_eq!(
        asked(&seeded_one, Query::Indexed),
        Ok(Reply::Indexed(Indexed { from: 1, height: 5 }))
    );
}

#[test]
fn an_index_seeded_with_no_rows_holds_history_from_the_next_block() {
    let dir = tempfile::tempdir().unwrap();
    let index = opened(dir.path(), || {
        Ok(Seed {
            height: 9,
            rows: Vec::new(),
        })
    })
    .unwrap();
    assert_eq!(
        indexed_to(&index, 9),
        Indexed {
            from: 10,
            height: 9
        }
    );
}

#[test]
fn a_block_indexed_already_is_skipped_and_a_missed_one_moves_the_history_on() {
    let dir = tempfile::tempdir().unwrap();
    let index = fed(dir.path(), chain());
    index.offer(block(2, vec![record(2, 0, 0, Some(2), "files")]));
    index.offer(block(7, vec![record(7, 0, 0, Some(2), "files")]));
    assert_eq!(indexed_to(&index, 7), Indexed { from: 7, height: 7 });
    let Reply::Used(used) = asked(
        &index,
        Query::Used {
            page: PageRequest::default(),
        },
    )
    .unwrap() else {
        panic!("Used answers counts");
    };
    let files = used
        .items
        .iter()
        .find(|used| used.account == 2)
        .map(|used| used.frames);
    assert_eq!(files, Some(2), "block 2 counted once, block 7 once");
}

#[test]
fn a_stalled_index_takes_every_block_without_holding_its_feeder() {
    let dir = tempfile::tempdir().unwrap();
    let (release, stalled) = mpsc::channel::<()>();
    let index = opened(dir.path(), move || {
        stalled.recv().unwrap();
        empty()
    })
    .unwrap();
    for block in chain() {
        index.offer(block);
    }
    assert_eq!(*index.indexed().borrow(), None);
    assert!(
        index.lagging.load(Ordering::Relaxed),
        "four blocks behind is past an epoch of two"
    );
    assert_eq!(
        asked(&index, Query::Indexed).unwrap_err().reason,
        reason::NOT_YET
    );
    release.send(()).unwrap();
    assert_eq!(indexed_to(&index, 4), Indexed { from: 1, height: 4 });
    index.offer(block(5, Vec::new()));
    assert!(
        !index.lagging.load(Ordering::Relaxed),
        "an index that caught up is no longer behind"
    );
}

#[test]
fn an_index_pacing_an_epoch_behind_logs_its_lag_once_until_it_catches_up() {
    let epoch = 2;
    let lags = [1, 3, 2, 3, 2, 3, 2, 1, 2, 3, 0, 3];
    let mut behind = false;
    let mut logged = Vec::new();
    for (at, lag) in lags.into_iter().enumerate() {
        let step = lagged(behind, lag, epoch);
        behind = step.behind;
        if step.logs {
            logged.push(at);
        }
    }
    assert_eq!(
        logged,
        [1, 9, 11],
        "once each time it falls behind, never while pacing an epoch back"
    );
}

#[test]
fn a_closed_index_reopens_where_it_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let index = fed(dir.path(), chain());
    runtime().block_on(index.close());
    drop(index);
    let reopened = opened(dir.path(), || {
        panic!("an index that holds a height is not seeded")
    })
    .unwrap();
    assert_eq!(indexed_to(&reopened, 4), Indexed { from: 1, height: 4 });
}

#[test]
fn a_refused_frame_keeps_why_and_an_applied_one_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let records = vec![
        record(1, 0, 1, Some(1), "chat"),
        record(1, 1, 1, Some(1), "chat"),
    ];
    let refusal = Refusal::new(reason::UNAUTHORIZED, "not yours");
    let receipt = |outcome| Receipt {
        program: "chat".into(),
        outcome,
        events: Vec::new(),
        nested: Vec::new(),
    };
    let receipts = [
        receipt(Outcome::Applied { output: Vec::new() }),
        receipt(Outcome::Rejected(refusal.clone())),
    ];
    let refused = refused(&records, &receipts);
    assert_eq!(refused, [(1, refusal.clone())]);
    let index = fed(
        dir.path(),
        vec![Block {
            height: 1,
            time: 1000,
            records,
            refused,
        }],
    );
    let why = |position| {
        asked(
            &index,
            Query::Refusal {
                height: 1,
                position,
            },
        )
    };
    assert_eq!(why(1), Ok(Reply::Refusal(Some(refusal))));
    assert_eq!(why(0), Ok(Reply::Refusal(None)));
    assert_eq!(why(9), Ok(Reply::Refusal(None)));
}
