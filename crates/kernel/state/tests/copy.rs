//! A commitment copied file by file between two of its store's commits opens at the root and height
//! it had at the copy, while the store it came from goes on committing: a store adopts the copy.
use std::collections::BTreeMap;
use std::path::Path;

use commonware_runtime::{Runner as _, Supervisor as _, tokio};
use state::{Commitment, Storage, Store, Writes, commitment_name};

const PROGRAMS: u64 = 4;
const BLOCKS: u64 = 48;

fn programs() -> Vec<String> {
    (0..PROGRAMS)
        .map(|index| format!("program{index}"))
        .collect()
}

fn block(height: u64) -> Writes {
    let keys = |index: u64| {
        (0..128 * (index + 1))
            .map(|key| {
                let name = format!("{index}/{key}").into_bytes();
                let deleted = (key + height).is_multiple_of(7);
                let value = format!("{height}/{key}").into_bytes();
                (name, (!deleted).then_some(value))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Writes {
        programs: programs()
            .into_iter()
            .zip(0..)
            .map(|(program, index)| (program, keys(index)))
            .collect(),
    }
}

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir.join("runtime")))
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn a_commitment_copied_between_two_commits_opens_at_the_root_it_had_at_the_copy() {
    let live = tempfile::tempdir().unwrap();
    let copied = tempfile::tempdir().unwrap();
    let (live_dir, copy_dir) = (live.path().to_owned(), copied.path().to_owned());
    let (at_copy, after) = runner(&live_dir).start(|context| async move {
        let storage = Storage::open(&live_dir.join("state")).unwrap();
        let mut store = Store::open(context.child("store"), "s", storage, programs())
            .await
            .unwrap();
        for height in 0..BLOCKS {
            store.commit(height, block(height)).await.unwrap();
        }
        let at_copy = BTreeMap::from_iter(store.roots().unwrap());
        copy(&live_dir.join("runtime"), &copy_dir.join("runtime"));
        for height in BLOCKS..2 * BLOCKS {
            store.commit(height, block(height)).await.unwrap();
        }
        store.sync().await.unwrap();
        (at_copy, BTreeMap::from_iter(store.roots().unwrap()))
    });
    let copy_dir = copied.path().to_owned();
    let (opened, heights, adopted, value) = runner(&copy_dir).start(|context| async move {
        let mut commitments = BTreeMap::new();
        let mut heights = BTreeMap::new();
        for program in programs() {
            let context = context.child("copy").with_attribute("program", &program);
            let commitment = Commitment::open(context, &commitment_name("s", &program), 0)
                .await
                .unwrap();
            heights.insert(program.clone(), commitment.height().await.unwrap());
            commitments.insert(program, commitment);
        }
        let opened = commitments
            .iter()
            .map(|(program, commitment)| (program.clone(), commitment.root().unwrap()))
            .collect::<BTreeMap<_, _>>();
        let storage = Storage::open(&copy_dir.join("state")).unwrap();
        let store = Store::adopt(
            context.child("adopted"),
            "s",
            storage,
            BLOCKS - 1,
            commitments,
        )
        .await
        .unwrap();
        let value = store.storage().get("program0", b"0/1").unwrap();
        let adopted = (
            store.height().unwrap(),
            BTreeMap::from_iter(store.roots().unwrap()),
        );
        (opened, heights, adopted, value)
    });
    assert_ne!(at_copy, after);
    assert_eq!(opened, at_copy);
    assert!(heights.values().all(|height| *height == Some(BLOCKS - 1)));
    assert_eq!(adopted, (Some(BLOCKS - 1), at_copy));
    assert_eq!(value, Some(format!("{}/1", BLOCKS - 1).into_bytes()));
}
