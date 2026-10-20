use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use ::tokio::sync::watch;
use commonware_runtime::{Runner as _, Supervisor as _, tokio};
use host::Tip;
use state::{Commitment, Storage, Store, Writes, commitment_name};
use statesync::Manifest;

use super::{DIR, MANIFEST, RUNTIME, Reflink, Refused, Removals, Snapshots, older_removed};

const PROGRAMS: [&str; 3] = ["a", "b", "c"];

fn programs() -> Vec<String> {
    PROGRAMS
        .iter()
        .map(|program| (*program).to_owned())
        .collect()
}

fn prefixes() -> Vec<String> {
    PROGRAMS
        .iter()
        .map(|program| state::partition_prefix(&commitment_name("s", program)))
        .collect()
}

fn block(height: u64) -> Writes {
    let keys = || {
        (0..16u64)
            .map(|key| {
                let deleted = (key + height).is_multiple_of(5);
                let value = format!("{height}/{key}").into_bytes();
                (key.to_string().into_bytes(), (!deleted).then_some(value))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Writes {
        programs: programs()
            .into_iter()
            .map(|program| (program, keys()))
            .collect(),
    }
}

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir))
}

const EPOCH: u64 = 8;

fn following(
    dir: &Path,
    runtime: &Path,
    reflink: Reflink,
    finalized: watch::Receiver<u64>,
) -> Snapshots {
    let removals = Removals::start(dir.to_owned()).unwrap();
    Snapshots::new(
        dir.to_owned(),
        runtime.to_owned(),
        reflink,
        removals,
        finalized,
        EPOCH,
    )
}

fn snapshots(dir: &Path, runtime: &Path, reflink: Reflink) -> Snapshots {
    following(dir, runtime, reflink, watch::channel(0).1)
}

fn copied(copy: &File, source: &File) -> Result<(), Refused> {
    let (mut copy, mut source) = (copy, source);
    io::copy(&mut source, &mut copy)?;
    Ok(())
}

fn listed(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn manifest_of(store: &Store<tokio::Context>, height: u64) -> Manifest {
    Manifest {
        tip: Tip {
            height,
            id: [7; 32],
        },
        targets: store
            .commitments()
            .map(|(program, commitment)| (program.clone(), commitment.target().unwrap().unwrap()))
            .collect(),
    }
}

#[test]
fn a_snapshot_opens_at_the_targets_its_manifest_names_and_only_the_newest_is_kept() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_owned();
    let snapshots_dir = dir.join(DIR);
    let runtime = dir.join("runtime");
    let kept = runner(&runtime).start({
        let (dir, snapshots_dir, runtime) = (dir.clone(), snapshots_dir.clone(), runtime.clone());
        |context| async move {
            let storage = Storage::open(&dir.join("state")).unwrap();
            let mut store = Store::open(context.child("store"), "s", storage, programs())
                .await
                .unwrap();
            let mut snapshots = snapshots(&snapshots_dir, &runtime, copied);
            let mut kept = Vec::new();
            for height in 0..16 {
                store.commit(height, block(height)).await.unwrap();
                let closes = height % 8 == 7;
                if closes {
                    let manifest = manifest_of(&store, height);
                    snapshots.keep(height, &prefixes(), &manifest);
                    kept.push(manifest);
                }
            }
            kept
        }
    });
    assert_eq!(listed(&snapshots_dir), ["15"]);
    let newest = snapshots_dir.join("15");
    let written: Manifest = abi::decode(&std::fs::read(newest.join(MANIFEST)).unwrap()).unwrap();
    assert_eq!(written, kept[1]);
    assert_ne!(kept[0].targets, kept[1].targets);
    let opened = runner(&newest.join(RUNTIME)).start(|context| async move {
        let mut opened = BTreeMap::new();
        for program in programs() {
            let context = context
                .child("snapshot")
                .with_attribute("program", &program);
            let commitment = Commitment::open(context, &commitment_name("s", &program), 0)
                .await
                .unwrap();
            opened.insert(program, commitment.target().unwrap().unwrap());
        }
        opened
    });
    assert_eq!(opened, written.targets);
}

#[test]
fn the_removal_of_older_snapshots_leaves_the_kept_one_and_any_newer_one_being_written() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["3", ".5", "9", ".12", "stray"] {
        std::fs::create_dir_all(temp.path().join(name).join(RUNTIME)).unwrap();
    }
    older_removed(temp.path(), 9);
    assert_eq!(listed(temp.path()), [".12", "9"]);
    older_removed(&temp.path().join("absent"), 9);
}

#[cfg(target_os = "linux")]
#[test]
fn the_real_clone_copies_the_bytes_or_is_refused_as_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(1 << 16).collect();
    std::fs::write(temp.path().join("source"), &bytes).unwrap();
    let source = File::open(temp.path().join("source")).unwrap();
    let copy = File::create_new(temp.path().join("copy")).unwrap();
    match super::reflink(&copy, &source) {
        Ok(()) => assert_eq!(std::fs::read(temp.path().join("copy")).unwrap(), bytes),
        Err(Refused::Unsupported(error)) => {
            println!("{} does not clone: {error}", temp.path().display())
        }
        Err(Refused::Failed(error)) => panic!("the clone failed: {error}"),
    }
}

static CLONED: AtomicUsize = AtomicUsize::new(0);

fn counted(copy: &File, source: &File) -> Result<(), Refused> {
    CLONED.fetch_add(1, Ordering::SeqCst);
    copied(copy, source)
}

#[test]
fn a_follower_replaying_finalized_epochs_clones_nothing_until_it_reaches_the_newest_one() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    partition_with_a_file(&runtime);
    let snapshots_dir = temp.path().join(DIR);
    let (finalized, known) = watch::channel(8 * EPOCH + 3);
    let mut kept = following(&snapshots_dir, &runtime, counted, known);
    let boundaries = (1..=8).map(|epoch| epoch * EPOCH - 1);
    let mut cloned = Vec::new();
    for boundary in boundaries {
        kept.keep(boundary, &prefixes(), &empty_manifest(boundary));
        cloned.push(CLONED.load(Ordering::SeqCst));
    }
    assert_eq!(cloned, [0, 0, 0, 0, 0, 0, 0, 1]);
    finalized.send_replace(9 * EPOCH);
    kept.keep(9 * EPOCH - 1, &prefixes(), &empty_manifest(9 * EPOCH - 1));
    assert_eq!(CLONED.load(Ordering::SeqCst), 2);
    drop(kept);
    assert_eq!(listed(&snapshots_dir), [(9 * EPOCH - 1).to_string()]);
}

fn partition() -> String {
    format!("{}log", prefixes()[0])
}

fn partition_with_a_file(runtime: &Path) {
    let partition = runtime.join(partition());
    std::fs::create_dir_all(runtime.join("elsewhere")).unwrap();
    std::fs::create_dir_all(&partition).unwrap();
    std::fs::write(partition.join("0000"), b"ops").unwrap();
}

fn empty_manifest(height: u64) -> Manifest {
    Manifest {
        tip: Tip {
            height,
            id: [height as u8; 32],
        },
        targets: BTreeMap::new(),
    }
}

static UNSUPPORTED: AtomicUsize = AtomicUsize::new(0);

fn unsupported(_: &File, _: &File) -> Result<(), Refused> {
    UNSUPPORTED.fetch_add(1, Ordering::SeqCst);
    Err(Refused::Unsupported(io::Error::from(
        io::ErrorKind::Unsupported,
    )))
}

fn failing(_: &File, _: &File) -> Result<(), Refused> {
    Err(Refused::Failed(io::Error::from(io::ErrorKind::StorageFull)))
}

#[test]
fn a_node_whose_filesystem_does_not_reflink_keeps_no_snapshot_and_asks_once() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    partition_with_a_file(&runtime);
    let snapshots_dir = temp.path().join(DIR);
    let mut kept = snapshots(&snapshots_dir, &runtime, unsupported);
    kept.keep(7, &prefixes(), &empty_manifest(7));
    kept.keep(15, &prefixes(), &empty_manifest(15));
    drop(kept);
    assert_eq!(UNSUPPORTED.load(Ordering::SeqCst), 1);
    assert!(listed(&snapshots_dir).is_empty());
}

#[test]
fn a_snapshot_that_fails_leaves_the_one_before_it_whole() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    partition_with_a_file(&runtime);
    let snapshots_dir = temp.path().join(DIR);
    snapshots(&snapshots_dir, &runtime, copied).keep(7, &prefixes(), &empty_manifest(7));
    snapshots(&snapshots_dir, &runtime, failing).keep(15, &prefixes(), &empty_manifest(15));
    let before = snapshots_dir.join("7");
    let written: Manifest = abi::decode(&std::fs::read(before.join(MANIFEST)).unwrap()).unwrap();
    assert_eq!(written, empty_manifest(7));
    let file = before.join(RUNTIME).join(partition()).join("0000");
    assert_eq!(std::fs::read(file).unwrap(), b"ops");
    assert!(!before.join(RUNTIME).join("elsewhere").exists());
    assert!(listed(&snapshots_dir).contains(&"7".to_owned()));
}
