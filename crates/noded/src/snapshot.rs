// A follower whose filesystem reflinks keeps its newest whole snapshot: at each epoch's last block,
// unless a later epoch's last block is already finalized, the writer clones every commitment file
// beside a manifest; a thread of its own removes older snapshots, and a stop waits for it.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use node::Node;
use statesync::Manifest;
use tokio::sync::watch;

use crate::Context;

pub(crate) const DIR: &str = "snapshots";
pub(crate) const MANIFEST: &str = "manifest";
pub(crate) const RUNTIME: &str = "runtime";

pub(crate) enum Refused {
    Unsupported(io::Error),
    Failed(io::Error),
}

impl From<io::Error> for Refused {
    fn from(error: io::Error) -> Refused {
        Refused::Failed(error)
    }
}

pub(crate) type Reflink = fn(&File, &File) -> Result<(), Refused>;

pub(crate) struct Removals {
    below: Option<mpsc::Sender<u64>>,
    thread: Option<JoinHandle<()>>,
}

impl Removals {
    pub(crate) fn start(dir: PathBuf) -> io::Result<Removals> {
        let (below, heard) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("snapshots".into())
            .spawn(move || removing(&dir, heard))?;
        Ok(Removals {
            below: Some(below),
            thread: Some(thread),
        })
    }

    fn below(&self, height: u64) {
        let sent = self.below.as_ref().map(|below| below.send(height));
        if let Some(Err(error)) = sent {
            unremoved(Path::new(DIR), &io::Error::other(error.to_string()));
        }
    }
}

impl Drop for Removals {
    fn drop(&mut self) {
        drop(self.below.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct Snapshots {
    dir: PathBuf,
    runtime: PathBuf,
    reflink: Reflink,
    removals: Removals,
    finalized: watch::Receiver<u64>,
    epoch_length: u64,
    able: bool,
}

impl Snapshots {
    pub(crate) fn new(
        dir: PathBuf,
        runtime: PathBuf,
        reflink: Reflink,
        removals: Removals,
        finalized: watch::Receiver<u64>,
        epoch_length: u64,
    ) -> Snapshots {
        Snapshots {
            dir,
            runtime,
            reflink,
            removals,
            finalized,
            epoch_length,
            able: true,
        }
    }

    pub(crate) fn take<E: Context>(&mut self, node: &Node<E>, height: u64) {
        if !self.due(height) {
            return;
        }
        let manifest = match statesync::manifest(node) {
            Ok(manifest) => manifest,
            Err(refusal) => return failed(height, &refusal.sentence),
        };
        let store = node.host().store();
        let prefixes: Vec<String> = store
            .programs()
            .map(|program| state::partition_prefix(&state::commitment_name(store.name(), program)))
            .collect();
        self.keep(height, &prefixes, &manifest);
    }

    fn due(&self, height: u64) -> bool {
        let next = height.saturating_add(self.epoch_length);
        self.able && *self.finalized.borrow() < next
    }

    pub(crate) fn keep(&mut self, height: u64, prefixes: &[String], manifest: &Manifest) {
        if !self.due(height) {
            return;
        }
        match self.written(height, prefixes, manifest) {
            Ok(partitions) => {
                self.removals.below(height);
                tracing::info!(
                    target: "ducktape::node",
                    event = "snapshot_taken",
                    height,
                    partitions,
                    "the node keeps a snapshot of its commitments at an epoch's last block"
                );
            }
            Err(Refused::Unsupported(error)) => self.unsupported(&error),
            Err(Refused::Failed(error)) => failed(height, &error.to_string()),
        }
    }

    fn unsupported(&mut self, error: &io::Error) {
        self.able = false;
        self.removals.below(u64::MAX);
        tracing::info!(
            target: "ducktape::node",
            event = "snapshot_unsupported",
            %error,
            "this node's filesystem does not reflink, so the node makes no snapshots"
        );
    }

    fn written(
        &self,
        height: u64,
        prefixes: &[String],
        manifest: &Manifest,
    ) -> Result<usize, Refused> {
        let name = height.to_string();
        let fresh = self.dir.join(format!(".{name}"));
        removed(&fresh)?;
        std::fs::create_dir_all(fresh.join(RUNTIME))?;
        let mut partitions = 0;
        for entry in std::fs::read_dir(&self.runtime)? {
            let entry = entry?;
            let partition = entry.file_name();
            let label = partition.to_string_lossy();
            let committed = prefixes
                .iter()
                .any(|prefix| label.starts_with(prefix.as_str()));
            if !committed {
                continue;
            }
            cloned(
                &entry.path(),
                &fresh.join(RUNTIME).join(&partition),
                self.reflink,
            )?;
            partitions += 1;
        }
        std::fs::write(fresh.join(MANIFEST), abi::encode(manifest))?;
        let whole = self.dir.join(&name);
        removed(&whole)?;
        std::fs::rename(&fresh, &whole)?;
        Ok(partitions)
    }
}

fn removing(dir: &Path, heard: mpsc::Receiver<u64>) {
    for kept in heard {
        older_removed(dir, kept);
    }
}

fn older_removed(dir: &Path, kept: u64) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => return unremoved(dir, &error),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return unremoved(dir, &error),
        };
        let older = height_of(&entry.file_name()).is_none_or(|height| height < kept);
        if !older {
            continue;
        }
        if let Err(error) = removed(&entry.path()) {
            unremoved(&entry.path(), &error);
        }
    }
}

fn height_of(name: &OsStr) -> Option<u64> {
    name.to_str()?.trim_start_matches('.').parse().ok()
}

fn unremoved(path: &Path, error: &io::Error) {
    tracing::warn!(
        target: "ducktape::node",
        event = "snapshot_unremoved",
        path = %path.display(),
        %error,
        reason = "snapshot_unremoved",
        "the node could not remove an older snapshot; the next snapshot tries again"
    );
}

fn failed(height: u64, error: &str) {
    tracing::warn!(
        target: "ducktape::node",
        event = "snapshot_failed",
        height,
        error,
        reason = "snapshot_unwritten",
        "the node could not snapshot its commitments; it keeps the snapshot before"
    );
}

fn cloned(from: &Path, to: &Path, reflink: Reflink) -> Result<(), Refused> {
    let directory = std::fs::symlink_metadata(from)?.is_dir();
    if !directory {
        let source = File::open(from)?;
        let copy = File::create_new(to)?;
        return reflink(&copy, &source);
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        cloned(&entry.path(), &to.join(entry.file_name()), reflink)?;
    }
    Ok(())
}

fn removed(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn reflink(copy: &File, source: &File) -> Result<(), Refused> {
    use rustix::io::Errno;
    let Err(errno) = rustix::fs::ioctl_ficlone(copy, source) else {
        return Ok(());
    };
    let unsupported = matches!(
        errno,
        Errno::OPNOTSUPP | Errno::XDEV | Errno::INVAL | Errno::NOTTY
    );
    let error = io::Error::from(errno);
    match unsupported {
        true => Err(Refused::Unsupported(error)),
        false => Err(Refused::Failed(error)),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn reflink(_: &File, _: &File) -> Result<(), Refused> {
    Err(Refused::Unsupported(io::Error::from(
        io::ErrorKind::Unsupported,
    )))
}

#[cfg(test)]
mod tests;
