// A joiner's side of a snapshot: it downloads its source's newest one beside the runtime, moves
// the commitments the snapshot's head names into the runtime's storage in place of what was there,
// and removes them, with the download, when the join refuses them.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use abi::reason;
use statesync::{Head, Place};

use crate::{Client, Error, Result};

const JOINING: &str = "joining";

pub(crate) struct Placing {
    source: Client,
    staging: PathBuf,
    runtime: PathBuf,
    prefixes: Vec<String>,
}

impl Placing {
    pub(crate) fn new(source: Client, workspace: &Path, runtime: PathBuf) -> Placing {
        Placing {
            source,
            staging: workspace.join(JOINING),
            runtime,
            prefixes: Vec::new(),
        }
    }
}

impl Place for Placing {
    type Error = Error;

    async fn place(&mut self, name: &str) -> Result<Option<Head>> {
        let head = match self.source.snapshot(&self.staging).await {
            Ok(head) => head,
            Err(error) => return unserved(error),
        };
        self.prefixes = head
            .targets
            .keys()
            .map(|program| state::partition_prefix(&state::commitment_name(name, program)))
            .collect();
        let staging = self.staging.clone();
        let runtime = self.runtime.clone();
        let prefixes = self.prefixes.clone();
        crate::offload(move || moved(&staging, &runtime, &prefixes)).await??;
        Ok(Some(head))
    }

    async fn discard(&mut self) -> Result<()> {
        let staging = self.staging.clone();
        let runtime = self.runtime.clone();
        let prefixes = self.prefixes.clone();
        crate::offload(move || discarded(&staging, &runtime, &prefixes)).await??;
        Ok(())
    }
}

fn unserved(error: Error) -> Result<Option<Head>> {
    let keeps_none =
        matches!(&error, Error::Refused(refusal) if refusal.reason == reason::NOT_FOUND);
    match keeps_none {
        true => Ok(None),
        false => Err(error),
    }
}

pub(super) fn moved(staging: &Path, runtime: &Path, prefixes: &[String]) -> io::Result<()> {
    let downloaded = staging.join(super::RUNTIME);
    synced(&downloaded)?;
    removed(runtime, prefixes)?;
    std::fs::create_dir_all(runtime)?;
    for entry in std::fs::read_dir(&downloaded)? {
        let entry = entry?;
        let partition = entry.file_name();
        if !committed(&partition, prefixes) {
            continue;
        }
        std::fs::rename(entry.path(), runtime.join(&partition))?;
    }
    File::open(runtime)?.sync_all()?;
    std::fs::remove_dir_all(staging)
}

pub(super) fn discarded(staging: &Path, runtime: &Path, prefixes: &[String]) -> io::Result<()> {
    removed(runtime, prefixes)?;
    match std::fs::remove_dir_all(staging) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

fn removed(runtime: &Path, prefixes: &[String]) -> io::Result<()> {
    let entries = match std::fs::read_dir(runtime) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if committed(&entry.file_name(), prefixes) {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn committed(partition: &OsStr, prefixes: &[String]) -> bool {
    let Some(partition) = partition.to_str() else {
        return false;
    };
    prefixes
        .iter()
        .any(|prefix| partition.starts_with(prefix.as_str()))
}

fn synced(path: &Path) -> io::Result<()> {
    let directory = std::fs::symlink_metadata(path)?.is_dir();
    if directory {
        for entry in std::fs::read_dir(path)? {
            synced(&entry?.path())?;
        }
    }
    File::open(path)?.sync_all()
}
