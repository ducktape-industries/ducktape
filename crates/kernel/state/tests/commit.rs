//! A block's commit applies every written program's commitment at once. A crash with several in
//! flight reopens each from the keys the pending row lists, read back from storage, at the clean
//! roots; a pending row of any other shape or height refuses the open and changes nothing.
use std::collections::BTreeMap;
use std::path::Path;

use abi::Root;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use commonware_utils::probability;
use state::{Pending, Slot, Storage, Store, SyncTarget, Writes};

const PROGRAMS: [&str; 4] = ["app", "chat", "files", "harness"];
const QUIET: &str = "quiet";
const BLOCKS: u64 = 24;
const SEEDS: u64 = 8;
const PENDING: &[u8] = b"\xffpending";
const APP: &str = "app";
const ROSTER: &str = "roster";
const GONE: &str = "gone";
const NEW: &str = "new";

type Ctx = deterministic::Context;
type Roots = Vec<(String, Root)>;
type Targets = Vec<(String, Option<SyncTarget>)>;

fn programs() -> Vec<String> {
    let mut programs = PROGRAMS.map(str::to_owned).to_vec();
    programs.push(QUIET.to_owned());
    programs
}

fn large(fill: u64) -> Vec<u8> {
    vec![fill as u8; fluent31::Options::default().value_threshold]
}

fn block(height: u64) -> Writes {
    let keys = |program: &str| {
        let mut keys = (0..8u64)
            .map(|key| {
                let name = format!("{program}/{key}").into_bytes();
                let value = match key {
                    0 => Some(large(height)),
                    _ => (!(key + height).is_multiple_of(5))
                        .then(|| format!("{height}/{key}").into_bytes()),
                };
                (name, value)
            })
            .collect::<BTreeMap<_, _>>();
        keys.insert(format!("{program}/fleeting/{height}").into_bytes(), None);
        keys
    };
    let quiet = (height < BLOCKS).then_some(QUIET);
    Writes {
        programs: PROGRAMS
            .into_iter()
            .chain(quiet)
            .map(|program| (program.to_owned(), keys(program)))
            .collect(),
    }
}

async fn behind(store: &Store<Ctx>, program: &str, height: u64) -> bool {
    match store.commitment(program).unwrap().height().await {
        Ok(committed) => committed != Some(height),
        Err(state::Error::Lost(_)) => true,
        Err(error) => panic!("{program}: {error:?}"),
    }
}

fn targets(store: &Store<Ctx>) -> Targets {
    store
        .commitments()
        .map(|(program, commitment)| (program.clone(), commitment.target().unwrap()))
        .collect()
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
    let mixed = (0..SEEDS).filter(|seed| crash_mid_commit(*seed)).count();
    assert!(
        mixed >= 1,
        "no seed tore a block with one written program at it and another behind"
    );
}

fn crash_mid_commit(seed: u64) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let runner = deterministic::Runner::seeded(seed);
    let ((torn, mixed), checkpoint) = runner.start_and_recover(|context| {
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
            let mut at_block = 0;
            for program in PROGRAMS {
                if !behind(&store, program, height).await {
                    at_block += 1;
                }
            }
            assert!(
                at_block < PROGRAMS.len(),
                "seed {seed}: no commitment was cut"
            );
            (height, at_block >= 1)
        }
    });

    let roots = clean_roots(torn);
    let next = clean_roots(torn + 1);
    deterministic::Runner::from(checkpoint).start(|context| async move {
        *context.storage_fault_config().write() = deterministic::FaultConfig::default();
        let storage = Storage::open(dir.path()).unwrap();
        let store = Store::open(context.child("reopened"), "s", storage, programs())
            .await
            .unwrap();
        assert_eq!(store.height().unwrap(), Some(torn), "seed {seed}");
        assert_eq!(store.roots().unwrap(), roots, "seed {seed}");
        let reconciled = targets(&store);
        drop(store);
        let storage = Storage::open(dir.path()).unwrap();
        let mut store = Store::open(context.child("again"), "s", storage, programs())
            .await
            .unwrap();
        assert_eq!(
            targets(&store),
            reconciled,
            "seed {seed}: a second open applied the block again"
        );
        store.commit(torn + 1, block(torn + 1)).await.unwrap();
        assert_eq!(store.roots().unwrap(), next, "seed {seed}");
    });
    mixed
}

fn running() -> Vec<String> {
    [APP, GONE, ROSTER].map(str::to_owned).to_vec()
}

fn keys(entries: &[(&str, Slot)]) -> BTreeMap<Vec<u8>, Slot> {
    entries
        .iter()
        .map(|(key, slot)| (key.as_bytes().to_vec(), slot.clone()))
        .collect()
}

fn rostered(height: u64) -> Writes {
    let value = format!("{height}").into_bytes();
    let code = || Some(b"code".to_vec());
    Writes {
        programs: BTreeMap::from([
            (APP.to_owned(), keys(&[("a/0", Some(value.clone()))])),
            (GONE.to_owned(), keys(&[("g/0", Some(value))])),
            (
                ROSTER.to_owned(),
                keys(&[(APP, code()), (GONE, code()), (ROSTER, code())]),
            ),
        ]),
    }
}

fn admitting_and_dropping() -> Writes {
    Writes {
        programs: BTreeMap::from([
            (
                APP.to_owned(),
                keys(&[("a/0", Some(large(1))), ("a/1", None)]),
            ),
            (
                GONE.to_owned(),
                keys(&[("g/0", None), ("g/1", Some(b"late".to_vec()))]),
            ),
            (
                NEW.to_owned(),
                keys(&[("n/0", Some(large(2))), ("n/1", Some(b"first".to_vec()))]),
            ),
            (
                ROSTER.to_owned(),
                keys(&[(GONE, None), (NEW, Some(b"code".to_vec()))]),
            ),
        ]),
    }
}

async fn up_to_the_admission(store: &mut Store<Ctx>) {
    for height in 0..BLOCKS {
        store.commit(height, rostered(height)).await.unwrap();
    }
    store.add_program(NEW).await.unwrap();
}

#[test]
fn a_block_torn_after_it_admitted_one_program_and_dropped_another_reopens_at_the_clean_roots() {
    let clean = tempfile::tempdir().unwrap();
    let roots = deterministic::Runner::default().start(|context| async move {
        let storage = Storage::open(clean.path()).unwrap();
        let mut store = Store::open(context.child("clean"), "s", storage, running())
            .await
            .unwrap();
        up_to_the_admission(&mut store).await;
        store
            .commit(BLOCKS, admitting_and_dropping())
            .await
            .unwrap();
        store.remove_program(GONE).await.unwrap();
        store.roots().unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    let (lost, checkpoint) = deterministic::Runner::default().start_and_recover(|context| {
        let dir = &dir;
        async move {
            let storage = Storage::open(dir.path()).unwrap();
            let mut store = Store::open(context.child("store"), "s", storage, running())
                .await
                .unwrap();
            up_to_the_admission(&mut store).await;
            *context.storage_fault_config().write() =
                deterministic::FaultConfig::default().sync(probability!(1.0));
            let error = store.commit(BLOCKS, admitting_and_dropping()).await.err();
            assert!(
                matches!(error, Some(state::Error::Commitment(_))),
                "{error:?}"
            );
            let mut behind_it = 0;
            for program in [APP, GONE, NEW, ROSTER] {
                if behind(&store, program, BLOCKS).await {
                    behind_it += 1;
                }
            }
            behind_it
        }
    });
    assert_eq!(
        lost, 4,
        "every commitment the torn block wrote is behind it"
    );
    deterministic::Runner::from(checkpoint).start(|context| async move {
        *context.storage_fault_config().write() = deterministic::FaultConfig::default();
        let storage = Storage::open(dir.path()).unwrap();
        assert_eq!(
            storage.pending_deleted(ROSTER).unwrap(),
            vec![GONE.as_bytes().to_vec()]
        );
        let after = [APP, NEW, ROSTER].map(str::to_owned).to_vec();
        let mut store = Store::open(context.child("reopened"), "s", storage, after)
            .await
            .unwrap();
        store.remove_program(GONE).await.unwrap();
        assert_eq!(store.height().unwrap(), Some(BLOCKS));
        assert_eq!(store.roots().unwrap(), roots);
        let left = store
            .storage()
            .iter(GONE, b"", None, false)
            .unwrap()
            .count();
        assert_eq!(left, 0, "the dropped program's keys left storage");
    });
}

fn options() -> fluent31::Options {
    fluent31::Options {
        wasm_enabled: false,
        max_key_size: usize::MAX,
        max_value_size: usize::MAX,
        max_txn_write_bytes: usize::MAX,
        ..fluent31::Options::default()
    }
}

fn set_row(dir: &Path, row: Option<Vec<u8>>) {
    let db = fluent31::Db::open(dir, options()).unwrap();
    let mut batch = fluent31::WriteBatch::new();
    match row {
        Some(row) => batch.put(PENDING, row),
        None => batch.delete(PENDING),
    }
    db.write(batch).unwrap();
}

#[test]
fn a_pending_row_of_another_shape_or_height_refuses_the_open_and_changes_nothing() {
    let rows = [
        b"not a pending row".to_vec(),
        abi::encode(&rostered(BLOCKS - 1)),
        abi::encode(&Pending::of(BLOCKS - 2, &rostered(BLOCKS - 2))),
    ];
    for row in rows {
        refused(row);
    }
}

fn refused(row: Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let storage = Storage::open(dir.path()).unwrap();
        let mut store = Store::open(context.child("store"), "s", storage, running())
            .await
            .unwrap();
        for height in 0..BLOCKS {
            store.commit(height, rostered(height)).await.unwrap();
        }
        let before = targets(&store);
        drop(store);
        set_row(dir.path(), Some(row));
        let storage = Storage::open(dir.path()).unwrap();
        let read = storage.pending().err();
        assert!(
            matches!(&read, Some(state::Error::Corrupt(why)) if why.contains("pending row")),
            "{read:?}"
        );
        let refused = Store::open(context.child("refused"), "s", storage, running())
            .await
            .err();
        assert!(
            matches!(refused, Some(state::Error::Corrupt(_))),
            "{refused:?}"
        );
        set_row(dir.path(), None);
        let storage = Storage::open(dir.path()).unwrap();
        assert_eq!(storage.height().unwrap(), Some(BLOCKS - 1));
        let store = Store::open(context.child("after"), "s", storage, running())
            .await
            .unwrap();
        assert_eq!(
            targets(&store),
            before,
            "the refused open changed a commitment"
        );
    });
}
