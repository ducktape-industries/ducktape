// Epoch snapshots keep commitment files, a manifest and the tip's anchor.
// The writer publishes each whole snapshot; a download holds it until the download ends or two newer
// snapshots have replaced it.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use arc_swap::ArcSwapOption;
use node::Node;
use statesync::{Anchor, Head, Manifest};
use tokio::sync::watch;

use crate::Context;

mod place;
mod stream;

pub(crate) use place::Placing;
pub(crate) use stream::{fetched, resumed, served};

pub(crate) const DIR: &str = "snapshots";
pub(crate) const MANIFEST: &str = "manifest";
pub(crate) const ANCHOR: &str = "anchor";
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

pub(crate) type Served = Arc<ArcSwapOption<Kept>>;

pub(crate) struct Kept {
    height: u64,
    dir: PathBuf,
    head: Head,
    released: mpsc::Sender<Removal>,
    cut: watch::Receiver<u64>,
}

impl Kept {
    fn open(
        snapshots: &Path,
        height: u64,
        removals: &mpsc::Sender<Removal>,
        cut: &watch::Sender<u64>,
    ) -> io::Result<Kept> {
        let dir = snapshots.join(height.to_string());
        let written = std::fs::read(dir.join(MANIFEST))?;
        let manifest: Manifest = abi::decode(&written)
            .map_err(|refusal| io::Error::new(io::ErrorKind::InvalidData, refusal.sentence))?;
        let its_own = manifest.tip.height == height;
        if !its_own {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("its manifest names height {}", manifest.tip.height),
            ));
        }
        let anchor: Anchor = abi::decode(&std::fs::read(dir.join(ANCHOR))?)
            .map_err(|refusal| io::Error::new(io::ErrorKind::InvalidData, refusal.sentence))?;
        removals
            .send(Removal::Kept(height))
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Kept {
            height,
            dir,
            head: Head {
                tip: manifest.tip,
                anchor,
                targets: manifest.targets,
            },
            released: removals.clone(),
            cut: cut.subscribe(),
        })
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        let _ = self.released.send(Removal::Released(self.height));
    }
}

enum Removal {
    Kept(u64),
    Released(u64),
    Cut(u64),
    Unable,
    Stop,
    #[cfg(test)]
    Settled(mpsc::Sender<()>),
}

pub(crate) struct Removals {
    dir: PathBuf,
    sender: mpsc::Sender<Removal>,
    cut: watch::Sender<u64>,
    served: Served,
    thread: Option<JoinHandle<()>>,
}

impl Removals {
    pub(crate) fn start(dir: PathBuf) -> io::Result<Removals> {
        let (sender, heard) = mpsc::channel();
        let (cut, _) = watch::channel(0);
        let adopted = adopted(&dir, &sender, &cut);
        let live: BTreeSet<u64> = adopted.iter().map(|kept| kept.height).collect();
        swept(&dir, &live, true);
        let thread = std::thread::Builder::new()
            .name("snapshots".into())
            .spawn({
                let dir = dir.clone();
                move || removing(&dir, heard)
            })?;
        Ok(Removals {
            dir,
            sender,
            cut,
            served: Arc::new(ArcSwapOption::new(adopted.map(Arc::new))),
            thread: Some(thread),
        })
    }

    pub(crate) fn served(&self) -> Served {
        self.served.clone()
    }

    fn publish(&self, height: u64) -> io::Result<()> {
        let kept = Kept::open(&self.dir, height, &self.sender, &self.cut)?;
        let Some(replaced) = self.served.swap(Some(Arc::new(kept))) else {
            return Ok(());
        };
        let below = replaced.height;
        self.cut.send_if_modified(|cut| {
            let later = below > *cut;
            if later {
                *cut = below;
            }
            later
        });
        self.send(Removal::Cut(below));
        Ok(())
    }

    fn unable(&self) {
        self.served.store(None);
        self.send(Removal::Unable);
    }

    fn send(&self, removal: Removal) {
        if let Err(error) = self.sender.send(removal) {
            unremoved(&self.dir, &io::Error::other(error.to_string()));
        }
    }

    #[cfg(test)]
    fn settled(&self) {
        let (done, settled) = mpsc::channel();
        self.send(Removal::Settled(done));
        let _ = settled.recv();
    }
}

impl Drop for Removals {
    fn drop(&mut self) {
        let _ = self.sender.send(Removal::Stop);
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

    pub(crate) fn take<E: Context>(&mut self, node: &Node<E>, height: u64, anchor: Option<Anchor>) {
        if !self.due(height) {
            return;
        }
        let Some(anchor) = anchor else {
            return failed(height, "the node holds no anchor for its tip");
        };
        let manifest = match statesync::manifest(node) {
            Ok(manifest) => manifest,
            Err(refusal) => return failed(height, &refusal.sentence),
        };
        let head = Head {
            tip: manifest.tip,
            anchor,
            targets: manifest.targets,
        };
        let store = node.host().store();
        let prefixes: Vec<String> = store
            .programs()
            .map(|program| state::partition_prefix(&state::commitment_name(store.name(), program)))
            .collect();
        self.keep(height, &prefixes, &head);
    }

    fn due(&self, height: u64) -> bool {
        let next = height.saturating_add(self.epoch_length);
        self.able && *self.finalized.borrow() < next
    }

    pub(crate) fn keep(&mut self, height: u64, prefixes: &[String], head: &Head) {
        if !self.due(height) {
            return;
        }
        match self.written(height, prefixes, head) {
            Ok(partitions) => self.published(height, partitions),
            Err(Refused::Unsupported(error)) => self.unsupported(&error),
            Err(Refused::Failed(error)) => failed(height, &error.to_string()),
        }
    }

    fn published(&self, height: u64, partitions: usize) {
        if let Err(error) = self.removals.publish(height) {
            return failed(height, &error.to_string());
        }
        tracing::info!(
            target: "ducktape::node",
            event = "snapshot_taken",
            height,
            partitions,
            "the node keeps and serves a snapshot of its commitments at an epoch's last block"
        );
    }

    fn unsupported(&mut self, error: &io::Error) {
        self.able = false;
        self.removals.unable();
        tracing::info!(
            target: "ducktape::node",
            event = "snapshot_unsupported",
            %error,
            "this node's filesystem does not reflink, so the node makes no snapshots"
        );
    }

    fn written(&self, height: u64, prefixes: &[String], head: &Head) -> Result<usize, Refused> {
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
        std::fs::write(fresh.join(MANIFEST), abi::encode(&head.manifest()))?;
        std::fs::write(fresh.join(ANCHOR), abi::encode(&head.anchor))?;
        let whole = self.dir.join(&name);
        removed(&whole)?;
        std::fs::rename(&fresh, &whole)?;
        Ok(partitions)
    }
}

fn adopted(dir: &Path, removals: &mpsc::Sender<Removal>, cut: &watch::Sender<u64>) -> Option<Kept> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut heights: Vec<u64> = entries
        .filter_map(|entry| whole(&entry.ok()?.file_name()))
        .collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights
        .into_iter()
        .find_map(|height| match Kept::open(dir, height, removals, cut) {
            Ok(kept) => Some(kept),
            Err(error) => {
                unserved(height, &error);
                None
            }
        })
}

fn whole(name: &OsStr) -> Option<u64> {
    name.to_str()?.parse().ok()
}

fn removing(dir: &Path, heard: mpsc::Receiver<Removal>) {
    let mut live = BTreeSet::new();
    let mut all = false;
    for removal in heard {
        match removal {
            Removal::Kept(height) => {
                live.insert(height);
            }
            Removal::Released(height) => {
                live.remove(&height);
            }
            Removal::Cut(below) => live.retain(|height| *height >= below),
            Removal::Unable => all = true,
            Removal::Stop => return,
            #[cfg(test)]
            Removal::Settled(done) => {
                let _ = done.send(());
                continue;
            }
        }
        swept(dir, &live, all);
    }
}

fn swept(dir: &Path, live: &BTreeSet<u64>, all: bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => return unremoved(dir, &error),
    };
    let newest = live.last().copied();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return unremoved(dir, &error),
        };
        if !unused(&entry.file_name(), live, newest, all) {
            continue;
        }
        if let Err(error) = removed(&entry.path()) {
            unremoved(&entry.path(), &error);
        }
    }
}

fn unused(name: &OsStr, live: &BTreeSet<u64>, newest: Option<u64>, all: bool) -> bool {
    let Some(name) = name.to_str() else {
        return true;
    };
    let (fresh, digits) = match name.strip_prefix('.') {
        Some(digits) => (true, digits),
        None => (false, name),
    };
    let Ok(height) = digits.parse::<u64>() else {
        return true;
    };
    let serving = !fresh && live.contains(&height);
    if serving {
        return false;
    }
    if all {
        return true;
    }
    newest.is_some_and(|newest| height < newest || (fresh && height == newest))
}

fn unremoved(path: &Path, error: &io::Error) {
    tracing::warn!(
        target: "ducktape::node",
        event = "snapshot_unremoved",
        path = %path.display(),
        %error,
        reason = "snapshot_unremoved",
        "the node could not remove a snapshot nothing serves; the next removal tries again"
    );
}

fn unserved(height: u64, error: &io::Error) {
    tracing::warn!(
        target: "ducktape::node",
        event = "snapshot_unserved",
        height,
        %error,
        reason = "snapshot_unread",
        "the node does not serve a snapshot whose manifest does not read as its own, and removes it"
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
