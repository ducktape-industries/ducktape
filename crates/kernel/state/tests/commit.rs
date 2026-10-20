//! A block's commit applies every written program's commitment at once: a crash with several of
//! them in flight reopens with each at the block's root, and the next block commits on top.
use std::collections::BTreeMap;

use abi::Root;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use commonware_utils::probability;
use state::{Storage, Store, Writes};

const PROGRAMS: [&str; 4] = ["app", "chat", "files", "harness"];
const BLOCKS: u64 = 24;
const SEEDS: u64 = 8;

type Roots = Vec<(String, Root)>;

fn programs() -> Vec<String> {
    PROGRAMS.map(str::to_owned).to_vec()
}

fn block(height: u64) -> Writes {
    let keys = |program: &str| {
        (0..8u64)
            .map(|key| {
                let name = format!("{program}/{key}").into_bytes();
                let deleted = (key + height).is_multiple_of(5);
                let value = format!("{height}/{key}").into_bytes();
                (name, (!deleted).then_some(value))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Writes {
        programs: PROGRAMS
            .iter()
            .map(|program| (program.to_string(), keys(program)))
            .collect(),
    }
}

fn clean_roots(through: u64) -> Roots {
    let dir = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let storage = Storage::open(dir.path()).unwrap();
        let mut store = Store::open(context.child("clean"), "s", storage, programs())
            .await
            .unwrap();
        for height in 0..=through {
            store.commit(height, block(height)).await.unwrap();
        }
        store.roots().unwrap()
    })
}

#[test]
fn a_crash_with_several_commitments_in_flight_reopens_each_at_the_block() {
    for seed in 0..SEEDS {
        crash_mid_commit(seed);
    }
}

fn crash_mid_commit(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let runner = deterministic::Runner::seeded(seed);
    let (torn, checkpoint) = runner.start_and_recover(|context| {
        let dir = &dir;
        async move {
            let storage = Storage::open(dir.path()).unwrap();
            let mut store = Store::open(context.child("store"), "s", storage, programs())
                .await
                .unwrap();
            for height in 0..BLOCKS {
                store.commit(height, block(height)).await.unwrap();
            }
            *context.storage_fault_config().write() =
                deterministic::FaultConfig::default().sync(probability!(0.5));
            let mut height = BLOCKS;
            let error = loop {
                match store.commit(height, block(height)).await {
                    Ok(()) => height += 1,
                    Err(error) => break error,
                }
            };
            assert!(matches!(error, state::Error::Commitment(_)), "{error:?}");
            let lost = PROGRAMS
                .iter()
                .filter(|program| matches!(store.root(program), Err(state::Error::Lost(_))))
                .count();
            assert!(lost >= 1, "seed {seed}: no commitment was cut");
            height
        }
    });

    let roots = clean_roots(torn);
    let next = clean_roots(torn + 1);
    deterministic::Runner::from(checkpoint).start(|context| async move {
        *context.storage_fault_config().write() = deterministic::FaultConfig::default();
        let storage = Storage::open(dir.path()).unwrap();
        let mut store = Store::open(context.child("reopened"), "s", storage, programs())
            .await
            .unwrap();
        assert_eq!(store.height().unwrap(), Some(torn), "seed {seed}");
        assert_eq!(store.roots().unwrap(), roots, "seed {seed}");
        store.commit(torn + 1, block(torn + 1)).await.unwrap();
        assert_eq!(store.roots().unwrap(), next, "seed {seed}");
    });
}
