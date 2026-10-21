//! The spent set's committed rows and its node-local index: the `$spent` root a fixed run of
//! blocks gives below the switch (pinned from the layout keyed by anchor) and across it, and
//! what a program, a reopen and a rebuild of the index see.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use abi::role::{self, halo, validators};
use abi::{Entry, HostOp, HostReply, Outcome, Refusal, Scan, reason};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic, tokio};
use fixture_probe::{Reply, Step};
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, VALSET,
};
use host::{
    ANCHOR_WINDOW, Block, BlockId, Founding, Genesis, Host, INDEXED_AT, LOCAL, Layer, Limits,
    SPENT, Submission, Tip,
};
use state::Storage;

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;
const BLOCKS: u64 = ANCHOR_WINDOW + 64;
const SWITCH: u64 = 40;
const ACROSS: u64 = SWITCH + ANCHOR_WINDOW + 40;
const ROOT: &str = "87056b3c89767d2e50d71077bbe44c5d1f57b14ea2592a510cca735290b1f6b9";
const FLOOD: u64 = 70_000;
const STEADY: [u64; 4] = [
    ANCHOR_WINDOW + 1,
    ANCHOR_WINDOW + 2,
    ANCHOR_WINDOW + 3,
    ANCHOR_WINDOW + 4,
];
const ROOT_ACROSS: &str = "2b4852e521ddab2d0276736f112b00090e519da392373e43b47bf72eb4fcb436";

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    let roles: &[&str] = match program {
        "programs" => &[role::PROGRAMS],
        "valset" => &[role::VALIDATORS],
        "identity" => &[role::IDENTITY],
        "blobs" => &[role::BLOBS],
        "awaits" => &[role::AWAITS],
        "halo" => &[role::HALO],
        "access" => &[role::ACCESS],
        _ => &[],
    };
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn genesis(network: &[u8]) -> Genesis {
    Genesis {
        network: network.to_vec(),
        validators: vec![validators::Member {
            key: b"v1".to_vec(),
            address: Some("v1:1".into()),
            halo: "v1:1".into(),
        }],
        programs: vec![
            founding("programs", PROGRAMS, Vec::new()),
            founding("valset", VALSET, Vec::new()),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), 1u64)]),
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new()),
            founding("awaits", AWAITS, Vec::new()),
            founding("probe", PROBE, abi::encode(&Vec::<Step>::new())),
            founding(
                "halo",
                HALO,
                abi::encode(&halo::Genesis { lanes: Vec::new() }),
            ),
            founding("access", ACCESS, Vec::new()),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: 1_000,
        time: TIME,
        member_cap: 16,
    }
}

async fn found(context: Ctx, name: &str, dir: &Path, network: &[u8]) -> Host<Ctx> {
    Host::found(context, name, dir, block_id(0), genesis(network))
        .await
        .unwrap()
        .0
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn frame(height: u64, n: u64) -> Submission {
    let lag = 1 + (height * 7 + n * 13) % (ANCHOR_WINDOW + 44);
    let anchored = height.saturating_sub(lag);
    let steps = match n % 3 {
        2 => vec![Step::Fail("no".into())],
        _ => Vec::new(),
    };
    Submission {
        signer: SIGNER.to_vec(),
        anchor: Tip {
            height: anchored,
            id: block_id(anchored),
        },
        salt: Some(height * 16 + n),
        target: "probe".to_owned(),
        payload: abi::encode(&steps),
    }
}

fn block(height: u64) -> Block {
    let mut submissions: Vec<Submission> = (0..height % 4).map(|n| frame(height, n)).collect();
    if height.is_multiple_of(5) {
        submissions.push(frame(height - 3, 0));
    }
    if height.is_multiple_of(11) {
        submissions.push(frame(height, 0));
    }
    carrying(height, submissions)
}

fn carrying(height: u64, submissions: Vec<Submission>) -> Block {
    Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions,
    }
}

async fn run(
    host: &mut Host<Ctx>,
    heights: std::ops::RangeInclusive<u64>,
) -> BTreeMap<String, u64> {
    let mut outcomes = BTreeMap::new();
    for height in heights {
        let applied = host.apply(block(height)).await.unwrap();
        for receipt in &applied.submissions {
            let outcome = match &receipt.outcome {
                Outcome::Applied { .. } => "applied".to_owned(),
                Outcome::Rejected(refusal) => refusal.reason.clone(),
            };
            *outcomes.entry(outcome).or_default() += 1;
        }
    }
    outcomes
}

fn root(host: &Host<Ctx>) -> String {
    abi::hex(&host.store().root(SPENT).unwrap().unwrap().0)
}

fn rows(host: &Host<Ctx>) -> Vec<Entry> {
    host.view(Layer::Confirmed)
        .scan(SPENT, &Scan::prefix(b""))
        .unwrap()
}

fn shapes(host: &Host<Ctx>) -> BTreeSet<usize> {
    rows(host).iter().map(|entry| entry.key.len()).collect()
}

fn indexed(host: &Host<Ctx>) -> BTreeSet<Vec<u8>> {
    host.view(Layer::Confirmed)
        .scan(LOCAL, &Scan::prefix(b""))
        .unwrap()
        .into_iter()
        .map(|entry| entry.key)
        .filter(|key| key != INDEXED_AT)
        .collect()
}

fn spent_ids(host: &Host<Ctx>) -> BTreeSet<Vec<u8>> {
    rows(host)
        .into_iter()
        .flat_map(|entry| match entry.key.len() {
            8 => entry.value.chunks(32).map(<[u8]>::to_vec).collect(),
            _ => vec![entry.key[8..].to_vec()],
        })
        .collect()
}

fn assert_indexed(host: &Host<Ctx>) {
    assert_eq!(indexed(host), spent_ids(host));
    let height = host.height().unwrap();
    assert_eq!(
        host.view(Layer::Confirmed).get(LOCAL, INDEXED_AT).unwrap(),
        Some(height.to_be_bytes().to_vec())
    );
}

fn refused(host: &Host<Ctx>, submission: &Submission) -> String {
    host.fresh(submission).unwrap().unwrap_err().reason
}

fn replace_local(dir: &Path, height: Option<u64>, ids: &[Vec<u8>]) {
    let storage = Storage::open(&dir.join("state")).unwrap();
    storage.clear_local().unwrap();
    let at = height.map(|height| (INDEXED_AT.to_vec(), Some(height.to_be_bytes().to_vec())));
    let ids = ids.iter().map(|id| (id.clone(), Some(Vec::new())));
    storage.write_local(ids.chain(at)).unwrap();
}

#[test]
fn the_spent_root_below_the_switch_is_the_one_the_layout_by_anchor_gave() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), b"net").await;
        host.set_rows_from(BLOCKS + 1);
        let outcomes = run(&mut host, 1..=BLOCKS).await;
        let expected = [
            (reason::ANCHOR, 14),
            ("applied", 413),
            (reason::DUPLICATE, 68),
            ("probe", 78),
        ];
        let expected: BTreeMap<String, u64> = expected
            .into_iter()
            .map(|(outcome, count)| (outcome.to_owned(), count))
            .collect();
        assert_eq!(outcomes, expected);
        assert_eq!(shapes(&host), BTreeSet::from([40]));
        assert_indexed(&host);
        assert_eq!(root(&host), ROOT, "{outcomes:?}");
    });
}

#[test]
fn from_the_switch_each_block_is_one_row_and_the_rows_by_anchor_expire_as_before() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), b"net").await;
        host.set_rows_from(SWITCH);
        run(&mut host, 1..=SWITCH + 1).await;
        let early = frame(SWITCH - 1, 0);
        assert_eq!(refused(&host, &early), reason::DUPLICATE);
        assert_eq!(shapes(&host), BTreeSet::from([8, 40]));
        assert_indexed(&host);
        run(&mut host, SWITCH + 2..=ACROSS).await;
        assert_eq!(shapes(&host), BTreeSet::from([8]));
        assert_eq!(refused(&host, &early), reason::ANCHOR);
        assert_indexed(&host);
        assert_eq!(root(&host), ROOT_ACROSS);
    });
}

#[test]
fn an_index_at_another_height_is_rebuilt_and_one_at_the_storage_height_is_kept() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), "net", dir.path(), b"net").await;
        host.set_rows_from(SWITCH);
        run(&mut host, 1..=SWITCH + 100).await;
        let before = indexed(&host);
        drop(host);

        replace_local(dir.path(), Some(1), &[vec![7; 32]]);
        let mut host = Host::open(context.child("rebuilt"), "net", dir.path())
            .await
            .unwrap();
        host.set_rows_from(SWITCH);
        assert_eq!(indexed(&host), before);
        assert_indexed(&host);
        assert_eq!(refused(&host, &frame(SWITCH - 1, 0)), reason::DUPLICATE);
        run(&mut host, SWITCH + 101..=ACROSS).await;
        assert_eq!(root(&host), ROOT_ACROSS);
        let height = host.height().unwrap();
        drop(host);

        replace_local(dir.path(), Some(height), &[vec![7; 32]]);
        let host = Host::open(context.child("kept"), "net", dir.path())
            .await
            .unwrap();
        assert_eq!(indexed(&host), BTreeSet::from([vec![7; 32]]));
    });
}

#[test]
fn a_rebuild_cut_before_it_marks_its_height_finishes_at_the_next_open() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), "net", dir.path(), b"net").await;
        host.set_rows_from(SWITCH);
        run(&mut host, 1..=SWITCH + 100).await;
        let before = indexed(&host);
        let root_before = root(&host);
        drop(host);

        let half: Vec<Vec<u8>> = before.iter().step_by(2).cloned().collect();
        let cut: Vec<Vec<u8>> = half.into_iter().chain([vec![7; 32], vec![9; 32]]).collect();
        replace_local(dir.path(), None, &cut);
        let mut host = Host::open(context.child("resumed"), "net", dir.path())
            .await
            .unwrap();
        host.set_rows_from(SWITCH);
        assert_eq!(indexed(&host), before);
        assert_indexed(&host);
        assert_eq!(root(&host), root_before);
        assert_eq!(refused(&host, &frame(SWITCH - 1, 0)), reason::DUPLICATE);
        run(&mut host, SWITCH + 101..=ACROSS).await;
        assert_eq!(root(&host), ROOT_ACROSS);
    });
}

#[test]
fn a_program_sees_none_of_the_index() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), b"net").await;
        run(&mut host, 1..=BLOCKS).await;
        assert!(!indexed(&host).is_empty());
        let carried = indexed(&host).into_iter().next().unwrap();
        let spent_root = host.store().root(SPENT).unwrap();
        assert!(spent_root.is_some());
        let steps = vec![
            Step::Op(HostOp::Set {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }),
            Step::Op(HostOp::Scan(Scan::prefix(b""))),
            Step::Op(HostOp::CommittedScan(Scan::prefix(b""))),
            Step::Op(HostOp::Get(carried)),
            Step::Op(HostOp::Get(INDEXED_AT.to_vec())),
            Step::Op(HostOp::Root(LOCAL.into())),
            Step::Op(HostOp::Root(SPENT.into())),
            Step::Op(HostOp::Query {
                program: LOCAL.into(),
                request: Vec::new(),
            }),
        ];
        let probe = Submission {
            salt: None,
            payload: abi::encode(&steps),
            ..frame(BLOCKS + 1, 0)
        };
        let applied = host.apply(carrying(BLOCKS + 1, vec![probe])).await.unwrap();
        let Outcome::Applied { output } = &applied.submissions[0].outcome else {
            panic!("{:?}", applied.submissions[0]);
        };
        let replies: Vec<Reply> = abi::decode(output).unwrap();
        assert_eq!(
            replies,
            vec![
                Reply::Host(HostReply::Done),
                Reply::Host(HostReply::Entries(vec![Entry {
                    key: b"k".to_vec(),
                    value: b"v".to_vec(),
                }])),
                Reply::Host(HostReply::Entries(Vec::new())),
                Reply::Host(HostReply::Value(None)),
                Reply::Host(HostReply::Value(None)),
                Reply::Host(HostReply::Root(None)),
                Reply::Host(HostReply::Root(spent_root)),
                Reply::Host(HostReply::Query(Err(Refusal::new(
                    reason::UNKNOWN_PROGRAM,
                    LOCAL
                )))),
            ]
        );
        assert!(!state::valid_program_id(LOCAL));
    });
}

#[test]
fn a_network_the_table_names_keeps_rows_by_anchor_until_announced_and_any_other_has_rows_from_genesis()
 {
    deterministic::Runner::default().start(|context| async move {
        for (name, shape) in [("net", 8), ("duckpond", 40), ("sealpond", 40)] {
            let dir = tempfile::tempdir().unwrap();
            let mut host = found(context.child(name), name, dir.path(), name.as_bytes()).await;
            run(&mut host, 1..=8).await;
            assert_eq!(shapes(&host), BTreeSet::from([shape]), "{name}");
            assert_indexed(&host);
        }
    });
}

#[test]
fn a_carried_frame_is_a_duplicate_until_its_anchor_leaves_the_window_in_either_layout() {
    deterministic::Runner::default().start(|context| async move {
        for (name, rows_from) in [("anchored", u64::MAX), ("rows", 0)] {
            let dir = tempfile::tempdir().unwrap();
            let mut host = found(context.child(name), name, dir.path(), b"net").await;
            host.set_rows_from(rows_from);
            let carried = Submission {
                signer: SIGNER.to_vec(),
                anchor: Tip {
                    height: 9,
                    id: block_id(9),
                },
                salt: None,
                target: "probe".to_owned(),
                payload: abi::encode(&Vec::<Step>::new()),
            };
            for height in 1..10 {
                host.apply(carrying(height, Vec::new())).await.unwrap();
            }
            let applied = host
                .apply(carrying(10, vec![carried.clone()]))
                .await
                .unwrap();
            assert!(
                matches!(applied.submissions[0].outcome, Outcome::Applied { .. }),
                "{name}"
            );
            for height in 11..=9 + ANCHOR_WINDOW {
                assert_eq!(
                    refused(&host, &carried),
                    reason::DUPLICATE,
                    "{name} {height}"
                );
                host.apply(carrying(height, Vec::new())).await.unwrap();
            }
            assert_eq!(refused(&host, &carried), reason::ANCHOR, "{name}");
            host.apply(carrying(10 + ANCHOR_WINDOW, Vec::new()))
                .await
                .unwrap();
            assert!(!indexed(&host).contains(carried.id().as_slice()), "{name}");
            assert!(rows(&host).is_empty(), "{name}");
            assert_indexed(&host);
        }
    });
}

fn flood(height: u64, frames: u64) -> Block {
    let anchor = height - 1;
    let submissions = (0..frames)
        .map(|n| Submission {
            signer: SIGNER.to_vec(),
            anchor: Tip {
                height: anchor,
                id: block_id(anchor),
            },
            salt: Some(n),
            target: "probe".to_owned(),
            payload: abi::encode(&Vec::<Step>::new()),
        })
        .collect();
    carrying(height, submissions)
}

fn per_frame(spent: Duration, frames: u64) -> f64 {
    spent.as_secs_f64() * 1e6 / frames as f64
}

#[test]
#[ignore]
fn bench_spent_commit_forget_and_rebuild() {
    let frames: u64 = std::env::var("FRAMES")
        .ok()
        .and_then(|frames| frames.parse().ok())
        .unwrap_or(FLOOD);
    for (layout, rows_from) in [("anchored", u64::MAX), ("rows", 0)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let config = tokio::Config::new().with_storage_directory(path.join("runtime"));
        tokio::Runner::new(config).start(|context| async move {
            let (mut host, _) =
                Host::found(context.child("found"), "net", &path, block_id(0), genesis(b"net"))
                    .await
                    .unwrap();
            host.set_rows_from(rows_from);
            let mut filling = Duration::ZERO;
            for height in 1..=ANCHOR_WINDOW {
                host.apply(flood(height, frames)).await.unwrap();
                filling += host.spent().committing;
            }
            println!(
                "BENCH {layout}: {frames} frames a block, commit {:.2} us/frame over the first {ANCHOR_WINDOW} blocks",
                per_frame(filling, frames * ANCHOR_WINDOW)
            );
            for height in STEADY {
                host.apply(flood(height, frames)).await.unwrap();
                println!(
                    "BENCH {layout}: block {height}, commit with forget {:.2} us/frame",
                    per_frame(host.spent().committing, frames)
                );
            }
            let height = host.height().unwrap();
            drop(host);

            Storage::open(&path.join("state"))
                .unwrap()
                .write_local([(INDEXED_AT.to_vec(), Some(0u64.to_be_bytes().to_vec()))])
                .unwrap();
            let started = Instant::now();
            let host = Host::open(context.child("rebuilt"), "net", &path)
                .await
                .unwrap();
            let rebuilt = started.elapsed();
            let ids = host
                .view(Layer::Confirmed)
                .scan(LOCAL, &Scan::prefix(b""))
                .unwrap()
                .len()
                - 1;
            assert_eq!(host.height().unwrap(), height);
            drop(host);
            let started = Instant::now();
            let host = Host::open(context.child("kept"), "net", &path)
                .await
                .unwrap();
            let kept = started.elapsed();
            drop(host);
            let rebuild = rebuilt.saturating_sub(kept);
            println!(
                "BENCH {layout}: open with a rebuild {rebuilt:.2?}, open with the index kept {kept:.2?}: {ids} ids rebuilt in {rebuild:.2?}, {:.2} us/id",
                rebuild.as_secs_f64() * 1e6 / ids as f64
            );
        });
        drop(dir);
    }
}
