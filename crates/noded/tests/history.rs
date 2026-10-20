// Times the one copy of a node's block history out of the archives older builds kept, and the copy
// back a rollback makes, on a copy of a node's runtime directory: RUNTIME names it, PARTITION the
// network and HISTORY an empty directory for the history store.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use commonware_runtime::{Runner as _, Supervisor as _, tokio};

const KINDS: [&str; 5] = ["blocks-z", "blocks", "summaries", "txs", "certificates"];

fn bytes_under(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| bytes_under(&entry.path()))
        .sum()
}

fn kind_of<'a>(name: &str, partition: &str) -> Option<&'a str> {
    KINDS
        .into_iter()
        .find(|kind| name.starts_with(&format!("{partition}-{kind}-")))
}

fn report(runtime: &Path, partition: &str, history: &Path) -> BTreeMap<String, u64> {
    let mut bytes: BTreeMap<String, u64> = BTreeMap::new();
    for entry in std::fs::read_dir(runtime).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(kind) = kind_of(&name, partition) {
            *bytes.entry(kind.to_owned()).or_default() += bytes_under(&entry.path());
        }
    }
    bytes.insert("history".into(), bytes_under(history));
    bytes
}

fn remove_plain(runtime: &Path, partition: &str) {
    for entry in std::fs::read_dir(runtime).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let plain = matches!(
            kind_of(&name, partition),
            Some("blocks" | "summaries" | "txs")
        );
        if plain {
            std::fs::remove_dir_all(entry.path()).unwrap();
        }
    }
}

#[test]
#[ignore]
fn a_nodes_history_copies_out_of_its_archives_and_back() {
    let runtime =
        PathBuf::from(std::env::var("RUNTIME").expect("RUNTIME names a runtime directory"));
    let partition = std::env::var("PARTITION").expect("PARTITION names the network");
    let history = PathBuf::from(std::env::var("HISTORY").expect("HISTORY names a directory"));
    let config = tokio::Config::new().with_storage_directory(runtime.clone());
    tokio::Runner::new(config).start(|context| async move {
        eprintln!("before: {:?}", report(&runtime, &partition, &history));
        let started = Instant::now();
        let first = noded::carry_history(context.child("first"), &partition, &history)
            .await
            .unwrap();
        eprintln!(
            "the first start copied {} summaries, left {} blocks unsummarized, in {:?}",
            first.copied,
            first.unsummarized.len(),
            started.elapsed()
        );
        eprintln!("after: {:?}", report(&runtime, &partition, &history));
        let started = Instant::now();
        let again = noded::carry_history(context.child("again"), &partition, &history)
            .await
            .unwrap();
        eprintln!(
            "the next start copied {} in {:?}",
            again.copied,
            started.elapsed()
        );
        assert_eq!(again.copied, 0);
        remove_plain(&runtime, &partition);
        let started = Instant::now();
        let (blocks, summaries) =
            noded::restore_history(context.child("back"), &partition, &history)
                .await
                .unwrap();
        eprintln!(
            "restore-archives wrote back {blocks} blocks and {summaries} summaries in {:?}",
            started.elapsed()
        );
        assert_eq!(summaries, first.copied);
        let forward = noded::carry_history(context.child("forward"), &partition, &history)
            .await
            .unwrap();
        assert_eq!(forward.copied, 0);
    });
}
