use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ::tokio::sync::watch;
use bytes::Bytes;
use commonware_runtime::{Runner as _, Supervisor as _, tokio};
use futures::{Stream, StreamExt as _};
use host::Tip;
use state::{Commitment, Storage, Store, Writes, commitment_name};
use statesync::Manifest;

use super::{
    DIR, Kept, MANIFEST, RUNTIME, Reflink, Refused, Removals, Snapshots, fetched, resumed, served,
    swept,
};
use crate::wire::snapshot::{Ask, Part, Resume};

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
        local: Default::default(),
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
fn a_sweep_leaves_what_is_served_or_read_and_any_newer_snapshot_being_written() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["3", ".5", "7", "9", ".9", ".12", "stray"] {
        std::fs::create_dir_all(temp.path().join(name).join(RUNTIME)).unwrap();
    }
    swept(temp.path(), &BTreeSet::from([3, 9]), false);
    assert_eq!(listed(temp.path()), [".12", "3", "9"]);
    swept(temp.path(), &BTreeSet::from([9]), true);
    assert_eq!(listed(temp.path()), ["9"]);
    swept(&temp.path().join("absent"), &BTreeSet::new(), true);
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
    assert!(kept.removals.served().load_full().is_none());
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

const FILES: [(&str, &[u8]); 4] = [
    ("a-log/0000", b"the first section of a's log"),
    ("a-log/0001", b"a's second"),
    ("a-meta/0", b""),
    (
        "b-log/0000",
        b"b's only section, a little longer than the others",
    ),
];

fn written_snapshot(dir: &Path, height: u64) {
    let snapshot = dir.join(height.to_string());
    for (path, bytes) in FILES {
        let path = snapshot.join(RUNTIME).join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    std::fs::write(
        snapshot.join(MANIFEST),
        abi::encode(&empty_manifest(height)),
    )
    .unwrap();
}

fn files() -> BTreeMap<String, Vec<u8>> {
    FILES
        .iter()
        .map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec()))
        .collect()
}

fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(at) = dirs.pop() {
        for entry in std::fs::read_dir(&at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let name = path.strip_prefix(dir).unwrap().to_str().unwrap().to_owned();
            files.insert(name, std::fs::read(&path).unwrap());
        }
    }
    files
}

fn framed(bytes: &[u8]) -> Vec<u8> {
    let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(bytes);
    framed
}

async fn answered(kept: Arc<Kept>, ask: Ask) -> Vec<u8> {
    let mut answer = Box::pin(served(kept, ask));
    let mut bytes = Vec::new();
    while let Some(piece) = answer.next().await {
        bytes.extend_from_slice(&piece.unwrap());
    }
    bytes
}

fn pieces(bytes: &[u8], each: usize) -> impl Stream<Item = crate::Result<Bytes>> + use<> {
    let pieces: Vec<crate::Result<Bytes>> = bytes
        .chunks(each.max(1))
        .map(|piece| Ok(Bytes::copy_from_slice(piece)))
        .collect();
    futures::stream::iter(pieces)
}

#[::tokio::test(crate = "::tokio")]
async fn a_download_cut_at_any_byte_resumes_to_the_snapshot_byte_for_byte() {
    let temp = tempfile::tempdir().unwrap();
    let snapshots_dir = temp.path().join(DIR);
    written_snapshot(&snapshots_dir, 7);
    let removals = Removals::start(snapshots_dir.clone()).unwrap();
    let kept = removals.served().load_full().unwrap();
    let whole = answered(kept.clone(), Ask::default()).await;
    for cut in 0..whole.len() {
        let into = temp.path().join(format!("into-{cut}"));
        let short = fetched(pieces(&whole[..cut], 1), &into).await;
        assert!(
            short.is_err(),
            "an answer cut at {cut} of {} reads whole",
            whole.len()
        );
        let resume = resumed(&into).await.unwrap();
        let rest = answered(kept.clone(), Ask { resume }).await;
        assert!(rest.len() <= whole.len(), "a resume at {cut} asks for more");
        let manifest = fetched(pieces(&rest, rest.len()), &into).await.unwrap();
        assert_eq!(manifest, empty_manifest(7), "cut at {cut}");
        assert_eq!(tree(&into.join(RUNTIME)), files(), "cut at {cut}");
    }
    let into = temp.path().join("whole");
    let manifest = fetched(pieces(&whole, 3), &into).await.unwrap();
    assert_eq!(manifest, empty_manifest(7));
    assert_eq!(tree(&into.join(RUNTIME)), files());
    let written: Manifest = abi::decode(&std::fs::read(into.join(MANIFEST)).unwrap()).unwrap();
    assert_eq!(written, empty_manifest(7));
}

#[::tokio::test(crate = "::tokio")]
async fn a_download_that_holds_every_file_is_sent_no_byte_of_them_again() {
    let temp = tempfile::tempdir().unwrap();
    let snapshots_dir = temp.path().join(DIR);
    written_snapshot(&snapshots_dir, 7);
    let removals = Removals::start(snapshots_dir.clone()).unwrap();
    let kept = removals.served().load_full().unwrap();
    let whole = answered(kept.clone(), Ask::default()).await;
    let into = temp.path().join("into");
    let short = fetched(pieces(&whole[..whole.len() - 4], 5), &into).await;
    assert!(short.is_err());
    let (last, bytes) = FILES[3];
    let len = bytes.len() as u64;
    let resume = resumed(&into).await.unwrap();
    let expected = Resume {
        height: 7,
        path: last.to_owned(),
        offset: len,
    };
    assert_eq!(resume, Some(expected));
    let rest = answered(kept, Ask { resume }).await;
    let header = Part {
        path: last.to_owned(),
        len,
        from: len,
    };
    let only_the_ends = [
        framed(&abi::encode(&empty_manifest(7))),
        framed(&abi::encode(&header)),
        vec![0; 4],
    ]
    .concat();
    assert_eq!(rest, only_the_ends);
}

#[::tokio::test(crate = "::tokio")]
async fn a_snapshot_a_download_reads_stays_until_its_answer_ends_and_goes_then() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    partition_with_a_file(&runtime);
    let snapshots_dir = temp.path().join(DIR);
    written_snapshot(&snapshots_dir, 7);
    let mut snapshots = snapshots(&snapshots_dir, &runtime, copied);
    let kept = snapshots.removals.served().load_full().unwrap();
    let mut answer = Box::pin(served(kept, Ask::default()));
    let head = answer.next().await.unwrap().unwrap();
    snapshots.keep(15, &prefixes(), &empty_manifest(15));
    snapshots.removals.settled();
    assert_eq!(listed(&snapshots_dir), ["15", "7"]);
    let serving = snapshots.removals.served().load_full().unwrap();
    assert_eq!(serving.height, 15);
    drop(serving);
    let into = temp.path().join("into");
    let read = futures::stream::once(async { Ok(head) })
        .chain(answer.map(|piece| piece.map_err(crate::Error::from)));
    let manifest = fetched(read, &into).await.unwrap();
    assert_eq!(manifest, empty_manifest(7));
    assert_eq!(tree(&into.join(RUNTIME)), files());
    snapshots.removals.settled();
    assert_eq!(listed(&snapshots_dir), ["15"]);
}

#[::tokio::test(crate = "::tokio")]
async fn a_download_resumed_after_a_newer_snapshot_took_over_holds_the_newer_one_alone() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    partition_with_a_file(&runtime);
    let snapshots_dir = temp.path().join(DIR);
    written_snapshot(&snapshots_dir, 7);
    let mut snapshots = snapshots(&snapshots_dir, &runtime, copied);
    let kept = snapshots.removals.served().load_full().unwrap();
    let whole = answered(kept, Ask::default()).await;
    let into = temp.path().join("into");
    let short = fetched(pieces(&whole[..whole.len() / 2], 7), &into).await;
    assert!(short.is_err());
    snapshots.keep(15, &prefixes(), &empty_manifest(15));
    let resume = resumed(&into).await.unwrap();
    assert_eq!(resume.as_ref().map(|resume| resume.height), Some(7));
    let newer = snapshots.removals.served().load_full().unwrap();
    let rest = answered(newer, Ask { resume }).await;
    let manifest = fetched(pieces(&rest, rest.len()), &into).await.unwrap();
    assert_eq!(manifest, empty_manifest(15));
    let held = BTreeMap::from([(format!("{}/0000", partition()), b"ops".to_vec())]);
    assert_eq!(tree(&into.join(RUNTIME)), held);
}

#[::tokio::test(crate = "::tokio")]
async fn a_stalled_download_is_cut_once_two_newer_snapshots_land_and_its_snapshot_goes() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    partition_with_a_file(&runtime);
    let snapshots_dir = temp.path().join(DIR);
    written_snapshot(&snapshots_dir, 7);
    let big = snapshots_dir.join("7").join(RUNTIME).join("a-big");
    std::fs::write(&big, b"").unwrap();
    let unit = {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(&big).unwrap().blksize() as usize
    };
    std::fs::write(&big, vec![7u8; 2 * unit + 1]).unwrap();
    let mut snapshots = snapshots(&snapshots_dir, &runtime, copied);
    let kept = snapshots.removals.served().load_full().unwrap();
    let mut stalled = Box::pin(served(kept, Ask::default()));
    let mut read = Vec::new();
    for _ in 0..3 {
        read.push(stalled.next().await.unwrap().unwrap());
    }
    assert!(
        read[2].len() < 2 * unit + 1,
        "the download stalls inside a-big: {} of {} bytes",
        read[2].len(),
        2 * unit + 1
    );
    snapshots.keep(15, &prefixes(), &empty_manifest(15));
    snapshots.removals.settled();
    assert_eq!(
        listed(&snapshots_dir),
        ["15", "7"],
        "the one before the newest stays"
    );
    snapshots.keep(23, &prefixes(), &empty_manifest(23));
    snapshots.removals.settled();
    assert_eq!(
        listed(&snapshots_dir),
        ["23"],
        "a stalled download holds nothing past that"
    );
    let rest = stalled.next().await.unwrap();
    let error = match rest {
        Err(error) => error,
        Ok(bytes) => panic!(
            "the stalled answer goes on with its open file after it was cut: {} bytes",
            bytes.len()
        ),
    };
    assert_eq!(error.to_string(), super::stream::replaced().to_string());
    assert!(stalled.next().await.is_none(), "the cut answer ends");
    let into = temp.path().join("into");
    let held =
        futures::stream::iter(read.into_iter().map(Ok)).chain(futures::stream::once(async {
            Err(crate::Error::from(error))
        }));
    assert!(fetched(held, &into).await.is_err());
    let resume = resumed(&into).await.unwrap();
    assert_eq!(
        resume.as_ref().map(|resume| resume.path.as_str()),
        Some("a-big")
    );
    let newest = snapshots.removals.served().load_full().unwrap();
    let whole = answered(newest, Ask { resume }).await;
    let manifest = fetched(pieces(&whole, whole.len()), &into).await.unwrap();
    assert_eq!(manifest, empty_manifest(23));
}

#[::tokio::test(crate = "::tokio")]
async fn a_download_refuses_a_header_longer_than_a_node_message_before_buffering_it() {
    let temp = tempfile::tempdir().unwrap();
    let past = u32::try_from(node::BLOCK_BYTES + 1).unwrap().to_le_bytes();
    let into = temp.path().join("into");
    let refused = fetched(pieces(&past, past.len()), &into).await;
    assert!(
        matches!(refused, Err(crate::Error::Decode(_))),
        "{refused:?}"
    );
    let header = [framed(&abi::encode(&empty_manifest(7))), past.to_vec()].concat();
    let refused = fetched(pieces(&header, header.len()), &into).await;
    assert!(
        matches!(refused, Err(crate::Error::Decode(_))),
        "{refused:?}"
    );
}

#[::tokio::test(crate = "::tokio")]
async fn a_download_refuses_a_file_named_outside_its_directory() {
    let temp = tempfile::tempdir().unwrap();
    for path in ["../escaped", "/escaped", ""] {
        let header = Part {
            path: path.to_owned(),
            len: 1,
            from: 0,
        };
        let answer = [
            framed(&abi::encode(&empty_manifest(7))),
            framed(&abi::encode(&header)),
            b"x".to_vec(),
            vec![0; 4],
        ]
        .concat();
        let into = temp.path().join("into");
        let refused = fetched(pieces(&answer, answer.len()), &into).await;
        assert!(
            matches!(refused, Err(crate::Error::Decode(_))),
            "{path:?}: {refused:?}"
        );
        assert!(!temp.path().join("escaped").exists(), "{path:?}");
    }
}

#[test]
fn a_restarted_node_serves_its_newest_snapshot_whose_manifest_reads_as_its_own_and_removes_the_rest()
 {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(DIR);
    for height in [5, 9, 12, 14] {
        written_snapshot(&dir, height);
    }
    std::fs::write(dir.join("12").join(MANIFEST), b"torn").unwrap();
    std::fs::write(
        dir.join("14").join(MANIFEST),
        abi::encode(&empty_manifest(13)),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".15")).unwrap();
    std::fs::create_dir_all(dir.join("stray")).unwrap();
    let removals = Removals::start(dir.clone()).unwrap();
    let kept = removals.served().load_full().unwrap();
    assert_eq!(kept.height, 9);
    assert_eq!(kept.manifest, empty_manifest(9));
    assert_eq!(listed(&dir), ["9"]);
}
